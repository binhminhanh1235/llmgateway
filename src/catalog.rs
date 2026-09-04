use crate::{config::AccountConfig, live_config::LiveConfig};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION},
    Client, StatusCode,
};
use serde::Serialize;
use serde_json::Value;
use sqlx::{sqlite::{SqliteConnectOptions, SqlitePoolOptions}, Row, SqlitePool};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    path::Path,
    str::FromStr,
    time::Duration,
};
use thiserror::Error;

#[derive(Clone)]
pub struct ModelCatalog {
    config: LiveConfig,
    pool: SqlitePool,
    client: Client,
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("catalog database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("invalid catalog configuration: {0}")]
    InvalidConfig(String),
    #[error("missing credential environment variable '{0}'")]
    MissingCredential(String),
    #[error("model discovery request failed: {0}")]
    Transport(String),
    #[error("model discovery upstream returned {status}: {body}")]
    Upstream { status: StatusCode, body: String },
    #[error("invalid model discovery response: {0}")]
    InvalidResponse(String),
    #[error("failed to prepare catalog storage: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountModelView {
    pub account_id: String,
    pub availability: String,
    pub enabled: bool,
    pub configured: bool,
    pub discovered: bool,
    pub last_seen_at: Option<String>,
    pub last_verified_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CatalogModelView {
    pub id: String,
    pub provider: String,
    pub external_id: String,
    pub display_name: String,
    pub owned_by: String,
    pub context_window: Option<i64>,
    pub capabilities: Vec<String>,
    pub accounts: Vec<AccountModelView>,
    pub routes: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountView {
    pub id: String,
    pub provider: String,
    pub enabled: bool,
    pub discover_models: bool,
    pub model_count: i64,
    pub available_model_count: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RefreshResult {
    pub account_id: String,
    pub provider: String,
    pub discovered_models: usize,
}

#[derive(Clone, Debug)]
struct DiscoveredModel {
    external_id: String,
    display_name: String,
    owned_by: String,
    context_window: Option<i64>,
    capabilities: Vec<String>,
    metadata_json: String,
}

impl ModelCatalog {
    pub async fn connect(config: LiveConfig) -> Result<Self, CatalogError> {
        let snapshot = config.snapshot();
        ensure_sqlite_parent(&snapshot.storage.database_url)?;
        let options = SqliteConnectOptions::from_str(&snapshot.storage.database_url)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|error| CatalogError::Transport(error.to_string()))?;
        let catalog = Self {
            config,
            pool,
            client,
        };
        catalog.migrate().await?;
        Ok(catalog)
    }

    async fn migrate(&self) -> Result<(), CatalogError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS models (
                canonical_id TEXT PRIMARY KEY,
                provider_id TEXT NOT NULL,
                external_id TEXT NOT NULL,
                display_name TEXT NOT NULL,
                owned_by TEXT NOT NULL,
                context_window INTEGER,
                capabilities_json TEXT NOT NULL DEFAULT '[]',
                metadata_json TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(provider_id, external_id)
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS account_models (
                account_id TEXT NOT NULL,
                canonical_model_id TEXT NOT NULL,
                availability TEXT NOT NULL DEFAULT 'unknown',
                enabled INTEGER NOT NULL DEFAULT 1,
                configured INTEGER NOT NULL DEFAULT 0,
                discovered INTEGER NOT NULL DEFAULT 0,
                last_seen_at TEXT,
                last_verified_at TEXT,
                last_error TEXT,
                PRIMARY KEY(account_id, canonical_model_id),
                FOREIGN KEY(canonical_model_id) REFERENCES models(canonical_id) ON DELETE CASCADE
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_account_models_model
             ON account_models(canonical_model_id)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn seed_from_config(&self) -> Result<(), CatalogError> {
        let config = self.config.snapshot();
        self.seed_from_app_config(config.as_ref()).await
    }

    pub async fn seed_from_app_config(&self, config: &crate::config::AppConfig) -> Result<(), CatalogError> {
        let mut capabilities_by_model: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for route in config.routes.iter().filter(|route| route.enabled) {
            let account = config.account(&route.account).ok_or_else(|| {
                CatalogError::InvalidConfig(format!("unknown account '{}'", route.account))
            })?;
            let provider = config.provider(&account.provider).ok_or_else(|| {
                CatalogError::InvalidConfig(format!("unknown provider '{}'", account.provider))
            })?;
            let canonical_id = canonical_model_id(&provider.id, &route.model);
            capabilities_by_model
                .entry(canonical_id)
                .or_default()
                .extend(route.capabilities.iter().cloned());
        }

        for (canonical_id, capabilities) in &capabilities_by_model {
            let (provider_id, external_id) = split_canonical_model_id(canonical_id)?;
            self.upsert_model(
                provider_id,
                external_id,
                external_id,
                provider_id,
                None,
                capabilities.iter().cloned().collect(),
                "{}".to_string(),
            )
            .await?;
        }

        for route in config.routes.iter().filter(|route| route.enabled) {
            let account = config.account(&route.account).ok_or_else(|| {
                CatalogError::InvalidConfig(format!("unknown account '{}'", route.account))
            })?;
            let canonical_id = canonical_model_id(&account.provider, &route.model);
            sqlx::query(
                "INSERT INTO account_models
                 (account_id, canonical_model_id, availability, enabled, configured, discovered)
                 VALUES (?, ?, 'unknown', 1, 1, 0)
                 ON CONFLICT(account_id, canonical_model_id) DO UPDATE SET
                    configured = 1",
            )
            .bind(&account.id)
            .bind(&canonical_id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub async fn refresh_account(&self, account_id: &str) -> Result<RefreshResult, CatalogError> {
        let config = self.config.snapshot();
        let account = config
            .account(account_id)
            .ok_or_else(|| CatalogError::InvalidConfig(format!("unknown account '{account_id}'")))?;
        if !account.discover_models {
            return Err(CatalogError::InvalidConfig(format!(
                "model discovery is disabled for account '{account_id}'"
            )));
        }
        let provider = config.provider(&account.provider).ok_or_else(|| {
            CatalogError::InvalidConfig(format!("unknown provider '{}'", account.provider))
        })?;
        if provider.models_path.trim().is_empty() {
            return Err(CatalogError::InvalidConfig(format!(
                "provider '{}' has no models_path",
                provider.id
            )));
        }

        let key = env::var(&account.api_key_env)
            .map_err(|_| CatalogError::MissingCredential(account.api_key_env.clone()))?;
        let url = discovery_url(&provider.base_url, &provider.models_path);
        let mut headers = HeaderMap::new();
        apply_auth(&mut headers, account, &key)?;
        let response = self
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|error| CatalogError::Transport(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CatalogError::Upstream { status, body });
        }
        let payload = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        let discovered = parse_discovered_models(&payload, &provider.id)?;

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE account_models SET
                discovered = 0,
                availability = CASE WHEN configured = 1 THEN 'unknown' ELSE 'unavailable' END
             WHERE account_id = ?",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        for model in &discovered {
            self.upsert_discovered_model(&provider.id, model).await?;
            let canonical_id = canonical_model_id(&provider.id, &model.external_id);
            sqlx::query(
                "INSERT INTO account_models
                 (account_id, canonical_model_id, availability, enabled, configured, discovered,
                  last_seen_at, last_verified_at, last_error)
                 VALUES (?, ?, 'available', 1, 0, 1, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL)
                 ON CONFLICT(account_id, canonical_model_id) DO UPDATE SET
                    availability = 'available',
                    discovered = 1,
                    last_seen_at = CURRENT_TIMESTAMP,
                    last_verified_at = CURRENT_TIMESTAMP,
                    last_error = NULL",
            )
            .bind(account_id)
            .bind(&canonical_id)
            .execute(&self.pool)
            .await?;
        }

        Ok(RefreshResult {
            account_id: account_id.to_string(),
            provider: provider.id.clone(),
            discovered_models: discovered.len(),
        })
    }

    pub async fn accounts(&self) -> Result<Vec<AccountView>, CatalogError> {
        let config = self.config.snapshot();
        let mut result = Vec::with_capacity(config.accounts.len());
        for account in &config.accounts {
            let row = sqlx::query(
                "SELECT
                    COUNT(*) AS model_count,
                    COALESCE(SUM(CASE WHEN availability = 'available' AND enabled = 1 THEN 1 ELSE 0 END), 0)
                        AS available_model_count
                 FROM account_models WHERE account_id = ?",
            )
            .bind(&account.id)
            .fetch_one(&self.pool)
            .await?;
            result.push(AccountView {
                id: account.id.clone(),
                provider: account.provider.clone(),
                enabled: account.enabled,
                discover_models: account.discover_models,
                model_count: row.try_get("model_count")?,
                available_model_count: row.try_get("available_model_count")?,
            });
        }
        Ok(result)
    }

    pub async fn account_models(&self, account_id: &str) -> Result<Vec<CatalogModelView>, CatalogError> {
        let config = self.config.snapshot();
        if config.account(account_id).is_none() {
            return Err(CatalogError::InvalidConfig(format!("unknown account '{account_id}'")));
        }
        let all = self.models().await?;
        Ok(all
            .into_iter()
            .filter(|model| model.accounts.iter().any(|account| account.account_id == account_id))
            .collect())
    }

    pub async fn models(&self) -> Result<Vec<CatalogModelView>, CatalogError> {
        let rows = sqlx::query(
            "SELECT
                m.canonical_id, m.provider_id, m.external_id, m.display_name, m.owned_by,
                m.context_window, m.capabilities_json,
                am.account_id, am.availability, am.enabled, am.configured, am.discovered,
                am.last_seen_at, am.last_verified_at, am.last_error
             FROM models m
             LEFT JOIN account_models am ON am.canonical_model_id = m.canonical_id
             ORDER BY m.provider_id, m.external_id, am.account_id",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut models: BTreeMap<String, CatalogModelView> = BTreeMap::new();
        for row in rows {
            let canonical_id: String = row.try_get("canonical_id")?;
            let capabilities_raw: String = row.try_get("capabilities_json")?;
            let capabilities = serde_json::from_str::<Vec<String>>(&capabilities_raw).unwrap_or_default();
            let entry = models.entry(canonical_id.clone()).or_insert_with(|| CatalogModelView {
                id: canonical_id.clone(),
                provider: row.get("provider_id"),
                external_id: row.get("external_id"),
                display_name: row.get("display_name"),
                owned_by: row.get("owned_by"),
                context_window: row.get("context_window"),
                capabilities,
                accounts: Vec::new(),
                routes: self.routes_for_model(&canonical_id),
            });

            let account_id: Option<String> = row.try_get("account_id")?;
            if let Some(account_id) = account_id {
                entry.accounts.push(AccountModelView {
                    account_id,
                    availability: row.get("availability"),
                    enabled: row.get::<i64, _>("enabled") != 0,
                    configured: row.get::<i64, _>("configured") != 0,
                    discovered: row.get::<i64, _>("discovered") != 0,
                    last_seen_at: row.get("last_seen_at"),
                    last_verified_at: row.get("last_verified_at"),
                    last_error: row.get("last_error"),
                });
            }
        }
        Ok(models.into_values().collect())
    }

    pub async fn selectable_models(&self) -> Result<Vec<CatalogModelView>, CatalogError> {
        Ok(self
            .models()
            .await?
            .into_iter()
            .filter(|model| {
                model.accounts.iter().any(|account| {
                    account.enabled && matches!(account.availability.as_str(), "available" | "unknown")
                })
            })
            .collect())
    }

    fn routes_for_model(&self, canonical_id: &str) -> Vec<String> {
        let Ok((provider_id, external_id)) = split_canonical_model_id(canonical_id) else {
            return Vec::new();
        };
        let config = self.config.snapshot();
        config
            .routes
            .iter()
            .filter(|route| {
                route.enabled
                    && route.model == external_id
                    && config
                        .account(&route.account)
                        .is_some_and(|account| account.provider == provider_id)
            })
            .map(|route| route.id.clone())
            .collect()
    }

    async fn upsert_discovered_model(
        &self,
        provider_id: &str,
        model: &DiscoveredModel,
    ) -> Result<(), CatalogError> {
        let canonical_id = canonical_model_id(provider_id, &model.external_id);
        let existing = sqlx::query("SELECT capabilities_json FROM models WHERE canonical_id = ?")
            .bind(&canonical_id)
            .fetch_optional(&self.pool)
            .await?;
        let mut capabilities = model.capabilities.iter().cloned().collect::<BTreeSet<_>>();
        if let Some(row) = existing {
            let raw: String = row.try_get("capabilities_json")?;
            capabilities.extend(serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default());
        }
        self.upsert_model(
            provider_id,
            &model.external_id,
            &model.display_name,
            &model.owned_by,
            model.context_window,
            capabilities.into_iter().collect(),
            model.metadata_json.clone(),
        )
        .await
    }

    async fn upsert_model(
        &self,
        provider_id: &str,
        external_id: &str,
        display_name: &str,
        owned_by: &str,
        context_window: Option<i64>,
        capabilities: Vec<String>,
        metadata_json: String,
    ) -> Result<(), CatalogError> {
        let canonical_id = canonical_model_id(provider_id, external_id);
        let capabilities_json = serde_json::to_string(&capabilities)
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        sqlx::query(
            "INSERT INTO models
             (canonical_id, provider_id, external_id, display_name, owned_by, context_window,
              capabilities_json, metadata_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(canonical_id) DO UPDATE SET
                display_name = excluded.display_name,
                owned_by = excluded.owned_by,
                context_window = COALESCE(excluded.context_window, models.context_window),
                capabilities_json = excluded.capabilities_json,
                metadata_json = CASE
                    WHEN excluded.metadata_json = '{}' THEN models.metadata_json
                    ELSE excluded.metadata_json
                END,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&canonical_id)
        .bind(provider_id)
        .bind(external_id)
        .bind(display_name)
        .bind(owned_by)
        .bind(context_window)
        .bind(capabilities_json)
        .bind(metadata_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

pub fn canonical_model_id(provider_id: &str, external_id: &str) -> String {
    format!("{}/{}", provider_id.trim_matches('/'), external_id.trim_start_matches('/'))
}

fn split_canonical_model_id(id: &str) -> Result<(&str, &str), CatalogError> {
    let (provider, external) = id
        .split_once('/')
        .ok_or_else(|| CatalogError::InvalidConfig(format!("invalid canonical model id '{id}'")))?;
    if provider.is_empty() || external.is_empty() {
        return Err(CatalogError::InvalidConfig(format!("invalid canonical model id '{id}'")));
    }
    Ok((provider, external))
}

fn discovery_url(base_url: &str, models_path: &str) -> String {
    if models_path.starts_with("http://") || models_path.starts_with("https://") {
        models_path.to_string()
    } else {
        format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            models_path.trim_start_matches('/')
        )
    }
}

fn apply_auth(
    headers: &mut HeaderMap,
    account: &AccountConfig,
    key: &str,
) -> Result<(), CatalogError> {
    match account.auth_style.as_str() {
        "bearer" => {
            let value = HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|error| CatalogError::InvalidConfig(error.to_string()))?;
            headers.insert(AUTHORIZATION, value);
        }
        "x-api-key" => {
            let value = HeaderValue::from_str(key)
                .map_err(|error| CatalogError::InvalidConfig(error.to_string()))?;
            headers.insert(HeaderName::from_static("x-api-key"), value);
        }
        other => {
            return Err(CatalogError::InvalidConfig(format!(
                "unsupported auth_style '{other}'"
            )));
        }
    }
    Ok(())
}

fn parse_discovered_models(payload: &Value, provider_id: &str) -> Result<Vec<DiscoveredModel>, CatalogError> {
    let items = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.get("models").and_then(Value::as_array))
        .ok_or_else(|| CatalogError::InvalidResponse("expected a 'data' or 'models' array".into()))?;

    let mut result = Vec::new();
    for item in items {
        let raw_id = item
            .get("id")
            .or_else(|| item.get("name"))
            .and_then(Value::as_str);
        let Some(raw_id) = raw_id else { continue; };
        let external_id = raw_id.strip_prefix("models/").unwrap_or(raw_id).to_string();
        if external_id.is_empty() {
            continue;
        }
        let display_name = item
            .get("display_name")
            .or_else(|| item.get("displayName"))
            .and_then(Value::as_str)
            .unwrap_or(&external_id)
            .to_string();
        let owned_by = item
            .get("owned_by")
            .and_then(Value::as_str)
            .unwrap_or(provider_id)
            .to_string();
        let context_window = item
            .get("context_length")
            .or_else(|| item.get("context_window"))
            .or_else(|| item.get("inputTokenLimit"))
            .and_then(Value::as_i64);
        result.push(DiscoveredModel {
            capabilities: infer_capabilities(item, &external_id),
            metadata_json: item.to_string(),
            external_id,
            display_name,
            owned_by,
            context_window,
        });
    }
    Ok(result)
}

fn infer_capabilities(item: &Value, external_id: &str) -> Vec<String> {
    let mut capabilities = BTreeSet::new();
    let lower = external_id.to_ascii_lowercase();
    if !lower.contains("embed") {
        capabilities.insert("chat".to_string());
    }
    if lower.contains("coder") || lower.contains("code") {
        capabilities.insert("coding".to_string());
    }

    if let Some(methods) = item.get("supportedGenerationMethods").and_then(Value::as_array) {
        if methods.iter().any(|method| method.as_str() == Some("generateContent")) {
            capabilities.insert("chat".to_string());
        }
    }
    if let Some(parameters) = item.get("supported_parameters").and_then(Value::as_array) {
        for parameter in parameters.iter().filter_map(Value::as_str) {
            match parameter {
                "tools" | "tool_choice" => { capabilities.insert("tools".to_string()); }
                "reasoning" | "reasoning_effort" => { capabilities.insert("reasoning".to_string()); }
                _ => {}
            }
        }
    }
    if let Some(modalities) = item
        .get("architecture")
        .and_then(|architecture| architecture.get("input_modalities"))
        .and_then(Value::as_array)
    {
        for modality in modalities.iter().filter_map(Value::as_str) {
            match modality {
                "image" => { capabilities.insert("vision".to_string()); }
                "audio" => { capabilities.insert("audio".to_string()); }
                _ => {}
            }
        }
    }
    capabilities.into_iter().collect()
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
    use serde_json::json;

    #[test]
    fn canonical_id_keeps_nested_external_model_id() {
        assert_eq!(
            canonical_model_id("openrouter", "anthropic/claude-sonnet"),
            "openrouter/anthropic/claude-sonnet"
        );
        let (provider, external) = split_canonical_model_id("openrouter/anthropic/claude-sonnet").unwrap();
        assert_eq!(provider, "openrouter");
        assert_eq!(external, "anthropic/claude-sonnet");
    }

    #[test]
    fn parses_openrouter_model_metadata() {
        let payload = json!({
            "data":[{
                "id":"vendor/model-x",
                "name":"Model X",
                "context_length":131072,
                "supported_parameters":["tools","reasoning"],
                "architecture":{"input_modalities":["text","image"]}
            }]
        });
        let models = parse_discovered_models(&payload, "openrouter").unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].external_id, "vendor/model-x");
        assert_eq!(models[0].context_window, Some(131072));
        assert!(models[0].capabilities.contains(&"tools".to_string()));
        assert!(models[0].capabilities.contains(&"vision".to_string()));
    }

    #[test]
    fn parses_gemini_native_models_shape() {
        let payload = json!({
            "models":[{
                "name":"models/gemini-test",
                "displayName":"Gemini Test",
                "inputTokenLimit":1000000,
                "supportedGenerationMethods":["generateContent"]
            }]
        });
        let models = parse_discovered_models(&payload, "gemini").unwrap();
        assert_eq!(models[0].external_id, "gemini-test");
        assert_eq!(models[0].display_name, "Gemini Test");
        assert!(models[0].capabilities.contains(&"chat".to_string()));
    }
}
