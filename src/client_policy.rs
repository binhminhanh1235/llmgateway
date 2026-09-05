use crate::{
    config::{AppConfig, ClientPolicyConfig},
    live_config::LiveConfig,
    quota_usage::QuotaUsageStore,
};
use chrono::{Datelike, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Row, SqlitePool,
};
use std::{collections::HashSet, env, path::Path, str::FromStr, sync::Arc};
use thiserror::Error;
use tokio::sync::Mutex;
use uuid::Uuid;

const DEFAULT_BUDGET_OUTPUT_TOKENS: u64 = 4096;

#[derive(Clone, Debug)]
pub enum ClientAccess {
    Admin,
    Client {
        id: String,
        policy: ClientPolicyConfig,
    },
}

impl ClientAccess {
    pub fn client_id(&self) -> Option<&str> {
        match self {
            Self::Admin => None,
            Self::Client { id, .. } => Some(id),
        }
    }

    pub fn policy(&self) -> Option<&ClientPolicyConfig> {
        match self {
            Self::Admin => None,
            Self::Client { policy, .. } => Some(policy),
        }
    }
}

#[derive(Clone)]
pub struct ClientPolicyStore {
    live_config: LiveConfig,
    admin_key: Arc<String>,
    pool: SqlitePool,
    reservation_lock: Arc<Mutex<()>>,
}

