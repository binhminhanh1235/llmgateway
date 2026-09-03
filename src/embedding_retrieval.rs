use crate::{
    config::{AccountConfig, AppConfig},
    conversation::StoredMessage,
    semantic_retrieval::{RetrievedChunk, RetrievalResult},
};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION},
    Client, StatusCode,
};
use serde_json::{json, Value};
use sqlx::{sqlite::{SqliteConnectOptions, SqlitePoolOptions}, Row, SqlitePool};
use std::{
    collections::{HashMap, HashSet},
    env,
    path::Path,
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use thiserror::Error;

#[derive(Clone)]
pub struct EmbeddingRetriever {
    config: Arc<AppConfig>,
    pool: SqlitePool,
    client: Client,
    account_id: String,
    model: String,
}

#[derive(Debug, Error)]
pub enum EmbeddingError {
    #[error("embedding retrieval database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("embedding retrieval storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("embedding retrieval configuration error: {0}")]
    InvalidConfig(String),
    #[error("missing embedding credential environment variable '{0}'")]
    MissingCredential(String),
    #[error("embedding request failed: {0}")]
    Transport(String),
    #[error("embedding upstream returned {status}: {body}")]
    Upstream { status: StatusCode, body: String },
    #[error("invalid embedding response: {0}")]
    InvalidResponse(String),
}

#[derive(Clone, Debug)]
struct HistoricalChunk {
    start_ordinal: i64,
    end_ordinal: i64,
    text: String,
    terms: Vec<String>,
}

impl EmbeddingRetriever {
    pub async fn connect(config: Arc<AppConfig>) -> Result<Option<Self>, EmbeddingError> {
        if !config.context.retrieval_enabled || config.context.retrieval_backend != "hybrid" {
            return Ok(None);
        }
        let account_id = config
            .context
            .retrieval_embedding_account
            .clone()
            .ok_or_else(|| EmbeddingError::InvalidConfig("embedding account is required".into()))?;
        let model = config
            .context
            .retrieval_embedding_model
            .clone()
            .ok_or_else(|| EmbeddingError::InvalidConfig("embedding model is required".into()))?;
        ensure_sqlite_parent(&config.storage.database_url)?;
        let options = SqliteConnectOptions::from_str(&config.storage.database_url)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .build()
            .map_err(|error| EmbeddingError::Transport(error.to_string()))?;
        let retriever = Self {
            config,
            pool,
            client,
            account_id,
            model,
        };
        retriever.migrate().await?;
        Ok(Some(retriever))
    }

    async fn migrate(&self) -> Result<(), EmbeddingError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS retrieval_embeddings (
                thread_id TEXT NOT NULL,
                start_ordinal INTEGER NOT NULL,
                end_ordinal INTEGER NOT NULL,
                embedding_model TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                vector_json TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY(thread_id, start_ordinal, end_ordinal, embedding_model),
                FOREIGN KEY(thread_id) REFERENCES threads(id) ON DELETE CASCADE
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn retrieve(
        &self,
        thread_id: &str,
        messages: &[StoredMessage],
        through_ordinal: i64,
        query_message: &Value,
        max_chunks: usize,
        max_tokens: usize,
        min_score: f64,
    ) -> Result<RetrievalResult, EmbeddingError> {
        if through_ordinal <= 0 || max_chunks == 0 || max_tokens == 0 {
            return Ok(empty_result());
        }
        let query = message_text(query_message);
        if query.trim().is_empty() {
            return Ok(empty_result());
        }
        let chunks = conversation_chunks(messages, through_ordinal);
        if chunks.is_empty() {
            return Ok(empty_result());
        }

        let query_vector = self
            .embed_texts(&[query.clone()])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| EmbeddingError::InvalidResponse("query embedding was missing".into()))?;

        let mut vectors: Vec<Option<Vec<f64>>> = vec![None; chunks.len()];
        let mut missing_indices = Vec::new();
        let mut missing_texts = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            if let Some(vector) = self.cached_vector(thread_id, chunk).await? {
                vectors[index] = Some(vector);
            } else {
                missing_indices.push(index);
                missing_texts.push(chunk.text.clone());
            }
        }

        if !missing_texts.is_empty() {
            let embedded = self.embed_texts(&missing_texts).await?;
            if embedded.len() != missing_indices.len() {
                return Err(EmbeddingError::InvalidResponse(format!(
                    "expected {} chunk embeddings, received {}",
                    missing_indices.len(),
                    embedded.len()
                )));
            }
            for ((index, vector), text) in missing_indices
                .into_iter()
                .zip(embedded.into_iter())
                .zip(missing_texts.into_iter())
            {
                self.save_vector(thread_id, &chunks[index], &text, &vector)
                    .await?;
                vectors[index] = Some(vector);
            }
        }

        let document_frequency = document_frequency(&chunks);
        let document_count = chunks.len() as f64;
        let query_terms = tokenize(&query);
        let semantic_weight = self.config.context.retrieval_semantic_weight;
        let min_similarity = self.config.context.retrieval_min_similarity;

        let mut scored = chunks
            .into_iter()
            .zip(vectors.into_iter())
            .filter_map(|(chunk, vector)| {
                let vector = vector?;
                let similarity = cosine_similarity(&query_vector, &vector)?;
                if similarity < min_similarity {
                    return None;
                }
                let lexical = normalized_lexical_score(
                    &chunk,
                    &query_terms,
                    &document_frequency,
                    document_count,
                );
                let semantic = ((similarity + 1.0) / 2.0).clamp(0.0, 1.0);
                let combined = semantic_weight * semantic + (1.0 - semantic_weight) * lexical;
                (combined >= min_score).then_some((chunk, combined, similarity, lexical))
            })
            .collect::<Vec<_>>();

        scored.sort_by(|(a, score_a, _, _), (b, score_b, _, _)| {
            score_b
                .partial_cmp(score_a)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.end_ordinal.cmp(&a.end_ordinal))
        });

        let mut selected = Vec::new();
        let mut sections = Vec::new();
        let mut used_tokens = 0usize;
        for (chunk, score, similarity, lexical) in scored.into_iter() {
            let section = format!(
                "Earlier transcript ordinals {}-{} [hybrid={:.3}, semantic={:.3}, lexical={:.3}]:\n{}",
                chunk.start_ordinal,
                chunk.end_ordinal,
                score,
                similarity,
                lexical,
                chunk.text
            );
            let tokens = estimate_text_tokens(&section);
            if !selected.is_empty() && used_tokens.saturating_add(tokens) > max_tokens {
                continue;
            }
            let rendered = if selected.is_empty() && tokens > max_tokens {
                truncate_to_token_budget(&section, max_tokens)
            } else {
                section
            };
            used_tokens = used_tokens.saturating_add(estimate_text_tokens(&rendered));
            selected.push(RetrievedChunk {
                start_ordinal: chunk.start_ordinal,
                end_ordinal: chunk.end_ordinal,
                score: (score * 1000.0).round() / 1000.0,
                preview: preview(&chunk.text, 180),
            });
            sections.push(rendered);
            if selected.len() >= max_chunks || used_tokens >= max_tokens {
                break;
            }
        }

        Ok(RetrievalResult {
            chunks: selected,
            rendered: sections.join("\n\n"),
            estimated_tokens: used_tokens,
        })
    }

    async fn cached_vector(
        &self,
        thread_id: &str,
        chunk: &HistoricalChunk,
    ) -> Result<Option<Vec<f64>>, EmbeddingError> {
        let content_hash = stable_hash(&chunk.text);
        let row = sqlx::query(
            "SELECT content_hash, vector_json
             FROM retrieval_embeddings
             WHERE thread_id = ? AND start_ordinal = ? AND end_ordinal = ? AND embedding_model = ?",
        )
        .bind(thread_id)
        .bind(chunk.start_ordinal)
        .bind(chunk.end_ordinal)
        .bind(&self.model)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let saved_hash: String = row.try_get("content_hash")?;
        if saved_hash != content_hash {
            return Ok(None);
        }
        let raw: String = row.try_get("vector_json")?;
        let vector = serde_json::from_str::<Vec<f64>>(&raw)
            .map_err(|error| EmbeddingError::InvalidResponse(error.to_string()))?;
        Ok((!vector.is_empty()).then_some(vector))
    }

    async fn save_vector(
        &self,
        thread_id: &str,
        chunk: &HistoricalChunk,
        text: &str,
        vector: &[f64],
    ) -> Result<(), EmbeddingError> {
        let vector_json = serde_json::to_string(vector)
            .map_err(|error| EmbeddingError::InvalidResponse(error.to_string()))?;
        sqlx::query(
            "INSERT INTO retrieval_embeddings
             (thread_id, start_ordinal, end_ordinal, embedding_model, content_hash, vector_json)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(thread_id, start_ordinal, end_ordinal, embedding_model) DO UPDATE SET
                content_hash = excluded.content_hash,
                vector_json = excluded.vector_json,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(thread_id)
        .bind(chunk.start_ordinal)
        .bind(chunk.end_ordinal)
        .bind(&self.model)
        .bind(stable_hash(text))
        .bind(vector_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f64>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let account = self
            .config
            .account(&self.account_id)
            .ok_or_else(|| EmbeddingError::InvalidConfig(format!("unknown account '{}'", self.account_id)))?;
        let provider = self
            .config
            .provider(&account.provider)
            .ok_or_else(|| EmbeddingError::InvalidConfig(format!("unknown provider '{}'", account.provider)))?;
        let key = env::var(&account.api_key_env)
            .map_err(|_| EmbeddingError::MissingCredential(account.api_key_env.clone()))?;
        let url = format!("{}/embeddings", provider.base_url.trim_end_matches('/'));
        let batch_size = self.config.context.retrieval_embedding_batch_size.max(1);
        let mut all = Vec::with_capacity(texts.len());

        for batch in texts.chunks(batch_size) {
            let mut headers = HeaderMap::new();
            apply_auth(&mut headers, account, &key)?;
            let response = self
                .client
                .post(&url)
                .headers(headers)
                .json(&json!({"model":self.model,"input":batch}))
                .send()
                .await
                .map_err(|error| EmbeddingError::Transport(error.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(EmbeddingError::Upstream { status, body });
            }
            let payload = response
                .json::<Value>()
                .await
                .map_err(|error| EmbeddingError::InvalidResponse(error.to_string()))?;
            let data = payload
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| EmbeddingError::InvalidResponse("expected embeddings data array".into()))?;
            let mut ordered = data
                .iter()
                .enumerate()
                .map(|(fallback_index, item)| {
                    let index = item
                        .get("index")
                        .and_then(Value::as_u64)
                        .map(|value| value as usize)
                        .unwrap_or(fallback_index);
                    let vector = item
                        .get("embedding")
                        .and_then(Value::as_array)
                        .ok_or_else(|| EmbeddingError::InvalidResponse("embedding vector missing".into()))?
                        .iter()
                        .map(|value| {
                            value.as_f64().ok_or_else(|| {
                                EmbeddingError::InvalidResponse("embedding contained non-number".into())
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok::<_, EmbeddingError>((index, vector))
                })
                .collect::<Result<Vec<_>, _>>()?;
            ordered.sort_by_key(|(index, _)| *index);
            if ordered.len() != batch.len() {
                return Err(EmbeddingError::InvalidResponse(format!(
                    "expected {} embeddings, received {}",
                    batch.len(),
                    ordered.len()
                )));
            }
            all.extend(ordered.into_iter().map(|(_, vector)| vector));
        }
        Ok(all)
    }
}

fn empty_result() -> RetrievalResult {
    RetrievalResult {
        chunks: Vec::new(),
        rendered: String::new(),
        estimated_tokens: 0,
    }
}

fn conversation_chunks(messages: &[StoredMessage], through_ordinal: i64) -> Vec<HistoricalChunk> {
    let mut chunks = Vec::new();
    let mut current: Vec<&StoredMessage> = Vec::new();
    for message in messages.iter().filter(|message| message.ordinal <= through_ordinal) {
        let role = message
            .message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or(&message.role);
        if role == "user" && !current.is_empty() {
            push_chunk(&mut chunks, &current);
            current.clear();
        }
        current.push(message);
    }
    if !current.is_empty() {
        push_chunk(&mut chunks, &current);
    }
    chunks
}

fn push_chunk(chunks: &mut Vec<HistoricalChunk>, messages: &[&StoredMessage]) {
    let Some(first) = messages.first() else { return; };
    let Some(last) = messages.last() else { return; };
    let text = messages
        .iter()
        .map(|message| {
            let role = message
                .message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or(&message.role);
            format!("{role}: {}", message_text(&message.message))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let terms = tokenize(&text);
    if text.trim().is_empty() || terms.is_empty() {
        return;
    }
    chunks.push(HistoricalChunk {
        start_ordinal: first.ordinal,
        end_ordinal: last.ordinal,
        text,
        terms,
    });
}

fn normalized_lexical_score(
    chunk: &HistoricalChunk,
    query_terms: &[String],
    document_frequency: &HashMap<String, usize>,
    document_count: f64,
) -> f64 {
    if query_terms.is_empty() {
        return 0.0;
    }
    let query_unique = query_terms.iter().collect::<HashSet<_>>();
    let chunk_unique = chunk.terms.iter().collect::<HashSet<_>>();
    let mut weighted_overlap = 0.0;
    let mut possible = 0.0;
    for term in query_unique {
        let df = *document_frequency.get(term).unwrap_or(&1) as f64;
        let weight = ((document_count + 1.0) / (df + 0.5)).ln().max(0.15);
        possible += weight;
        if chunk_unique.contains(term) {
            weighted_overlap += weight;
        }
    }
    if possible <= f64::EPSILON {
        0.0
    } else {
        (weighted_overlap / possible).clamp(0.0, 1.0)
    }
}

fn document_frequency(chunks: &[HistoricalChunk]) -> HashMap<String, usize> {
    let mut result = HashMap::new();
    for chunk in chunks {
        for term in chunk.terms.iter().cloned().collect::<HashSet<_>>() {
            *result.entry(term).or_insert(0) += 1;
        }
    }
    result
}

fn cosine_similarity(a: &[f64], b: &[f64]) -> Option<f64> {
    if a.is_empty() || a.len() != b.len() {
        return None;
    }
    let dot = a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>();
    let norm_a = a.iter().map(|value| value * value).sum::<f64>().sqrt();
    let norm_b = b.iter().map(|value| value * value).sum::<f64>().sqrt();
    if norm_a <= f64::EPSILON || norm_b <= f64::EPSILON {
        return None;
    }
    Some((dot / (norm_a * norm_b)).clamp(-1.0, 1.0))
}

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-' && ch != '.')
        .map(|term| term.trim_matches(|ch: char| ch == '.' || ch == '-'))
        .filter(|term| term.chars().count() >= 2)
        .map(ToString::to_string)
        .collect()
}

fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => message.get("tool_calls").map(Value::to_string).unwrap_or_default(),
    }
}

fn stable_hash(text: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn estimate_text_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4).max(1)
}

fn truncate_to_token_budget(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(4);
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated = text.chars().take(max_chars.saturating_sub(1)).collect::<String>();
    truncated.push('…');
    truncated
}

fn preview(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let mut value = compact.chars().take(max_chars.saturating_sub(1)).collect::<String>();
    value.push('…');
    value
}

fn apply_auth(
    headers: &mut HeaderMap,
    account: &AccountConfig,
    key: &str,
) -> Result<(), EmbeddingError> {
    match account.auth_style.as_str() {
        "bearer" => {
            let value = HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|error| EmbeddingError::InvalidConfig(error.to_string()))?;
            headers.insert(AUTHORIZATION, value);
        }
        "x-api-key" => {
            let value = HeaderValue::from_str(key)
                .map_err(|error| EmbeddingError::InvalidConfig(error.to_string()))?;
            headers.insert(HeaderName::from_static("x-api-key"), value);
        }
        other => {
            return Err(EmbeddingError::InvalidConfig(format!(
                "unsupported auth_style '{other}'"
            )));
        }
    }
    Ok(())
}

fn ensure_sqlite_parent(database_url: &str) -> Result<(), std::io::Error> {
    let Some(path) = database_url.strip_prefix("sqlite://") else {
        return Ok(());
    };
    let path = path.split('?').next().unwrap_or(path);
    if path == ":memory:" || path.is_empty() {
        return Ok(());
    }
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{cosine_similarity, stable_hash};

    #[test]
    fn cosine_similarity_handles_parallel_and_orthogonal_vectors() {
        assert!((cosine_similarity(&[1.0, 0.0], &[2.0, 0.0]).unwrap() - 1.0).abs() < 1e-9);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).unwrap().abs() < 1e-9);
    }

    #[test]
    fn stable_hash_is_deterministic() {
        assert_eq!(stable_hash("abc"), stable_hash("abc"));
        assert_ne!(stable_hash("abc"), stable_hash("abd"));
    }
}
