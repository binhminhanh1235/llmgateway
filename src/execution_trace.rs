use crate::config::AppConfig;
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use sqlx::{sqlite::{SqliteConnectOptions, SqlitePoolOptions}, Row, SqlitePool};
use std::{path::Path, str::FromStr, sync::Arc};
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone)]
pub struct ExecutionTraceStore {
    pool: SqlitePool,
}

#[derive(Debug, Error)]
pub enum ExecutionTraceError {
    #[error("execution trace database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("execution trace storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("execution trace '{0}' was not found")]
    NotFound(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionAttempt {
    pub attempt_index: i64,
    pub route_id: String,
    pub account_id: String,
    pub model: String,
    pub status_code: Option<i64>,
    pub outcome: String,
    pub retryable: bool,
    pub duration_ms: i64,
    pub error: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionTraceSummary {
    pub request_id: String,
    pub requested_model: String,
    pub preferred_route: Option<String>,
    pub status: String,
    pub selected_route: Option<String>,
    pub attempt_count: i64,
    pub final_error: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionTrace {
    #[serde(flatten)]
    pub summary: ExecutionTraceSummary,
    pub attempts: Vec<ExecutionAttempt>,
}

pub struct AttemptRecord<'a> {
    pub request_id: &'a str,
    pub attempt_index: usize,
    pub route_id: &'a str,
    pub account_id: &'a str,
    pub model: &'a str,
    pub status_code: Option<u16>,
    pub outcome: &'a str,
    pub retryable: bool,
    pub duration_ms: u128,
    pub error: Option<&'a str>,
}

impl ExecutionTraceStore {
    pub async fn connect(config: Arc<AppConfig>) -> Result<Self, ExecutionTraceError> {
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

    async fn migrate(&self) -> Result<(), ExecutionTraceError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS execution_requests (
                request_id TEXT PRIMARY KEY,
                requested_model TEXT NOT NULL,
                preferred_route TEXT,
                status TEXT NOT NULL DEFAULT 'running',
                selected_route TEXT,
                final_error TEXT,
                started_at TEXT NOT NULL,
                completed_at TEXT
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS execution_attempts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                request_id TEXT NOT NULL,
                attempt_index INTEGER NOT NULL,
                route_id TEXT NOT NULL,
                account_id TEXT NOT NULL,
                model TEXT NOT NULL,
                status_code INTEGER,
                outcome TEXT NOT NULL,
                retryable INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                error TEXT,
                created_at TEXT NOT NULL,
                UNIQUE(request_id, attempt_index),
                FOREIGN KEY(request_id) REFERENCES execution_requests(request_id) ON DELETE CASCADE
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_execution_requests_started
             ON execution_requests(started_at DESC)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_execution_attempts_request
             ON execution_attempts(request_id, attempt_index)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn start(
        &self,
        requested_model: &str,
        preferred_route: Option<&str>,
    ) -> Result<String, ExecutionTraceError> {
        let request_id = format!("req_{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO execution_requests
             (request_id, requested_model, preferred_route, status, started_at)
             VALUES (?, ?, ?, 'running', ?)",
        )
        .bind(&request_id)
        .bind(requested_model)
        .bind(preferred_route)
        .bind(now_string())
        .execute(&self.pool)
        .await?;
        Ok(request_id)
    }

    pub async fn record_attempt(&self, record: AttemptRecord<'_>) -> Result<(), ExecutionTraceError> {
        sqlx::query(
            "INSERT INTO execution_attempts
             (request_id, attempt_index, route_id, account_id, model, status_code,
              outcome, retryable, duration_ms, error, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(record.request_id)
        .bind(record.attempt_index as i64)
        .bind(record.route_id)
        .bind(record.account_id)
        .bind(record.model)
        .bind(record.status_code.map(i64::from))
        .bind(record.outcome)
        .bind(if record.retryable { 1_i64 } else { 0_i64 })
        .bind(record.duration_ms.min(i64::MAX as u128) as i64)
        .bind(record.error.map(truncate_error))
        .bind(now_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn complete(
        &self,
        request_id: &str,
        status: &str,
        selected_route: Option<&str>,
        final_error: Option<&str>,
    ) -> Result<(), ExecutionTraceError> {
        sqlx::query(
            "UPDATE execution_requests SET
                status = ?, selected_route = ?, final_error = ?, completed_at = ?
             WHERE request_id = ?",
        )
        .bind(status)
        .bind(selected_route)
        .bind(final_error.map(truncate_error))
        .bind(now_string())
        .bind(request_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, request_id: &str) -> Result<ExecutionTrace, ExecutionTraceError> {
        let summary = self.summary(request_id).await?;
        let rows = sqlx::query(
            "SELECT attempt_index, route_id, account_id, model, status_code, outcome,
                    retryable, duration_ms, error, created_at
             FROM execution_attempts WHERE request_id = ? ORDER BY attempt_index",
        )
        .bind(request_id)
        .fetch_all(&self.pool)
        .await?;
        let attempts = rows
            .into_iter()
            .map(|row| ExecutionAttempt {
                attempt_index: row.get("attempt_index"),
                route_id: row.get("route_id"),
                account_id: row.get("account_id"),
                model: row.get("model"),
                status_code: row.get("status_code"),
                outcome: row.get("outcome"),
                retryable: row.get::<i64, _>("retryable") != 0,
                duration_ms: row.get("duration_ms"),
                error: row.get("error"),
                created_at: row.get("created_at"),
            })
            .collect();
        Ok(ExecutionTrace { summary, attempts })
    }

    pub async fn list(&self, limit: usize) -> Result<Vec<ExecutionTraceSummary>, ExecutionTraceError> {
        let rows = sqlx::query(
            "SELECT r.request_id, r.requested_model, r.preferred_route, r.status,
                    r.selected_route, r.final_error, r.started_at, r.completed_at,
                    COUNT(a.id) AS attempt_count
             FROM execution_requests r
             LEFT JOIN execution_attempts a ON a.request_id = r.request_id
             GROUP BY r.request_id
             ORDER BY r.started_at DESC
             LIMIT ?",
        )
        .bind(limit.clamp(1, 200) as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(summary_from_row).collect())
    }

    async fn summary(&self, request_id: &str) -> Result<ExecutionTraceSummary, ExecutionTraceError> {
        let row = sqlx::query(
            "SELECT r.request_id, r.requested_model, r.preferred_route, r.status,
                    r.selected_route, r.final_error, r.started_at, r.completed_at,
                    COUNT(a.id) AS attempt_count
             FROM execution_requests r
             LEFT JOIN execution_attempts a ON a.request_id = r.request_id
             WHERE r.request_id = ?
             GROUP BY r.request_id",
        )
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| ExecutionTraceError::NotFound(request_id.to_string()))?;
        Ok(summary_from_row(row))
    }
}

fn summary_from_row(row: sqlx::sqlite::SqliteRow) -> ExecutionTraceSummary {
    ExecutionTraceSummary {
        request_id: row.get("request_id"),
        requested_model: row.get("requested_model"),
        preferred_route: row.get("preferred_route"),
        status: row.get("status"),
        selected_route: row.get("selected_route"),
        attempt_count: row.get("attempt_count"),
        final_error: row.get("final_error"),
        started_at: row.get("started_at"),
        completed_at: row.get("completed_at"),
    }
}

fn now_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn truncate_error(value: &str) -> String {
    const MAX_CHARS: usize = 2_000;
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn ensure_sqlite_parent(database_url: &str) -> std::io::Result<()> {
    let Some(path) = database_url.strip_prefix("sqlite://") else {
        return Ok(());
    };
    let path = path.split('?').next().unwrap_or(path);
    if path == ":memory:" || path.is_empty() {
        return Ok(());
    }
    if let Some(parent) = Path::new(path).parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::truncate_error;

    #[test]
    fn error_payloads_are_bounded() {
        let value = "x".repeat(2_100);
        let truncated = truncate_error(&value);
        assert!(truncated.chars().count() <= 2_001);
        assert!(truncated.ends_with('…'));
    }
}