#[derive(Debug, Error)]
pub enum ClientPolicyError {
    #[error("invalid client credential")]
    Unauthorized,
    #[error("client policy denied request: {0}")]
    Forbidden(String),
    #[error("client budget exceeded: {0}")]
    BudgetExceeded(String),
    #[error("client policy database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("client policy storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("client key environment variable {0} is not set")]
    MissingEnv(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct ClientBudgetWindow {
    pub requests: u64,
    pub tokens: u64,
    pub request_limit: Option<u64>,
    pub token_limit: Option<u64>,
    pub request_remaining: Option<u64>,
    pub token_remaining: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClientBudgetSnapshot {
    pub daily: ClientBudgetWindow,
    pub monthly: ClientBudgetWindow,
}

#[derive(Clone, Debug)]
pub struct ClientBudgetReservation {
    id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClientPolicySummary {
    pub id: String,
    pub enabled: bool,
    pub key_configured: bool,
    pub allowed_models: Vec<String>,
    pub allowed_routes: Vec<String>,
    pub execution_preference: Option<String>,
    pub api_fallback: Option<bool>,
    pub budget: ClientBudgetSnapshot,
}

impl ClientPolicyStore {
    pub async fn connect(
        app_config: Arc<AppConfig>,
        live_config: LiveConfig,
        admin_key: Arc<String>,
    ) -> Result<Self, ClientPolicyError> {
        ensure_sqlite_parent(&app_config.storage.database_url)?;
        let options = SqliteConnectOptions::from_str(&app_config.storage.database_url)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        let store = Self {
            live_config,
            admin_key,
            pool,
            reservation_lock: Arc::new(Mutex::new(())),
        };
        store.validate_enabled_client_keys()?;
        store.migrate().await?;
        Ok(store)
    }

    pub fn authenticate(&self, presented: &str) -> Result<ClientAccess, ClientPolicyError> {
        if presented == self.admin_key.as_str() {
            return Ok(ClientAccess::Admin);
        }

        let config = self.live_config.snapshot();
        for (id, policy) in &config.clients {
            if !policy.enabled {
                continue;
            }
            let key = env::var(&policy.key_env)
                .map_err(|_| ClientPolicyError::MissingEnv(policy.key_env.clone()))?;
            if presented == key {
                return Ok(ClientAccess::Client {
                    id: id.clone(),
                    policy: policy.clone(),
                });
            }
        }
        Err(ClientPolicyError::Unauthorized)
    }

    pub fn enforce_model(
        &self,
        access: &ClientAccess,
        requested_model: &str,
    ) -> Result<(), ClientPolicyError> {
        let Some(policy) = access.policy() else {
            return Ok(());
        };
        let config = self.live_config.snapshot();
        let resolved = config.resolve_model_alias(requested_model);
        if policy.model_allowed(requested_model, resolved) {
            Ok(())
        } else {
            Err(ClientPolicyError::Forbidden(format!(
                "model '{requested_model}' is not allowed for this client"
            )))
        }
    }

    pub async fn reserve_request(
        &self,
        access: &ClientAccess,
        body: &mut Value,
    ) -> Result<Option<ClientBudgetReservation>, ClientPolicyError> {
        let ClientAccess::Client { id, policy } = access else {
            return Ok(None);
        };

        if !policy.has_budget() {
            return Ok(None);
        }

        // Serialize admission inside one gateway process so concurrent requests cannot both
        // observe the same pre-limit snapshot and oversubscribe a client budget.
        let _reservation_guard = self.reservation_lock.lock().await;

        let input_tokens = QuotaUsageStore::estimate_input_tokens(body);
        let before = self.budget_snapshot(id, policy).await?;
        let has_token_budget =
            policy.daily_token_limit.is_some() || policy.monthly_token_limit.is_some();
        let reserved_output_tokens = match requested_output_tokens(body) {
            Some(tokens) => tokens,
            None if has_token_budget => {
                let tokens = implicit_output_cap(&before, input_tokens);
                if tokens == 0 {
                    return Err(ClientPolicyError::BudgetExceeded(
                        "token budget has no capacity for provider output".into(),
                    ));
                }
                let object = body.as_object_mut().ok_or_else(|| {
                    ClientPolicyError::Forbidden("request body must be a JSON object".into())
                })?;
                object.insert("max_tokens".into(), Value::from(tokens));
                tokens
            }
            None => 0,
        };
        let requested_tokens = input_tokens.saturating_add(reserved_output_tokens);

        enforce_window("daily", &before.daily, 1, requested_tokens)?;
        enforce_window("monthly", &before.monthly, 1, requested_tokens)?;

        let reservation_id = format!("client_usage_{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO client_usage_events
             (id, client_id, occurred_at, input_tokens, reserved_output_tokens)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&reservation_id)
        .bind(id)
        .bind(Utc::now().to_rfc3339())
        .bind(u64_to_i64(input_tokens))
        .bind(u64_to_i64(reserved_output_tokens))
        .execute(&self.pool)
        .await?;

        Ok(Some(ClientBudgetReservation { id: reservation_id }))
    }

    pub async fn reconcile_usage(
        &self,
        reservation: Option<&ClientBudgetReservation>,
        response: &Value,
    ) -> Result<(), ClientPolicyError> {
        let Some(reservation) = reservation else {
            return Ok(());
        };
        let (input_tokens, output_tokens) = normalized_observed_usage(response);
        if input_tokens.is_none() && output_tokens.is_none() {
            return Ok(());
        }

        sqlx::query(
            "UPDATE client_usage_events
             SET observed_input_tokens = COALESCE(?, observed_input_tokens),
                 observed_output_tokens = COALESCE(?, observed_output_tokens)
             WHERE id = ?",
        )
        .bind(input_tokens.map(u64_to_i64))
        .bind(output_tokens.map(u64_to_i64))
        .bind(&reservation.id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn summaries(&self) -> Result<Vec<ClientPolicySummary>, ClientPolicyError> {
        let config = self.live_config.snapshot();
        let mut ids = config.clients.keys().cloned().collect::<Vec<_>>();
        ids.sort();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(policy) = config.clients.get(&id) else {
                continue;
            };
            out.push(ClientPolicySummary {
                id: id.clone(),
                enabled: policy.enabled,
                key_configured: env::var(&policy.key_env).is_ok(),
                allowed_models: policy.allowed_models.clone(),
                allowed_routes: policy.allowed_routes.clone(),
                execution_preference: policy.execution_preference.clone(),
                api_fallback: policy.api_fallback,
                budget: self.budget_snapshot(&id, policy).await?,
            });
        }
        Ok(out)
    }

    pub async fn budget_snapshot(
        &self,
        client_id: &str,
        policy: &ClientPolicyConfig,
    ) -> Result<ClientBudgetSnapshot, ClientPolicyError> {
        let now = Utc::now();
        let day = now.format("%Y-%m-%d").to_string();
        let month = format!("{:04}-{:02}", now.year(), now.month());

        let daily = self
            .window(client_id, "substr(occurred_at, 1, 10) = ?", &day)
            .await?;
        let monthly = self
            .window(client_id, "substr(occurred_at, 1, 7) = ?", &month)
            .await?;

        Ok(ClientBudgetSnapshot {
            daily: budget_window(
                daily.0,
                daily.1,
                policy.daily_request_limit,
                policy.daily_token_limit,
            ),
            monthly: budget_window(
                monthly.0,
                monthly.1,
                policy.monthly_request_limit,
                policy.monthly_token_limit,
            ),
        })
    }

    async fn window(
        &self,
        client_id: &str,
        predicate: &str,
        window_value: &str,
    ) -> Result<(u64, u64), ClientPolicyError> {
        let sql = format!(
            "SELECT COUNT(*) AS requests,
                    COALESCE(SUM(
                        COALESCE(observed_input_tokens, input_tokens)
                        + COALESCE(observed_output_tokens, reserved_output_tokens)
                    ), 0) AS tokens
             FROM client_usage_events
             WHERE client_id = ? AND {predicate}"
        );
        let row = sqlx::query(&sql)
            .bind(client_id)
            .bind(window_value)
            .fetch_one(&self.pool)
            .await?;
        let requests = row.try_get::<i64, _>("requests")?.max(0) as u64;
        let tokens = row.try_get::<i64, _>("tokens")?.max(0) as u64;
        Ok((requests, tokens))
    }

    fn validate_enabled_client_keys(&self) -> Result<(), ClientPolicyError> {
        let config = self.live_config.snapshot();
        if self.admin_key.trim().is_empty() {
            return Err(ClientPolicyError::Forbidden(
                "global admin credential must not be empty".into(),
            ));
        }

        let mut seen = HashSet::new();
        seen.insert(self.admin_key.as_str().to_string());

        for (client_id, policy) in config.clients.iter().filter(|(_, policy)| policy.enabled) {
            let key = env::var(&policy.key_env)
                .map_err(|_| ClientPolicyError::MissingEnv(policy.key_env.clone()))?;
            if key.trim().is_empty() {
                return Err(ClientPolicyError::Forbidden(format!(
                    "credential for client '{client_id}' must not be empty"
                )));
            }
            if !seen.insert(key) {
                return Err(ClientPolicyError::Forbidden(format!(
                    "credential for client '{client_id}' duplicates another gateway credential"
                )));
            }
        }
        Ok(())
    }

    async fn migrate(&self) -> Result<(), ClientPolicyError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS client_usage_events (
                id TEXT PRIMARY KEY,
                client_id TEXT NOT NULL,
                occurred_at TEXT NOT NULL,
                input_tokens INTEGER NOT NULL,
                reserved_output_tokens INTEGER NOT NULL,
                observed_input_tokens INTEGER,
                observed_output_tokens INTEGER
            )",
        )
        .execute(&self.pool)
        .await?;
        let usage_columns = sqlx::query("PRAGMA table_info(client_usage_events)")
            .fetch_all(&self.pool)
            .await?;
        let has_observed_input = usage_columns.iter().any(|row| {
            row.try_get::<String, _>("name")
                .is_ok_and(|name| name == "observed_input_tokens")
        });
        if !has_observed_input {
            sqlx::query(
                "ALTER TABLE client_usage_events ADD COLUMN observed_input_tokens INTEGER",
            )
            .execute(&self.pool)
            .await?;
        }
        let has_observed_output = usage_columns.iter().any(|row| {
            row.try_get::<String, _>("name")
                .is_ok_and(|name| name == "observed_output_tokens")
        });
        if !has_observed_output {
            sqlx::query(
                "ALTER TABLE client_usage_events ADD COLUMN observed_output_tokens INTEGER",
            )
            .execute(&self.pool)
            .await?;
        }
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_client_usage_client_time
             ON client_usage_events(client_id, occurred_at)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn budget_window(
    requests: u64,
    tokens: u64,
    request_limit: Option<u64>,
    token_limit: Option<u64>,
) -> ClientBudgetWindow {
    ClientBudgetWindow {
        requests,
        tokens,
        request_limit,
        token_limit,
        request_remaining: request_limit.map(|limit| limit.saturating_sub(requests)),
        token_remaining: token_limit.map(|limit| limit.saturating_sub(tokens)),
    }
}

fn enforce_window(
    name: &str,
    window: &ClientBudgetWindow,
    request_increment: u64,
    token_increment: u64,
) -> Result<(), ClientPolicyError> {
    if window
        .request_limit
        .is_some_and(|limit| window.requests.saturating_add(request_increment) > limit)
    {
        return Err(ClientPolicyError::BudgetExceeded(format!(
            "{name} request budget reached"
        )));
    }
    if window
        .token_limit
        .is_some_and(|limit| window.tokens.saturating_add(token_increment) > limit)
    {
        return Err(ClientPolicyError::BudgetExceeded(format!(
            "{name} token budget reached"
        )));
    }
    Ok(())
}

fn requested_output_tokens(body: &Value) -> Option<u64> {
    ["max_output_tokens", "max_completion_tokens", "max_tokens"]
        .into_iter()
        .find_map(|key| body.get(key).and_then(Value::as_u64))
}

fn implicit_output_cap(snapshot: &ClientBudgetSnapshot, input_tokens: u64) -> u64 {
    [snapshot.daily.token_remaining, snapshot.monthly.token_remaining]
        .into_iter()
        .flatten()
        .map(|remaining| remaining.saturating_sub(input_tokens))
        .min()
        .unwrap_or(DEFAULT_BUDGET_OUTPUT_TOKENS)
        .min(DEFAULT_BUDGET_OUTPUT_TOKENS)
}

fn normalized_observed_usage(response: &Value) -> (Option<u64>, Option<u64>) {
    let Some(usage) = response.get("usage") else {
        return (None, None);
    };
    let input = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64);
    let output = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64);
    (input, output)
}

fn u64_to_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn ensure_sqlite_parent(database_url: &str) -> Result<(), std::io::Error> {
    let Some(path) = database_url.strip_prefix("sqlite://") else {
        return Ok(());
    };
    let path = Path::new(path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        enforce_window, implicit_output_cap, normalized_observed_usage,
        requested_output_tokens, ClientBudgetSnapshot, ClientBudgetWindow,
        DEFAULT_BUDGET_OUTPUT_TOKENS,
    };
    use serde_json::json;

    #[test]
    fn output_token_reservation_understands_compat_fields() {
        assert_eq!(requested_output_tokens(&json!({"max_tokens": 128})), Some(128));
        assert_eq!(
            requested_output_tokens(&json!({"max_completion_tokens": 256})),
            Some(256)
        );
        assert_eq!(
            requested_output_tokens(&json!({"max_output_tokens": 512})),
            Some(512)
        );
        assert_eq!(requested_output_tokens(&json!({})), None);
    }

    #[test]
    fn implicit_output_cap_is_bounded_by_remaining_budget() {
        let snapshot = ClientBudgetSnapshot {
            daily: ClientBudgetWindow {
                requests: 0,
                tokens: 100,
                request_limit: None,
                token_limit: Some(500),
                request_remaining: None,
                token_remaining: Some(400),
            },
            monthly: ClientBudgetWindow {
                requests: 0,
                tokens: 1000,
                request_limit: None,
                token_limit: Some(10_000),
                request_remaining: None,
                token_remaining: Some(9000),
            },
        };
        assert_eq!(implicit_output_cap(&snapshot, 25), 375);

        let unlimited = ClientBudgetSnapshot {
            daily: ClientBudgetWindow {
                requests: 0,
                tokens: 0,
                request_limit: None,
                token_limit: None,
                request_remaining: None,
                token_remaining: None,
            },
            monthly: ClientBudgetWindow {
                requests: 0,
                tokens: 0,
                request_limit: None,
                token_limit: None,
                request_remaining: None,
                token_remaining: None,
            },
        };
        assert_eq!(
            implicit_output_cap(&unlimited, 1),
            DEFAULT_BUDGET_OUTPUT_TOKENS
        );
    }

    #[test]
    fn observed_usage_accepts_openai_and_responses_fields() {
        assert_eq!(
            normalized_observed_usage(&json!({
                "usage":{"prompt_tokens":7,"completion_tokens":3}
            })),
            (Some(7), Some(3))
        );
        assert_eq!(
            normalized_observed_usage(&json!({
                "usage":{"input_tokens":11,"output_tokens":5}
            })),
            (Some(11), Some(5))
        );
    }

    #[test]
    fn budget_window_blocks_only_when_next_request_crosses_limit() {
        let window = ClientBudgetWindow {
            requests: 2,
            tokens: 90,
            request_limit: Some(2),
            token_limit: Some(100),
            request_remaining: Some(0),
            token_remaining: Some(10),
        };
        assert!(enforce_window("daily", &window, 1, 1).is_err());

        let token_window = ClientBudgetWindow {
            requests: 1,
            tokens: 90,
            request_limit: Some(10),
            token_limit: Some(100),
            request_remaining: Some(9),
            token_remaining: Some(10),
        };
        assert!(enforce_window("daily", &token_window, 1, 11).is_err());
        assert!(enforce_window("daily", &token_window, 1, 10).is_ok());
    }
}
