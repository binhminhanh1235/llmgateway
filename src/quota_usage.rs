use crate::config::AppConfig;
use chrono::{Datelike, SecondsFormat, Utc};
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Row, SqlitePool,
};
use std::{
    collections::HashMap,
    fs,
    path::Path,
    str::FromStr,
    sync::Arc,
};
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize)]
pub struct UsageConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub hard_limits: bool,
    #[serde(default = "default_rate_limit_cooldown_seconds")]
    pub default_rate_limit_cooldown_seconds: i64,
    #[serde(default = "default_balance_weight")]
    pub balance_weight: i32,
    #[serde(default)]
    pub accounts: HashMap<String, AccountQuotaConfig>,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hard_limits: true,
            default_rate_limit_cooldown_seconds: default_rate_limit_cooldown_seconds(),
            balance_weight: default_balance_weight(),
            accounts: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AccountQuotaConfig {
    #[serde(default)]
    pub daily_request_limit: Option<u64>,
    #[serde(default)]
    pub monthly_request_limit: Option<u64>,
    #[serde(default)]
    pub daily_token_limit: Option<u64>,
    #[serde(default)]
    pub monthly_token_limit: Option<u64>,
    #[serde(default)]
    pub rate_limit_cooldown_seconds: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ConfigEnvelope {
    #[serde(default)]
    usage: UsageConfig,
}

#[derive(Clone)]
pub struct QuotaUsageStore {
    app_config: Arc<AppConfig>,
    usage_config: Arc<UsageConfig>,
    pool: SqlitePool,
}

#[derive(Debug, Error)]
pub enum UsageError {
    #[error("usage database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("usage config read error: {0}")]
    Io(#[from] std::io::Error),
    #[error("usage config TOML error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid usage config: {0}")]
    InvalidConfig(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageWindow {
    pub requests: u64,
    pub tokens: u64,
    pub request_limit: Option<u64>,
    pub token_limit: Option<u64>,
    pub pressure: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountUsageSnapshot {
    pub account_id: String,
    pub blocked: bool,
    pub cooldown_until: Option<String>,
    pub last_429_at: Option<String>,
    pub consecutive_429: u32,
    pub remaining_requests_hint: Option<i64>,
    pub remaining_tokens_hint: Option<i64>,
    pub last_error: Option<String>,
    pub daily: UsageWindow,
    pub monthly: UsageWindow,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageSummary {
    pub enabled: bool,
    pub hard_limits: bool,
    pub accounts: Vec<AccountUsageSnapshot>,
}

#[derive(Clone, Debug)]
pub struct UsageEvent<'a> {
    pub account_id: &'a str,
    pub route_id: &'a str,
    pub model: &'a str,
    pub status_code: Option<u16>,
    pub outcome: &'a str,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub usage_source: &'a str,
    pub error: Option<&'a str>,
}

impl UsageConfig {
    pub fn load_from_gateway_config(path: impl AsRef<Path>) -> Result<Self, UsageError> {
        let raw = fs::read_to_string(path)?;
        let envelope: ConfigEnvelope = toml::from_str(&raw)?;
        Ok(envelope.usage)
    }
}

impl QuotaUsageStore {
    pub async fn connect(
        app_config: Arc<AppConfig>,
        usage_config: UsageConfig,
    ) -> Result<Self, UsageError> {
        validate_usage_config(app_config.as_ref(), &usage_config)?;
        ensure_sqlite_parent(&app_config.storage.database_url)?;
        let options = SqliteConnectOptions::from_str(&app_config.storage.database_url)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        let store = Self {
            app_config,
            usage_config: Arc::new(usage_config),
            pool,
        };
        store.migrate().await?;
        Ok(store)
    }

    pub fn enabled(&self) -> bool {
        self.usage_config.enabled
    }

    pub async fn record_event(&self, event: UsageEvent<'_>) -> Result<Option<String>, UsageError> {
        if !self.enabled() {
            return Ok(None);
        }
        let event_id = format!("usage_{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO usage_events
             (id, account_id, route_id, model, occurred_at, status_code, outcome,
              input_tokens, output_tokens, usage_source, error)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&event_id)
        .bind(event.account_id)
        .bind(event.route_id)
        .bind(event.model)
        .bind(now_string())
        .bind(event.status_code.map(i64::from))
        .bind(event.outcome)
        .bind(event.input_tokens as i64)
        .bind(event.output_tokens as i64)
        .bind(event.usage_source)
        .bind(event.error)
        .execute(&self.pool)
        .await?;
        Ok(Some(event_id))
    }

    pub async fn update_provider_usage(
        &self,
        event_id: &str,
        response: &Value,
    ) -> Result<bool, UsageError> {
        let Some((input, output)) = extract_provider_usage(response) else {
            return Ok(false);
        };
        let result = sqlx::query(
            "UPDATE usage_events
             SET input_tokens = ?, output_tokens = ?, usage_source = 'provider'
             WHERE id = ?",
        )
        .bind(input as i64)
        .bind(output as i64)
        .bind(event_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn observe_headers(
        &self,
        account_id: &str,
        headers: &HeaderMap,
    ) -> Result<(), UsageError> {
        if !self.enabled() {
            return Ok(());
        }
        let remaining_requests = header_i64(headers, "x-ratelimit-remaining-requests");
        let remaining_tokens = header_i64(headers, "x-ratelimit-remaining-tokens");
        if remaining_requests.is_none() && remaining_tokens.is_none() {
            return Ok(());
        }
        self.ensure_account_state(account_id).await?;
        sqlx::query(
            "UPDATE account_quota_state
             SET remaining_requests_hint = COALESCE(?, remaining_requests_hint),
                 remaining_tokens_hint = COALESCE(?, remaining_tokens_hint),
                 updated_at = ?
             WHERE account_id = ?",
        )
        .bind(remaining_requests)
        .bind(remaining_tokens)
        .bind(now_string())
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_rate_limited(
        &self,
        account_id: &str,
        retry_after_seconds: Option<i64>,
        error: &str,
    ) -> Result<(), UsageError> {
        if !self.enabled() {
            return Ok(());
        }
        self.ensure_account_state(account_id).await?;
        let cooldown = retry_after_seconds
            .filter(|seconds| *seconds > 0)
            .unwrap_or_else(|| self.cooldown_seconds(account_id));
        let until = Utc::now() + chrono::Duration::seconds(cooldown);
        sqlx::query(
            "UPDATE account_quota_state
             SET cooldown_until = ?, last_429_at = ?,
                 consecutive_429 = consecutive_429 + 1,
                 last_error = ?, updated_at = ?
             WHERE account_id = ?",
        )
        .bind(until.to_rfc3339_opts(SecondsFormat::Secs, true))
        .bind(now_string())
        .bind(error)
        .bind(now_string())
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_account_cooldown(
        &self,
        account_id: &str,
        seconds: i64,
        error: &str,
    ) -> Result<(), UsageError> {
        if !self.enabled() {
            return Ok(());
        }
        self.ensure_account_state(account_id).await?;
        let until = Utc::now() + chrono::Duration::seconds(seconds.max(1));
        sqlx::query(
            "UPDATE account_quota_state
             SET cooldown_until = ?, last_error = ?, updated_at = ?
             WHERE account_id = ?",
        )
        .bind(until.to_rfc3339_opts(SecondsFormat::Secs, true))
        .bind(error)
        .bind(now_string())
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_success(&self, account_id: &str) -> Result<(), UsageError> {
        if !self.enabled() {
            return Ok(());
        }
        self.ensure_account_state(account_id).await?;
        sqlx::query(
            "UPDATE account_quota_state
             SET cooldown_until = NULL, consecutive_429 = 0, last_error = NULL, updated_at = ?
             WHERE account_id = ?",
        )
        .bind(now_string())
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn reset_account_state(&self, account_id: &str) -> Result<(), UsageError> {
        if self.app_config.account(account_id).is_none() {
            return Err(UsageError::InvalidConfig(format!(
                "unknown account '{account_id}'"
            )));
        }
        self.ensure_account_state(account_id).await?;
        sqlx::query(
            "UPDATE account_quota_state
             SET cooldown_until = NULL, last_429_at = NULL, consecutive_429 = 0,
                 remaining_requests_hint = NULL, remaining_tokens_hint = NULL,
                 last_error = NULL, updated_at = ?
             WHERE account_id = ?",
        )
        .bind(now_string())
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Returns None when the account should not be routed to. Otherwise returns
    /// an additive routing penalty; lower values are preferred.
    pub async fn route_penalty(&self, account_id: &str) -> Result<Option<i32>, UsageError> {
        if !self.enabled() {
            return Ok(Some(0));
        }
        let snapshot = self.account_snapshot(account_id).await?;
        if snapshot.blocked {
            return Ok(None);
        }
        let pressure = snapshot.daily.pressure.max(snapshot.monthly.pressure);
        let pressure_penalty = (pressure.clamp(0.0, 0.999) * 1_000.0).round() as i32;
        let balance_penalty = snapshot
            .daily
            .requests
            .min(i32::MAX as u64) as i32
            * self.usage_config.balance_weight.max(0);
        let rate_limit_penalty = (snapshot.consecutive_429 as i32).saturating_mul(100);
        let hint_penalty = if snapshot.remaining_requests_hint == Some(0)
            || snapshot.remaining_tokens_hint == Some(0)
        {
            500
        } else {
            0
        };
        Ok(Some(
            pressure_penalty
                .saturating_add(balance_penalty)
                .saturating_add(rate_limit_penalty)
                .saturating_add(hint_penalty),
        ))
    }

    pub async fn account_snapshot(
        &self,
        account_id: &str,
    ) -> Result<AccountUsageSnapshot, UsageError> {
        if self.app_config.account(account_id).is_none() {
            return Err(UsageError::InvalidConfig(format!(
                "unknown account '{account_id}'"
            )));
        }
        self.ensure_account_state(account_id).await?;
        let quota = self.usage_config.accounts.get(account_id).cloned().unwrap_or_default();
        let daily = self
            .window_usage(
                account_id,
                &day_start_string(),
                quota.daily_request_limit,
                quota.daily_token_limit,
            )
            .await?;
        let monthly = self
            .window_usage(
                account_id,
                &month_start_string(),
                quota.monthly_request_limit,
                quota.monthly_token_limit,
            )
            .await?;
        let row = sqlx::query(
            "SELECT cooldown_until, last_429_at, consecutive_429,
                    remaining_requests_hint, remaining_tokens_hint, last_error
             FROM account_quota_state WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await?;
        let cooldown_until: Option<String> = row.try_get("cooldown_until")?;
        let cooldown_active = cooldown_until
            .as_deref()
            .and_then(parse_time)
            .is_some_and(|until| until > Utc::now());
        let hard_exhausted = self.usage_config.hard_limits
            && (daily.pressure >= 1.0 || monthly.pressure >= 1.0);
        Ok(AccountUsageSnapshot {
            account_id: account_id.to_string(),
            blocked: cooldown_active || hard_exhausted,
            cooldown_until,
            last_429_at: row.try_get("last_429_at")?,
            consecutive_429: row.try_get::<i64, _>("consecutive_429")?.max(0) as u32,
            remaining_requests_hint: row.try_get("remaining_requests_hint")?,
            remaining_tokens_hint: row.try_get("remaining_tokens_hint")?,
            last_error: row.try_get("last_error")?,
            daily,
            monthly,
        })
    }

    pub async fn summary(&self) -> Result<UsageSummary, UsageError> {
        let mut accounts = Vec::with_capacity(self.app_config.accounts.len());
        for account in self.app_config.accounts.iter().filter(|account| account.enabled) {
            accounts.push(self.account_snapshot(&account.id).await?);
        }
        Ok(UsageSummary {
            enabled: self.enabled(),
            hard_limits: self.usage_config.hard_limits,
            accounts,
        })
    }

    pub fn estimate_input_tokens(body: &Value) -> u64 {
        estimate_json_tokens(body).max(1)
    }

    pub fn retry_after_seconds(headers: &HeaderMap) -> Option<i64> {
        header_i64(headers, "retry-after").filter(|seconds| *seconds > 0)
    }

    async fn window_usage(
        &self,
        account_id: &str,
        start: &str,
        request_limit: Option<u64>,
        token_limit: Option<u64>,
    ) -> Result<UsageWindow, UsageError> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS requests,
                    COALESCE(SUM(input_tokens + output_tokens), 0) AS tokens
             FROM usage_events
             WHERE account_id = ? AND occurred_at >= ?",
        )
        .bind(account_id)
        .bind(start)
        .fetch_one(&self.pool)
        .await?;
        let requests = row.try_get::<i64, _>("requests")?.max(0) as u64;
        let tokens = row.try_get::<i64, _>("tokens")?.max(0) as u64;
        let request_pressure = request_limit
            .map(|limit| requests as f64 / limit.max(1) as f64)
            .unwrap_or(0.0);
        let token_pressure = token_limit
            .map(|limit| tokens as f64 / limit.max(1) as f64)
            .unwrap_or(0.0);
        Ok(UsageWindow {
            requests,
            tokens,
            request_limit,
            token_limit,
            pressure: request_pressure.max(token_pressure),
        })
    }

    async fn ensure_account_state(&self, account_id: &str) -> Result<(), UsageError> {
        sqlx::query(
            "INSERT INTO account_quota_state
             (account_id, consecutive_429, updated_at)
             VALUES (?, 0, ?)
             ON CONFLICT(account_id) DO NOTHING",
        )
        .bind(account_id)
        .bind(now_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    fn cooldown_seconds(&self, account_id: &str) -> i64 {
        self.usage_config
            .accounts
            .get(account_id)
            .and_then(|config| config.rate_limit_cooldown_seconds)
            .unwrap_or(self.usage_config.default_rate_limit_cooldown_seconds)
            .max(1)
    }

    async fn migrate(&self) -> Result<(), UsageError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS usage_events (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                route_id TEXT NOT NULL,
                model TEXT NOT NULL,
                occurred_at TEXT NOT NULL,
                status_code INTEGER,
                outcome TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                usage_source TEXT NOT NULL DEFAULT 'estimated',
                error TEXT
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_usage_events_account_time
             ON usage_events(account_id, occurred_at)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS account_quota_state (
                account_id TEXT PRIMARY KEY,
                cooldown_until TEXT,
                last_429_at TEXT,
                consecutive_429 INTEGER NOT NULL DEFAULT 0,
                remaining_requests_hint INTEGER,
                remaining_tokens_hint INTEGER,
                last_error TEXT,
                updated_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        for account in &self.app_config.accounts {
            self.ensure_account_state(&account.id).await?;
        }
        Ok(())
    }
}

fn validate_usage_config(app: &AppConfig, usage: &UsageConfig) -> Result<(), UsageError> {
    if usage.default_rate_limit_cooldown_seconds <= 0 {
        return Err(UsageError::InvalidConfig(
            "usage.default_rate_limit_cooldown_seconds must be greater than zero".into(),
        ));
    }
    if usage.balance_weight < 0 {
        return Err(UsageError::InvalidConfig(
            "usage.balance_weight must be non-negative".into(),
        ));
    }
    for (account_id, quota) in &usage.accounts {
        if app.account(account_id).is_none() {
            return Err(UsageError::InvalidConfig(format!(
                "usage account '{account_id}' is not defined in [[accounts]]"
            )));
        }
        for (name, value) in [
            ("daily_request_limit", quota.daily_request_limit),
            ("monthly_request_limit", quota.monthly_request_limit),
            ("daily_token_limit", quota.daily_token_limit),
            ("monthly_token_limit", quota.monthly_token_limit),
        ] {
            if value == Some(0) {
                return Err(UsageError::InvalidConfig(format!(
                    "usage.accounts.{account_id}.{name} must be greater than zero"
                )));
            }
        }
        if quota.rate_limit_cooldown_seconds.is_some_and(|value| value <= 0) {
            return Err(UsageError::InvalidConfig(format!(
                "usage.accounts.{account_id}.rate_limit_cooldown_seconds must be greater than zero"
            )));
        }
    }
    Ok(())
}

fn extract_provider_usage(response: &Value) -> Option<(u64, u64)> {
    let usage = response.get("usage")?;
    let input = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if input == 0 && output == 0 {
        None
    } else {
        Some((input, output))
    }
}

fn estimate_json_tokens(value: &Value) -> u64 {
    match value {
        Value::String(text) => ((text.chars().count() as u64) + 3) / 4,
        Value::Array(values) => values.iter().map(estimate_json_tokens).sum::<u64>() + 1,
        Value::Object(map) => {
            map.iter()
                .map(|(key, value)| ((key.len() as u64) + 3) / 4 + estimate_json_tokens(value))
                .sum::<u64>()
                + 1
        }
        Value::Number(_) | Value::Bool(_) => 1,
        Value::Null => 0,
    }
}

fn header_i64(headers: &HeaderMap, name: &str) -> Option<i64> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<i64>().ok())
}

fn now_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn day_start_string() -> String {
    let today = Utc::now().date_naive();
    today
        .and_hms_opt(0, 0, 0)
        .expect("valid midnight")
        .and_utc()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn month_start_string() -> String {
    let now = Utc::now();
    chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1)
        .expect("valid first day of month")
        .and_hms_opt(0, 0, 0)
        .expect("valid midnight")
        .and_utc()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn parse_time(value: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn ensure_sqlite_parent(database_url: &str) -> Result<(), std::io::Error> {
    let Some(path) = database_url.strip_prefix("sqlite://") else {
        return Ok(());
    };
    if path == ":memory:" || path.starts_with("file:") {
        return Ok(());
    }
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

fn default_true() -> bool {
    true
}

fn default_rate_limit_cooldown_seconds() -> i64 {
    60
}

fn default_balance_weight() -> i32 {
    1
}

#[cfg(test)]
mod tests {
    use super::{estimate_json_tokens, extract_provider_usage};
    use serde_json::json;

    #[test]
    fn estimates_nested_request_tokens() {
        let value = json!({"messages":[{"role":"user","content":"abcdefgh"}]});
        assert!(estimate_json_tokens(&value) >= 4);
    }

    #[test]
    fn extracts_openai_and_responses_usage_shapes() {
        assert_eq!(
            extract_provider_usage(&json!({"usage":{"prompt_tokens":10,"completion_tokens":3}})),
            Some((10, 3))
        );
        assert_eq!(
            extract_provider_usage(&json!({"usage":{"input_tokens":7,"output_tokens":2}})),
            Some((7, 2))
        );
    }
}
