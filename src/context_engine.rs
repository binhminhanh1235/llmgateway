use crate::{
    catalog::ModelCatalog,
    config::AppConfig,
    conversation::{ConversationError, ConversationStore, ThreadDetail},
    gateway::{Gateway, GatewayError},
};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{sqlite::{SqliteConnectOptions, SqlitePoolOptions}, Row, SqlitePool};
use std::{path::Path, str::FromStr, sync::Arc};
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone)]
pub struct ContextEngine {
    config: Arc<AppConfig>,
    conversations: Arc<ConversationStore>,
    catalog: Arc<ModelCatalog>,
    gateway: Arc<Gateway>,
    pool: SqlitePool,
}

#[derive(Debug, Error)]
pub enum ContextError {
    #[error(transparent)]
    Conversation(#[from] ConversationError),
    #[error(transparent)]
    Gateway(#[from] GatewayError),
    #[error("context database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("context storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid context JSON: {0}")]
    InvalidJson(String),
    #[error("summary model returned no text")]
    EmptySummary,
}

#[derive(Clone, Debug, Serialize)]
pub struct ContextCheckpoint {
    pub id: String,
    pub thread_id: String,
    pub through_ordinal: i64,
    pub summary: String,
    pub summary_model: String,
    pub route_id: Option<String>,
    pub source_tokens: i64,
    pub summary_tokens: i64,
    pub created_at: String,
}

#[derive(Clone, Debug)]
pub struct PreparedContext {
    pub messages: Vec<Value>,
    pub estimated_source_tokens: usize,
    pub estimated_prepared_tokens: usize,
    pub budget_tokens: usize,
    pub compressed: bool,
    pub checkpoint: Option<ContextCheckpoint>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ContextStatus {
    pub thread_id: String,
    pub state: String,
    pub source_tokens: usize,
    pub prepared_tokens: usize,
    pub budget_tokens: usize,
    pub trigger_tokens: usize,
    pub checkpoint: Option<ContextCheckpoint>,
}

impl ContextEngine {
    pub async fn connect(
        config: Arc<AppConfig>,
        conversations: Arc<ConversationStore>,
        catalog: Arc<ModelCatalog>,
        gateway: Arc<Gateway>,
    ) -> Result<Self, ContextError> {
        ensure_sqlite_parent(&config.storage.database_url)?;
        let options = SqliteConnectOptions::from_str(&config.storage.database_url)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        let engine = Self {
            config,
            conversations,
            catalog,
            gateway,
            pool,
        };
        engine.migrate().await?;
        Ok(engine)
    }

    async fn migrate(&self) -> Result<(), ContextError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS context_checkpoints (
                id TEXT PRIMARY KEY,
                thread_id TEXT NOT NULL,
                through_ordinal INTEGER NOT NULL,
                summary TEXT NOT NULL,
                summary_model TEXT NOT NULL,
                route_id TEXT,
                source_tokens INTEGER NOT NULL,
                summary_tokens INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(thread_id, through_ordinal),
                FOREIGN KEY(thread_id) REFERENCES threads(id) ON DELETE CASCADE
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_context_checkpoints_thread
             ON context_checkpoints(thread_id, through_ordinal DESC)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn prepare(
        &self,
        thread_id: &str,
        requested_model: &str,
    ) -> Result<PreparedContext, ContextError> {
        let detail = self.conversations.thread(thread_id).await?;
        let budget = self.budget_for_model(requested_model).await;
        let source_messages = detail
            .messages
            .iter()
            .map(|message| message.message.clone())
            .collect::<Vec<_>>();
        let source_tokens = estimate_messages_tokens(&source_messages);
        let trigger = self.trigger_tokens(budget);
        let mut checkpoint = self.latest_checkpoint(thread_id).await?;

        if self.config.context.enabled
            && source_tokens >= trigger
            && self.can_compact(&detail, checkpoint.as_ref())
        {
            checkpoint = self
                .compact_detail(&detail, requested_model, checkpoint, false)
                .await?;
        }

        let mut prepared = self.messages_from_checkpoint(&detail, checkpoint.as_ref());
        prepared = fit_messages_to_budget(prepared, budget);
        let prepared_tokens = estimate_messages_tokens(&prepared);

        Ok(PreparedContext {
            messages: prepared,
            estimated_source_tokens: source_tokens,
            estimated_prepared_tokens: prepared_tokens,
            budget_tokens: budget,
            compressed: checkpoint.is_some(),
            checkpoint,
        })
    }

    pub async fn status(
        &self,
        thread_id: &str,
        requested_model: Option<&str>,
    ) -> Result<ContextStatus, ContextError> {
        let detail = self.conversations.thread(thread_id).await?;
        let model = requested_model.unwrap_or(&detail.model);
        let budget = self.budget_for_model(model).await;
        let trigger = self.trigger_tokens(budget);
        let checkpoint = self.latest_checkpoint(thread_id).await?;
        let source_messages = detail
            .messages
            .iter()
            .map(|message| message.message.clone())
            .collect::<Vec<_>>();
        let source_tokens = estimate_messages_tokens(&source_messages);
        let prepared = fit_messages_to_budget(
            self.messages_from_checkpoint(&detail, checkpoint.as_ref()),
            budget,
        );
        let prepared_tokens = estimate_messages_tokens(&prepared);
        let state = if checkpoint.is_some() {
            "compressed"
        } else if source_tokens >= trigger {
            "needs_compaction"
        } else {
            "full"
        };

        Ok(ContextStatus {
            thread_id: thread_id.to_string(),
            state: state.to_string(),
            source_tokens,
            prepared_tokens,
            budget_tokens: budget,
            trigger_tokens: trigger,
            checkpoint,
        })
    }

    pub async fn compact(
        &self,
        thread_id: &str,
        requested_model: Option<&str>,
    ) -> Result<ContextStatus, ContextError> {
        let detail = self.conversations.thread(thread_id).await?;
        let model = requested_model.unwrap_or(&detail.model);
        let checkpoint = self.latest_checkpoint(thread_id).await?;
        let _ = self
            .compact_detail(&detail, model, checkpoint, true)
            .await?;
        self.status(thread_id, Some(model)).await
    }

    async fn compact_detail(
        &self,
        detail: &ThreadDetail,
        requested_model: &str,
        mut checkpoint: Option<ContextCheckpoint>,
        force: bool,
    ) -> Result<Option<ContextCheckpoint>, ContextError> {
        let keep_recent = self.config.context.recent_messages.max(1);
        if detail.messages.len() <= keep_recent {
            return Ok(checkpoint);
        }
        let cutoff_index = detail.messages.len() - keep_recent;
        let cutoff_ordinal = detail.messages[cutoff_index - 1].ordinal;
        if checkpoint
            .as_ref()
            .is_some_and(|existing| existing.through_ordinal >= cutoff_ordinal)
        {
            return Ok(checkpoint);
        }

        let previous_through = checkpoint
            .as_ref()
            .map(|existing| existing.through_ordinal)
            .unwrap_or(0);
        let pending = detail
            .messages
            .iter()
            .filter(|message| {
                message.ordinal > previous_through && message.ordinal <= cutoff_ordinal
            })
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return Ok(checkpoint);
        }

        if !force {
            let pending_values = pending
                .iter()
                .map(|message| message.message.clone())
                .collect::<Vec<_>>();
            if estimate_messages_tokens(&pending_values)
                < self.config.context.min_checkpoint_tokens
            {
                return Ok(checkpoint);
            }
        }

        let chunks = chunk_messages(
            &pending,
            self.config.context.summary_input_tokens.max(512),
        );
        for chunk in chunks {
            let values = chunk
                .iter()
                .map(|message| message.message.clone())
                .collect::<Vec<_>>();
            let previous_summary = checkpoint.as_ref().map(|existing| existing.summary.as_str());
            let summary = self
                .summarize(
                    requested_model,
                    detail.sticky_route.as_deref(),
                    previous_summary,
                    &values,
                )
                .await?;
            let through_ordinal = chunk
                .last()
                .map(|message| message.ordinal)
                .unwrap_or(previous_through);
            let source_tokens = estimate_messages_tokens(&values) as i64;
            let summary_tokens = estimate_text_tokens(&summary) as i64;
            let summary_model = self
                .config
                .context
                .summary_model
                .as_deref()
                .unwrap_or(requested_model)
                .to_string();
            let saved = self
                .save_checkpoint(
                    &detail.id,
                    through_ordinal,
                    &summary,
                    &summary_model,
                    None,
                    source_tokens,
                    summary_tokens,
                )
                .await?;
            checkpoint = Some(saved);
        }
        Ok(checkpoint)
    }

    async fn summarize(
        &self,
        requested_model: &str,
        sticky_route: Option<&str>,
        previous_summary: Option<&str>,
        messages: &[Value],
    ) -> Result<String, ContextError> {
        let summary_model = self
            .config
            .context
            .summary_model
            .as_deref()
            .unwrap_or(requested_model);
        let transcript = serde_json::to_string_pretty(messages)
            .map_err(|error| ContextError::InvalidJson(error.to_string()))?;
        let previous = previous_summary.unwrap_or("(none yet)");
        let prompt = format!(
            "Update the durable conversation memory below. Preserve concrete facts, decisions, constraints, user preferences, code/API names, unresolved questions, and important reasoning outcomes. Remove repetition and conversational filler. Do not invent details. Keep it concise and useful to a different model continuing the same conversation.\n\nPREVIOUS MEMORY:\n{previous}\n\nNEW TRANSCRIPT SEGMENT:\n{transcript}\n\nReturn only the updated memory in plain text with short sections when useful."
        );
        let request = json!({
            "model": summary_model,
            "stream": false,
            "temperature": 0.1,
            "max_tokens": self.config.context.summary_max_tokens,
            "messages": [
                {"role":"system","content":"You are llmgateway's loss-aware conversation memory compressor."},
                {"role":"user","content":prompt}
            ]
        });
        let routed = self
            .gateway
            .execute_openai_chat_with_affinity(summary_model, &request, sticky_route)
            .await?;
        let route_id = routed.route.id.clone();
        let payload = routed
            .response
            .json::<Value>()
            .await
            .map_err(|error| GatewayError::Transport(error.to_string()))?;
        let text = payload
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or(ContextError::EmptySummary)?;
        let _ = route_id;
        Ok(text.to_string())
    }

    fn can_compact(
        &self,
        detail: &ThreadDetail,
        checkpoint: Option<&ContextCheckpoint>,
    ) -> bool {
        let keep_recent = self.config.context.recent_messages.max(1);
        if detail.messages.len() <= keep_recent {
            return false;
        }
        let cutoff_ordinal = detail.messages[detail.messages.len() - keep_recent - 1].ordinal;
        checkpoint
            .map(|existing| existing.through_ordinal < cutoff_ordinal)
            .unwrap_or(true)
    }

    fn messages_from_checkpoint(
        &self,
        detail: &ThreadDetail,
        checkpoint: Option<&ContextCheckpoint>,
    ) -> Vec<Value> {
        let mut messages = Vec::new();
        let through = checkpoint.map(|value| value.through_ordinal).unwrap_or(0);
        if let Some(checkpoint) = checkpoint {
            messages.push(json!({
                "role":"system",
                "content": format!(
                    "llmgateway conversation memory checkpoint. Treat this as compressed context from earlier turns. If a recent message conflicts with it, prefer the recent message.\n\n{}",
                    checkpoint.summary
                )
            }));
        }
        messages.extend(
            detail
                .messages
                .iter()
                .filter(|message| message.ordinal > through)
                .map(|message| message.message.clone()),
        );
        messages
    }

    async fn budget_for_model(&self, requested_model: &str) -> usize {
        let configured = self.config.context.target_tokens.max(1024);
        let reserve = self.config.context.reserve_output_tokens;
        let models = match self.catalog.models().await {
            Ok(models) => models,
            Err(_) => return configured,
        };
        let matched = models.iter().find(|model| {
            model.id == requested_model || model.external_id == requested_model
        });
        let Some(window) = matched.and_then(|model| model.context_window) else {
            return configured;
        };
        let window = usize::try_from(window).unwrap_or(configured);
        configured.min(window.saturating_sub(reserve).max(1024))
    }

    fn trigger_tokens(&self, budget: usize) -> usize {
        ((budget as f64) * self.config.context.compaction_trigger_ratio)
            .round()
            .clamp(1.0, budget as f64) as usize
    }

    async fn latest_checkpoint(
        &self,
        thread_id: &str,
    ) -> Result<Option<ContextCheckpoint>, ContextError> {
        let row = sqlx::query(
            "SELECT id, thread_id, through_ordinal, summary, summary_model, route_id,
                    source_tokens, summary_tokens, created_at
             FROM context_checkpoints
             WHERE thread_id = ?
             ORDER BY through_ordinal DESC
             LIMIT 1",
        )
        .bind(thread_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(checkpoint_from_row).transpose()
    }

    async fn save_checkpoint(
        &self,
        thread_id: &str,
        through_ordinal: i64,
        summary: &str,
        summary_model: &str,
        route_id: Option<&str>,
        source_tokens: i64,
        summary_tokens: i64,
    ) -> Result<ContextCheckpoint, ContextError> {
        let id = format!("ctx_{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO context_checkpoints
             (id, thread_id, through_ordinal, summary, summary_model, route_id, source_tokens, summary_tokens)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(thread_id, through_ordinal) DO UPDATE SET
                summary = excluded.summary,
                summary_model = excluded.summary_model,
                route_id = excluded.route_id,
                source_tokens = excluded.source_tokens,
                summary_tokens = excluded.summary_tokens,
                created_at = CURRENT_TIMESTAMP",
        )
        .bind(&id)
        .bind(thread_id)
        .bind(through_ordinal)
        .bind(summary)
        .bind(summary_model)
        .bind(route_id)
        .bind(source_tokens)
        .bind(summary_tokens)
        .execute(&self.pool)
        .await?;

        let row = sqlx::query(
            "SELECT id, thread_id, through_ordinal, summary, summary_model, route_id,
                    source_tokens, summary_tokens, created_at
             FROM context_checkpoints
             WHERE thread_id = ? AND through_ordinal = ?",
        )
        .bind(thread_id)
        .bind(through_ordinal)
        .fetch_one(&self.pool)
        .await?;
        checkpoint_from_row(row)
    }
}

fn checkpoint_from_row(row: sqlx::sqlite::SqliteRow) -> Result<ContextCheckpoint, ContextError> {
    Ok(ContextCheckpoint {
        id: row.try_get("id")?,
        thread_id: row.try_get("thread_id")?,
        through_ordinal: row.try_get("through_ordinal")?,
        summary: row.try_get("summary")?,
        summary_model: row.try_get("summary_model")?,
        route_id: row.try_get("route_id")?,
        source_tokens: row.try_get("source_tokens")?,
        summary_tokens: row.try_get("summary_tokens")?,
        created_at: row.try_get("created_at")?,
    })
}

fn chunk_messages<'a>(
    messages: &[&'a crate::conversation::StoredMessage],
    max_tokens: usize,
) -> Vec<Vec<&'a crate::conversation::StoredMessage>> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut current_tokens = 0usize;
    for message in messages {
        let tokens = estimate_message_tokens(&message.message).max(1);
        if !current.is_empty() && current_tokens + tokens > max_tokens {
            chunks.push(current);
            current = Vec::new();
            current_tokens = 0;
        }
        current.push(*message);
        current_tokens = current_tokens.saturating_add(tokens);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

pub fn estimate_messages_tokens(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(estimate_message_tokens)
        .sum::<usize>()
        .saturating_add(messages.len() * 4)
}

pub fn estimate_message_tokens(message: &Value) -> usize {
    estimate_text_tokens(&message.to_string()).saturating_add(4)
}

pub fn estimate_text_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    chars.div_ceil(4).max(1)
}

fn fit_messages_to_budget(messages: Vec<Value>, budget: usize) -> Vec<Value> {
    if estimate_messages_tokens(&messages) <= budget {
        return messages;
    }
    let mut prefix = Vec::new();
    let mut start = 0usize;
    if messages
        .first()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        == Some("system")
    {
        prefix.push(messages[0].clone());
        start = 1;
    }
    let mut selected = Vec::new();
    let mut used = estimate_messages_tokens(&prefix);
    for message in messages[start..].iter().rev() {
        let tokens = estimate_message_tokens(message).saturating_add(4);
        if used + tokens > budget && !selected.is_empty() {
            break;
        }
        if used + tokens <= budget {
            selected.push(message.clone());
            used += tokens;
        }
    }
    selected.reverse();
    prefix.extend(selected);
    prefix
}

fn ensure_sqlite_parent(database_url: &str) -> Result<(), std::io::Error> {
    let path = database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"));
    let Some(path) = path else {
        return Ok(());
    };
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
    use super::{estimate_text_tokens, fit_messages_to_budget};
    use serde_json::json;

    #[test]
    fn token_estimator_is_stable_and_non_zero() {
        assert_eq!(estimate_text_tokens("abcd"), 1);
        assert_eq!(estimate_text_tokens("abcde"), 2);
        assert_eq!(estimate_text_tokens(""), 1);
    }

    #[test]
    fn budget_keeps_checkpoint_and_latest_messages() {
        let messages = vec![
            json!({"role":"system","content":"memory"}),
            json!({"role":"user","content":"a".repeat(120)}),
            json!({"role":"assistant","content":"b".repeat(120)}),
            json!({"role":"user","content":"latest"}),
        ];
        let fitted = fit_messages_to_budget(messages, 30);
        assert_eq!(fitted.first().and_then(|m| m.get("role")).and_then(|v| v.as_str()), Some("system"));
        assert_eq!(fitted.last().and_then(|m| m.get("content")).and_then(|v| v.as_str()), Some("latest"));
    }
}