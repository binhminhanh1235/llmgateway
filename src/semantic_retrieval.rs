use crate::{
    config::ContextConfig,
    conversation::{StoredMessage, ThreadDetail},
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, Serialize)]
pub struct RetrievedChunk {
    pub start_ordinal: i64,
    pub end_ordinal: i64,
    pub score: f64,
    pub text: String,
}

pub trait RetrievalScorer: Send + Sync {
    fn score(&self, query: &str, document: &str) -> f64;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LexicalScorer;

impl RetrievalScorer for LexicalScorer {
    fn score(&self, query: &str, document: &str) -> f64 {
        let query_terms = terms(query);
        if query_terms.is_empty() {
            return 0.0;
        }
        let document_terms = terms(document);
        if document_terms.is_empty() {
            return 0.0;
        }
        let frequencies = document_terms.into_iter().fold(HashMap::new(), |mut map, term| {
            *map.entry(term).or_insert(0usize) += 1;
            map
        });
        let matched = query_terms
            .iter()
            .filter(|term| frequencies.contains_key(*term))
            .count() as f64;
        let coverage = matched / query_terms.len() as f64;
        let frequency_bonus = query_terms
            .iter()
            .filter_map(|term| frequencies.get(term))
            .map(|count| (*count).min(3) as f64)
            .sum::<f64>()
            / (query_terms.len() as f64 * 3.0);
        coverage * 0.8 + frequency_bonus * 0.2
    }
}

pub fn retrieve(
    detail: &ThreadDetail,
    current_message: &Value,
    through_ordinal: i64,
    config: &ContextConfig,
) -> Vec<RetrievedChunk> {
    if !config.retrieval_enabled || through_ordinal <= 0 || config.retrieval_top_k == 0 {
        return Vec::new();
    }
    let query = content_text(current_message);
    if query.trim().is_empty() {
        return Vec::new();
    }
    let scorer = LexicalScorer;
    let chunks = historical_chunks(&detail.messages, through_ordinal);
    let max_ordinal = through_ordinal.max(1) as f64;
    let mut scored = chunks
        .into_iter()
        .filter_map(|mut chunk| {
            let relevance = scorer.score(&query, &chunk.text);
            if relevance < config.retrieval_min_score {
                return None;
            }
            let recency = chunk.end_ordinal.max(0) as f64 / max_ordinal;
            chunk.score = relevance * 0.9 + recency * 0.1;
            Some(chunk)
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.end_ordinal.cmp(&left.end_ordinal))
    });
    scored.truncate(config.retrieval_top_k);
    scored.sort_by_key(|chunk| chunk.start_ordinal);
    trim_chunks_to_token_budget(scored, config.retrieval_max_tokens)
}

pub fn inject_retrieval(
    messages: Vec<Value>,
    chunks: &[RetrievedChunk],
    budget_tokens: usize,
) -> (Vec<Value>, usize) {
    if chunks.is_empty() || messages.is_empty() {
        return (messages, 0);
    }
    let used = estimate_messages_tokens(&messages);
    let slack = budget_tokens.saturating_sub(used);
    if slack < 32 {
        return (messages, 0);
    }
    let mut selected = Vec::new();
    let mut selected_tokens = 0usize;
    for chunk in chunks.iter().rev() {
        let rendered = format!(
            "Historical excerpt, messages {}-{} (retrieval score {:.3}):\n{}",
            chunk.start_ordinal, chunk.end_ordinal, chunk.score, chunk.text
        );
        let tokens = estimate_text_tokens(&rendered).saturating_add(8);
        if selected_tokens.saturating_add(tokens) > slack {
            continue;
        }
        selected.push(rendered);
        selected_tokens = selected_tokens.saturating_add(tokens);
    }
    if selected.is_empty() {
        return (messages, 0);
    }
    selected.reverse();
    let retrieval_message = json!({
        "role":"system",
        "content": format!(
            "llmgateway retrieved historical context. These are quoted older excerpts selected for relevance. Use them only when relevant; recent conversation and explicit current instructions take precedence.\n\n{}",
            selected.join("\n\n---\n\n")
        )
    });
    let mut output = messages;
    let insert_at = output.len().saturating_sub(1);
    output.insert(insert_at, retrieval_message);
    (output, selected.len())
}

fn historical_chunks(messages: &[StoredMessage], through_ordinal: i64) -> Vec<RetrievedChunk> {
    let eligible = messages
        .iter()
        .filter(|message| message.ordinal <= through_ordinal)
        .collect::<Vec<_>>();
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    for message in eligible {
        let role = message
            .message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or(&message.role);
        if role == "user" && !current.is_empty() {
            if let Some(chunk) = build_chunk(&current) {
                chunks.push(chunk);
            }
            current.clear();
        }
        current.push(message);
    }
    if let Some(chunk) = build_chunk(&current) {
        chunks.push(chunk);
    }
    chunks
}

