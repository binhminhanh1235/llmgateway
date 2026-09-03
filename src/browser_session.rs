use crate::config::AppConfig;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Row, SqlitePool,
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize)]
pub struct BrowserConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_profile_root")]
    pub profile_root: String,
    #[serde(default)]
    pub sessions: BTreeMap<String, BrowserSessionSpec>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            profile_root: default_profile_root(),
            sessions: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct BrowserSessionSpec {
    pub provider: String,
    pub login_url: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ConfigEnvelope {
    #[serde(default)]
    browser: BrowserConfig,
}

#[derive(Clone)]
pub struct BrowserSessionStore {
    browser_config: Arc<BrowserConfig>,
    pool: SqlitePool,
}

#[derive(Debug, Error)]
pub enum BrowserSessionError {
    #[error("browser session database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("browser session storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("browser session config TOML error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid browser session configuration: {0}")]
    InvalidConfig(String),
    #[error("browser session '{0}' was not found")]
    NotFound(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserSessionView {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub enabled: bool,
    pub status: String,
    pub login_url: String,
    pub profile_dir: String,
    pub login_attempt_id: Option<String>,
    pub login_started_at: Option<String>,
    pub last_ready_at: Option<String>,
    pub last_verified_at: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserSessionSummary {
    pub enabled: bool,
    pub profile_root: String,
    pub sessions: Vec<BrowserSessionView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserLoginStart {
    pub session: BrowserSessionView,
    pub login_attempt_id: String,
    pub login_url: String,
    pub profile_dir: String,
    pub instructions: Vec<String>,
}

impl BrowserConfig {
    pub fn load_from_gateway_config(path: impl AsRef<Path>) -> Result<Self, BrowserSessionError> {
        let raw = fs::read_to_string(path)?;
        let envelope: ConfigEnvelope = toml::from_str(&raw)?;
        validate_browser_config(&envelope.browser)?;
        Ok(envelope.browser)
    }
}

impl BrowserSessionStore {
    pub async fn connect(
        app_config: Arc<AppConfig>,
        browser_config: BrowserConfig,
    ) -> Result<Self, BrowserSessionError> {
        ensure_sqlite_parent(&app_config.storage.database_url)?;
        if browser_config.enabled || !browser_config.sessions.is_empty() {
            ensure_private_dir(Path::new(&browser_config.profile_root))?;
        }
        let options = SqliteConnectOptions::from_str(&app_config.storage.database_url)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        let store = Self {
            browser_config: Arc::new(browser_config),
            pool,
        };
        store.migrate().await?;
        store.seed().await?;
        Ok(store)
    }

    pub fn enabled(&self) -> bool {
        self.browser_config.enabled
    }

    pub async fn summary(&self) -> Result<BrowserSessionSummary, BrowserSessionError> {
        let mut sessions = Vec::new();
        for id in self.browser_config.sessions.keys() {
            sessions.push(self.session(id).await?);
        }
        Ok(BrowserSessionSummary {
            enabled: self.enabled(),
            profile_root: self.browser_config.profile_root.clone(),
            sessions,
        })
    }

    pub async fn session(&self, id: &str) -> Result<BrowserSessionView, BrowserSessionError> {
        let spec = self.spec(id)?;
        let row = sqlx::query(
            "SELECT status, login_attempt_id, login_started_at, last_ready_at,
                    last_verified_at, last_error, updated_at
             FROM browser_session_state WHERE session_id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        let (status, login_attempt_id, login_started_at, last_ready_at, last_verified_at, last_error, updated_at) =
            if let Some(row) = row {
                (
                    row.try_get::<String, _>("status")?,
                    row.try_get("login_attempt_id")?,
                    row.try_get("login_started_at")?,
                    row.try_get("last_ready_at")?,
                    row.try_get("last_verified_at")?,
                    row.try_get("last_error")?,
                    row.try_get("updated_at")?,
                )
            } else {
                ("requires_login".into(), None, None, None, None, None, None)
            };

        Ok(BrowserSessionView {
            id: id.to_string(),
            provider: spec.provider.clone(),
            label: spec.label.clone().unwrap_or_else(|| id.to_string()),
            enabled: self.enabled() && spec.enabled,
            status,
            login_url: spec.login_url.clone(),
            profile_dir: self.profile_dir(id).display().to_string(),
            login_attempt_id,
            login_started_at,
            last_ready_at,
            last_verified_at,
            last_error,
            updated_at,
        })
    }

    pub async fn begin_login(&self, id: &str) -> Result<BrowserLoginStart, BrowserSessionError> {
        let spec = self.spec(id)?;
        if !self.enabled() || !spec.enabled {
            return Err(BrowserSessionError::InvalidConfig(format!(
                "browser session '{id}' is disabled"
            )));
        }
        let profile_dir = self.profile_dir(id);
        ensure_private_dir(&profile_dir)?;
        let attempt_id = format!("browser_login_{}", Uuid::new_v4());
        let now = now_string();
        sqlx::query(
            "UPDATE browser_session_state
             SET status = 'login_in_progress', login_attempt_id = ?, login_started_at = ?,
                 last_error = NULL, updated_at = ?
             WHERE session_id = ?",
        )
        .bind(&attempt_id)
        .bind(&now)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;

        let session = self.session(id).await?;
        Ok(BrowserLoginStart {
            login_attempt_id: attempt_id,
            login_url: spec.login_url.clone(),
            profile_dir: profile_dir.display().to_string(),
            session,
            instructions: vec![
                "Open the login URL in a Chromium browser using this isolated profile directory.".into(),
                "Complete login, CAPTCHA, and 2FA normally in the browser if requested.".into(),
                "Never copy raw cookies into llmgateway; the browser profile owns session secrets.".into(),
                "Mark the session ready only after the normal login has completed successfully.".into(),
            ],
        })
    }

    pub async fn mark_ready(&self, id: &str) -> Result<BrowserSessionView, BrowserSessionError> {
        self.spec(id)?;
        let now = now_string();
        sqlx::query(
            "UPDATE browser_session_state
             SET status = 'ready', login_attempt_id = NULL, login_started_at = NULL,
                 last_ready_at = ?, last_verified_at = ?, last_error = NULL, updated_at = ?
             WHERE session_id = ?",
        )
        .bind(&now)
        .bind(&now)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.session(id).await
    }

    pub async fn mark_verified(&self, id: &str) -> Result<BrowserSessionView, BrowserSessionError> {
        self.spec(id)?;
        let now = now_string();
        sqlx::query(
            "UPDATE browser_session_state SET last_verified_at = ?, updated_at = ? WHERE session_id = ?",
        )
        .bind(&now)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.session(id).await
    }

    pub async fn require_attention(
        &self,
        id: &str,
        error: &str,
    ) -> Result<BrowserSessionView, BrowserSessionError> {
        self.spec(id)?;
        let now = now_string();
        sqlx::query(
            "UPDATE browser_session_state
             SET status = 'requires_attention', login_attempt_id = NULL, login_started_at = NULL,
                 last_error = ?, updated_at = ? WHERE session_id = ?",
        )
        .bind(error)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.session(id).await
    }

    pub async fn reset(&self, id: &str) -> Result<BrowserSessionView, BrowserSessionError> {
        self.spec(id)?;
        let now = now_string();
        sqlx::query(
            "UPDATE browser_session_state
             SET status = 'requires_login', login_attempt_id = NULL, login_started_at = NULL,
                 last_error = NULL, updated_at = ? WHERE session_id = ?",
        )
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.session(id).await
    }

    fn spec(&self, id: &str) -> Result<&BrowserSessionSpec, BrowserSessionError> {
        self.browser_config
            .sessions
            .get(id)
            .ok_or_else(|| BrowserSessionError::NotFound(id.to_string()))
    }

    fn profile_dir(&self, id: &str) -> PathBuf {
        Path::new(&self.browser_config.profile_root).join(id)
    }

    async fn migrate(&self) -> Result<(), BrowserSessionError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS browser_session_state (
                session_id TEXT PRIMARY KEY,
                provider_id TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'requires_login',
                login_attempt_id TEXT,
                login_started_at TEXT,
                last_ready_at TEXT,
                last_verified_at TEXT,
                last_error TEXT,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn seed(&self) -> Result<(), BrowserSessionError> {
        for (id, spec) in &self.browser_config.sessions {
            sqlx::query(
                "INSERT INTO browser_session_state (session_id, provider_id, status, updated_at)
                 VALUES (?, ?, 'requires_login', ?)
                 ON CONFLICT(session_id) DO UPDATE SET provider_id = excluded.provider_id",
            )
            .bind(id)
            .bind(&spec.provider)
            .bind(now_string())
            .execute(&self.pool)
            .await?;
            if self.browser_config.enabled && spec.enabled {
                ensure_private_dir(&self.profile_dir(id))?;
            }
        }
        Ok(())
    }
}

fn validate_browser_config(config: &BrowserConfig) -> Result<(), BrowserSessionError> {
    if config.profile_root.trim().is_empty() {
        return Err(BrowserSessionError::InvalidConfig(
            "browser.profile_root must not be empty".into(),
        ));
    }
    for (id, spec) in &config.sessions {
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(BrowserSessionError::InvalidConfig(format!(
                "browser session id '{id}' may contain only letters, numbers, '.', '-' and '_'"
            )));
        }
        if spec.provider.trim().is_empty() {
            return Err(BrowserSessionError::InvalidConfig(format!(
                "browser session '{id}' requires provider"
            )));
        }
        if !(spec.login_url.starts_with("https://")
            || spec.login_url.starts_with("http://127.0.0.1")
            || spec.login_url.starts_with("http://localhost"))
        {
            return Err(BrowserSessionError::InvalidConfig(format!(
                "browser session '{id}' login_url must use HTTPS (localhost is allowed for tests)"
            )));
        }
    }
    Ok(())
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

fn ensure_private_dir(path: &Path) -> Result<(), std::io::Error> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn now_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn default_true() -> bool {
    true
}

fn default_profile_root() -> String {
    "data/browser-profiles".into()
}

#[cfg(test)]
mod tests {
    use super::{validate_browser_config, BrowserConfig, BrowserSessionSpec};
    use std::collections::BTreeMap;

    #[test]
    fn missing_browser_section_is_opt_in_disabled() {
        assert!(!BrowserConfig::default().enabled);
    }

    #[test]
    fn rejects_session_ids_that_can_escape_profile_root() {
        let mut sessions = BTreeMap::new();
        sessions.insert(
            "../bad".into(),
            BrowserSessionSpec {
                provider: "gemini".into(),
                login_url: "https://gemini.google.com/app".into(),
                enabled: true,
                label: None,
            },
        );
        let config = BrowserConfig {
            enabled: true,
            profile_root: "data/browser-profiles".into(),
            sessions,
        };
        assert!(validate_browser_config(&config).is_err());
    }
}
