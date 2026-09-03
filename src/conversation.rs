use crate::config::AppConfig;
use bytes::Bytes;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{sqlite::{SqliteConnectOptions, SqlitePoolOptions}, Row, SqlitePool};
use std::{collections::BTreeMap, convert::Infallible, path::Path, str::FromStr, sync::Arc};
use thiserror::Error;
use tokio::sync::oneshot;
use uuid::Uuid;

#[derive(Clone)]
pub struct ConversationStore {
    pool: SqlitePool,
}

#[derive(Debug, Error)]
pub enum ConversationError {
    #[error("conversation database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("conversation storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("thread '{0}' was not found")]
    ThreadNotFound(String),
    #[error("response '{0}' was not found")]
    ResponseNotFound(String),
    #[error("invalid stored conversation JSON: {0}")]
    InvalidJson(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct ThreadSummary {
    pub id: String,
    pub title: String,
    pub model: String,
    pub sticky_route: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct StoredMessage {
    pub id: String,
    pub role: String,
    pub message: Value,
    pub model: Option<String>,
    pub route_id: Option<String>,
    pub ordinal: i64,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ThreadDetail {
    pub id: String,
    pub title: String,
    pub model: String,
    pub sticky_route: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub messages: Vec<StoredMessage>,
}

#[derive(Clone, Debug)]
pub struct ThreadContext {
    pub id: String,
    pub model: String,
    pub sticky_route: Option<String>,
    pub messages: Vec<Value>,
}

#[derive(Clone, Debug)]
pub struct ResponseContext {
    pub response_id: String,
    pub requested_model: String,
    pub messages: Vec<Value>,
    pub route_id: Option<String>,
}

impl ConversationStore {
    pub async fn connect(config: Arc<AppConfig>) -> Result<Self, ConversationError> {
        ensure_sqlite_parent(&config.storage.database_url)?;
        let options = SqliteConnectOptions::from_str(&config.storage.database_url)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<(), ConversationError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS threads (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                model TEXT NOT NULL,
                sticky_route TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS thread_messages (
                id TEXT PRIMARY KEY,
                thread_id TEXT NOT NULL,
                role TEXT NOT NULL,
                message_json TEXT NOT NULL,
                model TEXT,
                route_id TEXT,
                ordinal INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(thread_id, ordinal),
                FOREIGN KEY(thread_id) REFERENCES threads(id) ON DELETE CASCADE
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_thread_messages_thread
             ON thread_messages(thread_id, ordinal)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS response_contexts (
                response_id TEXT PRIMARY KEY,
                requested_model TEXT NOT NULL,
                messages_json TEXT NOT NULL,
                route_id TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn create_thread(
        &self,
        title: Option<&str>,
        model: &str,
    ) -> Result<ThreadDetail, ConversationError> {
        let id = format!("thread_{}", Uuid::new_v4());
        let title = title
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("New chat")
            .trim();
        sqlx::query("INSERT INTO threads (id, title, model) VALUES (?, ?, ?)")
            .bind(&id)
            .bind(title)
            .bind(model)
            .execute(&self.pool)
            .await?;
        self.thread(&id).await
    }

    pub async fn list_threads(&self) -> Result<Vec<ThreadSummary>, ConversationError> {
        let rows = sqlx::query(
            "SELECT t.id, t.title, t.model, t.sticky_route, t.created_at, t.updated_at,
                    COUNT(m.id) AS message_count
             FROM threads t
             LEFT JOIN thread_messages m ON m.thread_id = t.id
             GROUP BY t.id
             ORDER BY t.updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(thread_summary_from_row).collect()
    }

    pub async fn thread(&self, id: &str) -> Result<ThreadDetail, ConversationError> {
        let row = sqlx::query(
            "SELECT id, title, model, sticky_route, created_at, updated_at
             FROM threads WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| ConversationError::ThreadNotFound(id.to_string()))?;

        let message_rows = sqlx::query(
            "SELECT id, role, message_json, model, route_id, ordinal, created_at
             FROM thread_messages WHERE thread_id = ? ORDER BY ordinal",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;

        let mut messages = Vec::with_capacity(message_rows.len());
        for message_row in message_rows {
            let raw: String = message_row.try_get("message_json")?;
            let message = serde_json::from_str(&raw)
                .map_err(|error| ConversationError::InvalidJson(error.to_string()))?;
            messages.push(StoredMessage {
                id: message_row.try_get("id")?,
                role: message_row.try_get("role")?,
                message,
                model: message_row.try_get("model")?,
                route_id: message_row.try_get("route_id")?,
                ordinal: message_row.try_get("ordinal")?,
                created_at: message_row.try_get("created_at")?,
            });
        }

        Ok(ThreadDetail {
            id: row.try_get("id")?,
            title: row.try_get("title")?,
            model: row.try_get("model")?,
            sticky_route: row.try_get("sticky_route")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            messages,
        })
    }

    pub async fn context(&self, id: &str) -> Result<ThreadContext, ConversationError> {
        let thread = self.thread(id).await?;
        Ok(ThreadContext {
            id: thread.id,
            model: thread.model,
            sticky_route: thread.sticky_route,
            messages: thread.messages.into_iter().map(|message| message.message).collect(),
        })
    }

    pub async fn append_message(
        &self,
        thread_id: &str,
        message: &Value,
        model: Option<&str>,
        route_id: Option<&str>,
    ) -> Result<StoredMessage, ConversationError> {
        if self.thread_exists(thread_id).await? == false {
            return Err(ConversationError::ThreadNotFound(thread_id.to_string()));
        }
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("assistant")
            .to_string();
        let raw = serde_json::to_string(message)
            .map_err(|error| ConversationError::InvalidJson(error.to_string()))?;
        let id = format!("msg_{}", Uuid::new_v4());

        let mut tx = self.pool.begin().await?;
        let ordinal: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM thread_messages WHERE thread_id = ?",
        )
        .bind(thread_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO thread_messages
             (id, thread_id, role, message_json, model, route_id, ordinal)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(thread_id)
        .bind(&role)
        .bind(raw)
        .bind(model)
        .bind(route_id)
        .bind(ordinal)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE threads SET updated_at = CURRENT_TIMESTAMP WHERE id = ?")
            .bind(thread_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        let row = sqlx::query(
            "SELECT id, role, message_json, model, route_id, ordinal, created_at
             FROM thread_messages WHERE id = ?",
        )
        .bind(&id)
        .fetch_one(&self.pool)
        .await?;
        let stored_raw: String = row.try_get("message_json")?;
        Ok(StoredMessage {
            id: row.try_get("id")?,
            role: row.try_get("role")?,
            message: serde_json::from_str(&stored_raw)
                .map_err(|error| ConversationError::InvalidJson(error.to_string()))?,
            model: row.try_get("model")?,
            route_id: row.try_get("route_id")?,
            ordinal: row.try_get("ordinal")?,
            created_at: row.try_get("created_at")?,
        })
    }

    pub async fn update_thread_route_and_model(
        &self,
        thread_id: &str,
        route_id: &str,
        model: &str,
    ) -> Result<(), ConversationError> {
        let result = sqlx::query(
            "UPDATE threads SET sticky_route = ?, model = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(route_id)
        .bind(model)
        .bind(thread_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(ConversationError::ThreadNotFound(thread_id.to_string()));
        }
        Ok(())
    }

    pub async fn delete_thread(&self, thread_id: &str) -> Result<(), ConversationError> {
        let result = sqlx::query("DELETE FROM threads WHERE id = ?")
            .bind(thread_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(ConversationError::ThreadNotFound(thread_id.to_string()));
        }
        Ok(())
    }

    pub async fn save_response_context(
        &self,
        response_id: &str,
        requested_model: &str,
        messages: &[Value],
        route_id: Option<&str>,
    ) -> Result<(), ConversationError> {
        let raw = serde_json::to_string(messages)
            .map_err(|error| ConversationError::InvalidJson(error.to_string()))?;
        sqlx::query(
            "INSERT INTO response_contexts (response_id, requested_model, messages_json, route_id)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(response_id) DO UPDATE SET
                requested_model = excluded.requested_model,
                messages_json = excluded.messages_json,
                route_id = excluded.route_id",
        )
        .bind(response_id)
        .bind(requested_model)
        .bind(raw)
        .bind(route_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn response_context(&self, response_id: &str) -> Result<ResponseContext, ConversationError> {
        let row = sqlx::query(
            "SELECT response_id, requested_model, messages_json, route_id
             FROM response_contexts WHERE response_id = ?",
        )
        .bind(response_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| ConversationError::ResponseNotFound(response_id.to_string()))?;
        let raw: String = row.try_get("messages_json")?;
        let messages = serde_json::from_str(&raw)
            .map_err(|error| ConversationError::InvalidJson(error.to_string()))?;
        Ok(ResponseContext {
            response_id: row.try_get("response_id")?,
            requested_model: row.try_get("requested_model")?,
            messages,
            route_id: row.try_get("route_id")?,
        })
    }

    async fn thread_exists(&self, thread_id: &str) -> Result<bool, ConversationError> {
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM threads WHERE id = ?")
            .bind(thread_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(exists > 0)
    }
}

pub fn openai_stream_with_capture(
    response: reqwest::Response,
    completion: oneshot::Sender<Value>,
) -> impl futures_util::Stream<Item = Result<Bytes, Infallible>> {
    async_stream::stream! {
        let mut upstream = response.bytes_stream();
        let mut buffer = String::new();
        let mut text = String::new();
        let mut tools: BTreeMap<usize, CapturedTool> = BTreeMap::new();

        while let Some(item) = upstream.next().await {
            match item {
                Ok(bytes) => {
                    let normalized = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
                    buffer.push_str(&normalized);
                    capture_frames(&mut buffer, &mut text, &mut tools);
                    yield Ok(bytes);
                }
                Err(_) => {
                    return;
                }
            }
        }
        capture_frames(&mut buffer, &mut text, &mut tools);
        let mut message = json!({
            "role":"assistant",
            "content": if text.is_empty() { Value::Null } else { Value::String(text) }
        });
        if !tools.is_empty() {
            let tool_calls = tools.into_values().map(|tool| json!({
                "id": if tool.id.is_empty() { format!("call_{}", Uuid::new_v4()) } else { tool.id },
                "type":"function",
                "function":{
                    "name": if tool.name.is_empty() { "tool".to_string() } else { tool.name },
                    "arguments":tool.arguments
                }
            })).collect::<Vec<_>>();
            if let Some(object) = message.as_object_mut() {
                object.insert("tool_calls".into(), Value::Array(tool_calls));
            }
        }
        let _ = completion.send(message);
    }
}

#[derive(Default)]
struct CapturedTool {
    id: String,
    name: String,
    arguments: String,
}

fn capture_frames(buffer: &mut String, text: &mut String, tools: &mut BTreeMap<usize, CapturedTool>) {
    while let Some(pos) = buffer.find("\n\n") {
        let frame = buffer[..pos].to_string();
        buffer.drain(..pos + 2);
        let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let Some(delta) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("delta"))
        else {
            continue;
        };
        if let Some(content) = delta.get("content") {
            if let Some(value) = content.as_str() {
                text.push_str(value);
            } else if let Some(parts) = content.as_array() {
                for part in parts {
                    if let Some(value) = part.get("text").and_then(Value::as_str) {
                        text.push_str(value);
                    }
                }
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let captured = tools.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    captured.id = id.to_string();
                }
                if let Some(name) = call
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                {
                    captured.name.push_str(name);
                }
                if let Some(arguments) = call
                    .get("function")
                    .and_then(|function| function.get("arguments"))
                    .and_then(Value::as_str)
                {
                    captured.arguments.push_str(arguments);
                }
            }
        }
    }
}

fn thread_summary_from_row(row: sqlx::sqlite::SqliteRow) -> Result<ThreadSummary, ConversationError> {
    Ok(ThreadSummary {
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        model: row.try_get("model")?,
        sticky_route: row.try_get("sticky_route")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        message_count: row.try_get("message_count")?,
    })
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
    use super::*;

    #[test]
    fn capture_frames_collects_text_and_tool_calls() {
        let mut buffer = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"p\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]}}]}\n\n"
        ).to_string();
        let mut text = String::new();
        let mut tools = BTreeMap::new();
        capture_frames(&mut buffer, &mut text, &mut tools);
        assert_eq!(text, "hello");
        assert_eq!(tools.get(&0).unwrap().name, "read");
        assert_eq!(tools.get(&0).unwrap().arguments, "{\"p\":1}");
    }
}
