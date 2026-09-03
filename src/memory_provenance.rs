use crate::{config::AppConfig, structured_memory::StructuredMemorySnapshot};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{sqlite::{SqliteConnectOptions, SqlitePoolOptions}, Row, SqlitePool};
use std::{path::Path, str::FromStr, sync::Arc};
use thiserror::Error;

#[derive(Clone)]
pub struct MemoryProvenanceStore {
    pool: SqlitePool,
}

#[derive(Debug, Error)]
pub enum MemoryProvenanceError {
    #[error("memory provenance database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("memory provenance storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid memory category '{0}'")]
    InvalidCategory(String),
    #[error("invalid confidence {0}; expected 0..=1")]
    InvalidConfidence(f64),
    #[error("memory item '{0}' was not found")]
    ItemNotFound(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct MemoryItemMetadata {
    pub thread_id: String,
    pub item_key: String,
    pub category: String,
    pub value: String,
    pub confidence: f64,
    pub pinned: bool,
    pub active: bool,
    pub source_kind: String,
    pub first_seen_ordinal: Option<i64>,
    pub last_seen_ordinal: Option<i64>,
    pub model: Option<String>,
    pub route_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl MemoryProvenanceStore {
    pub async fn connect(config: Arc<AppConfig>) -> Result<Self, MemoryProvenanceError> {
        ensure_sqlite_parent(&config.storage.database_url)?;
        let options = SqliteConnectOptions::from_str(&config.storage.database_url)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new().max_connections(5).connect_with(options).await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<(), MemoryProvenanceError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS memory_items (
                thread_id TEXT NOT NULL,
                item_key TEXT NOT NULL,
                category TEXT NOT NULL,
                value TEXT NOT NULL,
                confidence REAL NOT NULL,
                pinned INTEGER NOT NULL DEFAULT 0,
                active INTEGER NOT NULL DEFAULT 1,
                source_kind TEXT NOT NULL,
                first_seen_ordinal INTEGER,
                last_seen_ordinal INTEGER,
                model TEXT,
                route_id TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY(thread_id, item_key),
                FOREIGN KEY(thread_id) REFERENCES threads(id) ON DELETE CASCADE
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memory_items_thread_active
             ON memory_items(thread_id, active, pinned)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn sync_snapshot(
        &self,
        snapshot: &StructuredMemorySnapshot,
    ) -> Result<Vec<MemoryItemMetadata>, MemoryProvenanceError> {
        sqlx::query(
            "UPDATE memory_items
             SET active = 0, updated_at = CURRENT_TIMESTAMP
             WHERE thread_id = ? AND pinned = 0 AND source_kind = 'checkpoint'",
        )
        .bind(&snapshot.thread_id)
        .execute(&self.pool)
        .await?;

        for (category, value) in snapshot_items(snapshot) {
            let key = memory_item_key(category, value);
            sqlx::query(
                "INSERT INTO memory_items
                 (thread_id, item_key, category, value, confidence, pinned, active, source_kind,
                  first_seen_ordinal, last_seen_ordinal, model, route_id)
                 VALUES (?, ?, ?, ?, 0.65, 0, 1, 'checkpoint', ?, ?, ?, ?)
                 ON CONFLICT(thread_id, item_key) DO UPDATE SET
                    category = excluded.category,
                    value = excluded.value,
                    confidence = CASE
                        WHEN memory_items.pinned = 1 THEN memory_items.confidence
                        WHEN COALESCE(memory_items.last_seen_ordinal, -1) < excluded.last_seen_ordinal
                            THEN MIN(0.95, memory_items.confidence + 0.05)
                        ELSE memory_items.confidence
                    END,
                    active = 1,
                    last_seen_ordinal = MAX(COALESCE(memory_items.last_seen_ordinal, 0), excluded.last_seen_ordinal),
                    model = CASE WHEN memory_items.source_kind = 'manual' THEN memory_items.model ELSE excluded.model END,
                    route_id = CASE WHEN memory_items.source_kind = 'manual' THEN memory_items.route_id ELSE excluded.route_id END,
                    source_kind = CASE WHEN memory_items.source_kind = 'manual' THEN 'manual' ELSE 'checkpoint' END,
                    updated_at = CURRENT_TIMESTAMP",
            )
            .bind(&snapshot.thread_id)
            .bind(key)
            .bind(category)
            .bind(value)
            .bind(snapshot.through_ordinal)
            .bind(snapshot.through_ordinal)
            .bind(&snapshot.model)
            .bind(&snapshot.route_id)
            .execute(&self.pool)
            .await?;
        }
        self.list_items(&snapshot.thread_id).await
    }

    pub async fn list_items(&self, thread_id: &str) -> Result<Vec<MemoryItemMetadata>, MemoryProvenanceError> {
        let rows = sqlx::query(
            "SELECT thread_id, item_key, category, value, confidence, pinned, active,
                    source_kind, first_seen_ordinal, last_seen_ordinal, model, route_id,
                    created_at, updated_at
             FROM memory_items WHERE thread_id = ?
             ORDER BY pinned DESC, active DESC, category ASC, value ASC",
        )
        .bind(thread_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(item_from_row).collect()
    }

    pub async fn add_manual_pin(
        &self,
        thread_id: &str,
        category: &str,
        value: &str,
        confidence: f64,
    ) -> Result<MemoryItemMetadata, MemoryProvenanceError> {
        validate_category(category)?;
        validate_confidence(confidence)?;
        let value = value.trim();
        let key = memory_item_key(category, value);
        sqlx::query(
            "INSERT INTO memory_items
             (thread_id, item_key, category, value, confidence, pinned, active, source_kind, model, route_id)
             VALUES (?, ?, ?, ?, ?, 1, 1, 'manual', 'manual', NULL)
             ON CONFLICT(thread_id, item_key) DO UPDATE SET
                value = excluded.value, category = excluded.category,
                confidence = excluded.confidence, pinned = 1, active = 1,
                source_kind = 'manual', model = 'manual', route_id = NULL,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(thread_id)
        .bind(&key)
        .bind(category)
        .bind(value)
        .bind(confidence)
        .execute(&self.pool)
        .await?;
        self.item(thread_id, &key).await
    }

    pub async fn set_pinned(
        &self,
        thread_id: &str,
        item_key: &str,
        pinned: bool,
    ) -> Result<MemoryItemMetadata, MemoryProvenanceError> {
        let result = sqlx::query(
            "UPDATE memory_items
             SET pinned = ?, active = CASE WHEN ? = 1 THEN 1 ELSE active END,
                 updated_at = CURRENT_TIMESTAMP
             WHERE thread_id = ? AND item_key = ?",
        )
        .bind(pinned)
        .bind(pinned)
        .bind(thread_id)
        .bind(item_key)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(MemoryProvenanceError::ItemNotFound(item_key.to_string()));
        }
        self.item(thread_id, item_key).await
    }

    pub async fn pinned_prompt(&self, thread_id: &str) -> Result<Option<String>, MemoryProvenanceError> {
        let items = self.list_items(thread_id).await?
            .into_iter().filter(|item| item.pinned && item.active).collect::<Vec<_>>();
        if items.is_empty() { return Ok(None); }
        let mut out = String::from(
            "Pinned llmgateway memory. These are durable user-approved items. "
        );
        out.push_str("If checkpoint memory or retrieved history conflicts with a pinned item, the pinned item wins.\n");
        for item in items {
            out.push_str(&format!("- [{}] {} (confidence {:.2}, key {})\n", item.category, item.value, item.confidence, item.item_key));
        }
        Ok(Some(out))
    }

    async fn item(&self, thread_id: &str, item_key: &str) -> Result<MemoryItemMetadata, MemoryProvenanceError> {
        let row = sqlx::query(
            "SELECT thread_id, item_key, category, value, confidence, pinned, active,
                    source_kind, first_seen_ordinal, last_seen_ordinal, model, route_id,
                    created_at, updated_at
             FROM memory_items WHERE thread_id = ? AND item_key = ?",
        )
        .bind(thread_id)
        .bind(item_key)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| MemoryProvenanceError::ItemNotFound(item_key.to_string()))?;
        item_from_row(row)
    }
}

pub fn inject_pinned_memory(messages: &mut Vec<Value>, prompt: &str, budget_tokens: usize) -> usize {
    if prompt.trim().is_empty() { return estimate_messages_tokens(messages); }
    if let Some(first) = messages.first_mut() {
        if first.get("role").and_then(Value::as_str) == Some("system") {
            let existing = first.get("content").and_then(Value::as_str).unwrap_or_default();
            first["content"] = Value::String(format!("{existing}\n\n{prompt}"));
        } else {
            messages.insert(0, json!({"role":"system","content":prompt}));
        }
    } else {
        messages.push(json!({"role":"system","content":prompt}));
    }
    fit_messages_to_budget(messages, budget_tokens);
    estimate_messages_tokens(messages)
}

fn fit_messages_to_budget(messages: &mut Vec<Value>, budget: usize) {
    if estimate_messages_tokens(messages) <= budget { return; }
    let mut prefix = Vec::new();
    let mut start = 0usize;
    if messages.first().and_then(|m| m.get("role")).and_then(Value::as_str) == Some("system") {
        prefix.push(messages[0].clone());
        start = 1;
    }
    let groups = atomic_message_groups(&messages[start..]);
    let mut selected: Vec<Vec<Value>> = Vec::new();
    let mut used = estimate_messages_tokens(&prefix);
    for group in groups.iter().rev() {
        let tokens = estimate_messages_tokens(group);
        if used.saturating_add(tokens) <= budget {
            selected.push(group.clone());
            used = used.saturating_add(tokens);
        } else if selected.is_empty() {
            selected.push(group.clone());
            break;
        } else {
            break;
        }
    }
    selected.reverse();
    for group in selected { prefix.extend(group); }
    *messages = prefix;
}

fn atomic_message_groups(messages: &[Value]) -> Vec<Vec<Value>> {
    let mut groups = Vec::new();
    let mut index = 0usize;
    while index < messages.len() {
        let mut group = vec![messages[index].clone()];
        if has_tool_calls(&messages[index]) {
            index += 1;
            while index < messages.len() && is_tool_result(&messages[index]) {
                group.push(messages[index].clone());
                index += 1;
            }
        } else { index += 1; }
        groups.push(group);
    }
    groups
}

fn has_tool_calls(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("assistant")
        && message.get("tool_calls").and_then(Value::as_array).is_some_and(|calls| !calls.is_empty())
}
fn is_tool_result(message: &Value) -> bool { message.get("role").and_then(Value::as_str) == Some("tool") }
fn estimate_messages_tokens(messages: &[Value]) -> usize {
    messages.iter().map(|m| m.to_string().chars().count().div_ceil(4).max(1).saturating_add(4)).sum::<usize>()
        .saturating_add(messages.len() * 4)
}

fn snapshot_items(snapshot: &StructuredMemorySnapshot) -> Vec<(&'static str, &str)> {
    let memory = &snapshot.memory;
    let mut result = Vec::new();
    for value in &memory.facts { result.push(("fact", value.as_str())); }
    for value in &memory.decisions { result.push(("decision", value.as_str())); }
    for value in &memory.constraints { result.push(("constraint", value.as_str())); }
    for value in &memory.user_preferences { result.push(("user_preference", value.as_str())); }
    for value in &memory.entities { result.push(("entity", value.as_str())); }
    for value in &memory.code_context { result.push(("code_context", value.as_str())); }
    for value in &memory.open_questions { result.push(("open_question", value.as_str())); }
    result
}

fn validate_category(category: &str) -> Result<(), MemoryProvenanceError> {
    match category {
        "fact" | "decision" | "constraint" | "user_preference" | "entity" | "code_context" | "open_question" => Ok(()),
        other => Err(MemoryProvenanceError::InvalidCategory(other.to_string())),
    }
}
fn validate_confidence(confidence: f64) -> Result<(), MemoryProvenanceError> {
    if confidence.is_finite() && (0.0..=1.0).contains(&confidence) { Ok(()) }
    else { Err(MemoryProvenanceError::InvalidConfidence(confidence)) }
}
fn memory_item_key(category: &str, value: &str) -> String {
    let normalized = format!("{}:{}", category, value.trim().to_lowercase());
    let mut hash = 0xcbf29ce484222325u64;
    for byte in normalized.as_bytes() { hash ^= u64::from(*byte); hash = hash.wrapping_mul(0x100000001b3); }
    format!("mem_{hash:016x}")
}
fn item_from_row(row: sqlx::sqlite::SqliteRow) -> Result<MemoryItemMetadata, MemoryProvenanceError> {
    Ok(MemoryItemMetadata {
        thread_id: row.try_get("thread_id")?, item_key: row.try_get("item_key")?,
        category: row.try_get("category")?, value: row.try_get("value")?, confidence: row.try_get("confidence")?,
        pinned: row.try_get::<i64,_>("pinned")? != 0, active: row.try_get::<i64,_>("active")? != 0,
        source_kind: row.try_get("source_kind")?, first_seen_ordinal: row.try_get("first_seen_ordinal")?,
        last_seen_ordinal: row.try_get("last_seen_ordinal")?, model: row.try_get("model")?, route_id: row.try_get("route_id")?,
        created_at: row.try_get("created_at")?, updated_at: row.try_get("updated_at")?,
    })
}
fn ensure_sqlite_parent(database_url: &str) -> Result<(), std::io::Error> {
    let Some(path) = database_url.strip_prefix("sqlite://") else { return Ok(()); };
    let path = path.split('?').next().unwrap_or(path);
    if path == ":memory:" || path.is_empty() { return Ok(()); }
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() { std::fs::create_dir_all(parent)?; }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{inject_pinned_memory, memory_item_key};
    use serde_json::json;

    #[test]
    fn memory_keys_are_stable_and_category_scoped() {
        assert_eq!(memory_item_key("fact", "Uses Rust"), memory_item_key("fact", " uses rust "));
        assert_ne!(memory_item_key("fact", "Uses Rust"), memory_item_key("decision", "Uses Rust"));
    }

    #[test]
    fn pinned_memory_is_injected_without_losing_current_turn() {
        let mut messages = vec![
            json!({"role":"system","content":"checkpoint"}),
            json!({"role":"user","content":"old turn"}),
            json!({"role":"assistant","content":"old answer"}),
            json!({"role":"user","content":"current turn"}),
        ];
        let _ = inject_pinned_memory(&mut messages, "PINNED VALUE", 200);
        assert!(messages[0]["content"].as_str().unwrap().contains("PINNED VALUE"));
        assert_eq!(messages.last().unwrap()["content"], "current turn");
    }
}