fn build_chunk(messages: &[&StoredMessage]) -> Option<RetrievedChunk> {
    let first = messages.first()?;
    let last = messages.last()?;
    let text = messages
        .iter()
        .filter_map(|message| {
            let role = message
                .message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or(&message.role);
            let content = content_text(&message.message);
            (!content.trim().is_empty()).then(|| format!("{role}: {content}"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(RetrievedChunk {
        start_ordinal: first.ordinal,
        end_ordinal: last.ordinal,
        score: 0.0,
        text,
    })
}

fn trim_chunks_to_token_budget(
    chunks: Vec<RetrievedChunk>,
    max_tokens: usize,
) -> Vec<RetrievedChunk> {
    let mut output = Vec::new();
    let mut used = 0usize;
    for chunk in chunks.into_iter().rev() {
        let tokens = estimate_text_tokens(&chunk.text).saturating_add(16);
        if used.saturating_add(tokens) > max_tokens {
            continue;
        }
        used = used.saturating_add(tokens);
        output.push(chunk);
    }
    output.reverse();
    output
}

fn content_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join(" "),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn terms(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|character: char| !character.is_alphanumeric() && character != '_' && character != '-')
        .filter(|term| term.len() >= 2)
        .filter(|term| !STOP_WORDS.contains(term))
        .map(str::to_string)
        .collect()
}

fn estimate_messages_tokens(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(|message| estimate_text_tokens(&message.to_string()).saturating_add(4))
        .sum::<usize>()
        .saturating_add(messages.len() * 4)
}

fn estimate_text_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4).max(1)
}

const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "this", "that", "from", "into", "your", "you", "are",
    "was", "were", "have", "has", "had", "but", "not", "can", "could", "should", "would",
    "một", "những", "các", "cho", "với", "của", "là", "và", "trong", "được", "này", "đó",
];

#[cfg(test)]
mod tests {
    use super::{inject_retrieval, retrieve, LexicalScorer, RetrievalScorer};
    use crate::{
        config::ContextConfig,
        conversation::{StoredMessage, ThreadDetail, ThreadRecord},
    };
    use serde_json::{json, Value};

    fn stored(ordinal: i64, role: &str, content: &str) -> StoredMessage {
        StoredMessage {
            id: format!("msg_{ordinal}"),
            role: role.to_string(),
            message: json!({"role":role,"content":content}),
            model: None,
            route_id: None,
            ordinal,
            created_at: "2026-09-03 00:00:00".into(),
        }
    }

    fn detail(messages: Vec<StoredMessage>) -> ThreadDetail {
        ThreadDetail {
            id: "thread_test".into(),
            title: "test".into(),
            model: "llmgateway-auto".into(),
            sticky_route: None,
            created_at: "2026-09-03 00:00:00".into(),
            updated_at: "2026-09-03 00:00:00".into(),
            messages,
        }
    }

    #[test]
    fn lexical_scorer_rewards_matching_terms() {
        let scorer = LexicalScorer;
        assert!(
            scorer.score("Kafka zero copy", "Kafka uses zero copy with sendfile")
                > scorer.score("Kafka zero copy", "Java garbage collector tuning")
        );
    }

    #[test]
    fn retrieval_selects_relevant_old_turn() {
        let mut config = ContextConfig::default();
        config.retrieval_enabled = true;
        config.retrieval_top_k = 2;
        config.retrieval_max_tokens = 300;
        config.retrieval_min_score = 0.05;
        let thread = detail(vec![
            stored(1, "user", "We chose optimistic locking for payment updates"),
            stored(2, "assistant", "Use a version column and retry conflicts"),
            stored(3, "user", "Kafka zero copy uses sendfile"),
            stored(4, "assistant", "Correct, it reduces user-space copying"),
        ]);
        let results = retrieve(
            &thread,
            &json!({"role":"user","content":"Why did we choose optimistic locking?"}),
            4,
            &config,
        );
        assert!(!results.is_empty());
        assert!(results[0].text.contains("optimistic locking"));
    }

    #[test]
    fn injection_keeps_current_user_last() {
        let messages = vec![
            json!({"role":"system","content":"memory"}),
            json!({"role":"assistant","content":"recent"}),
            json!({"role":"user","content":"current"}),
        ];
        let chunks = vec![super::RetrievedChunk {
            start_ordinal: 1,
            end_ordinal: 2,
            score: 0.9,
            text: "user: old decision".into(),
        }];
        let (output, count) = inject_retrieval(messages, &chunks, 500);
        assert_eq!(count, 1);
        assert_eq!(
            output.last().and_then(|value| value.get("content")).and_then(Value::as_str),
            Some("current")
        );
    }
}
