use crate::browser_session::{
    BrowserSessionError, BrowserSessionStore, STATUS_DEGRADED, STATUS_FAILED,
    STATUS_LOGIN_REQUIRED, STATUS_READY, STATUS_REQUIRES_ATTENTION, STATUS_STARTING,
    STATUS_STOPPED,
};
use chrono::{SecondsFormat, Utc};
use futures_util::SinkExt;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::{
    process::{Child, Command},
    sync::Mutex,
    time::{sleep, Instant},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Clone, Debug, Deserialize)]
pub struct ChromiumConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default = "default_startup_timeout_seconds")]
    pub startup_timeout_seconds: u64,
    #[serde(default = "default_true")]
    pub auto_recover: bool,
    #[serde(default = "default_reconcile_interval_seconds")]
    pub reconcile_interval_seconds: u64,
    #[serde(default)]
    pub sessions: BTreeMap<String, ChromiumSessionConfig>,
}

impl Default for ChromiumConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            executable: None,
            extra_args: Vec::new(),
            startup_timeout_seconds: default_startup_timeout_seconds(),
            auto_recover: true,
            reconcile_interval_seconds: default_reconcile_interval_seconds(),
            sessions: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChromiumSessionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub ready_url_prefixes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ConfigEnvelope {
    #[serde(default)]
    chromium: ChromiumConfig,
}

#[derive(Debug, Error)]
pub enum ChromiumDriverError {
    #[error("chromium driver is disabled")]
    Disabled,
    #[error("chromium driver session '{0}' is not configured")]
    SessionNotConfigured(String),
    #[error("chromium driver session '{0}' is disabled")]
    SessionDisabled(String),
    #[error("browser session error: {0}")]
    BrowserSession(#[from] BrowserSessionError),
    #[error("chromium executable was not found; set chromium.executable or install Chrome/Chromium")]
    ExecutableNotFound,
    #[error("chromium process for session '{0}' is already running")]
    AlreadyRunning(String),
    #[error("chromium process for session '{0}' is not running")]
    NotRunning(String),
    #[error("failed to launch Chromium: {0}")]
    Launch(#[source] std::io::Error),
    #[error("Chromium did not expose DevTools before the startup timeout")]
    StartupTimeout,
    #[error("invalid DevToolsActivePort file: {0}")]
    InvalidDevToolsPort(String),
    #[error("failed to query Chromium DevTools: {0}")]
    DevToolsTransport(#[source] reqwest::Error),
    #[error("invalid Chromium DevTools response: {0}")]
    DevToolsResponse(String),
    #[error("chromium driver config TOML error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("chromium driver config storage error: {0}")]
    ConfigIo(#[from] std::io::Error),
    #[error("invalid chromium driver configuration: {0}")]
    InvalidConfig(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct ChromiumLaunchView {
    pub session_id: String,
    pub login_attempt_id: String,
    pub executable: String,
    pub pid: Option<u32>,
    pub debugger_port: u16,
    pub profile_dir: String,
    pub login_url: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChromiumPageView {
    pub kind: String,
    pub location: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChromiumStatusView {
    pub session_id: String,
    pub running: bool,
    pub managed: bool,
    pub pid: Option<u32>,
    pub executable: Option<String>,
    pub started_at: Option<String>,
    pub debugger_port: Option<u16>,
    pub debugger_reachable: bool,
    pub pages: Vec<ChromiumPageView>,
    pub ready_match: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChromiumReconcileView {
    pub session_id: String,
    pub action: String,
    pub session_status: String,
    pub running: bool,
    pub ready: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChromiumReconcileSummary {
    pub checked: usize,
    pub ready: usize,
    pub recovered: usize,
    pub attention: usize,
    pub sessions: Vec<ChromiumReconcileView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChromiumVerifyView {
    pub authenticated: bool,
    pub session_id: String,
    pub ready_match: Option<String>,
    pub status: ChromiumStatusView,
}

struct ManagedProcess {
    child: Child,
    executable: String,
    started_at: String,
    pid: Option<u32>,
}

#[derive(Clone)]
pub struct ChromiumDriver {
    config: Arc<ChromiumConfig>,
    sessions: Arc<BrowserSessionStore>,
    client: Client,
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
}

#[derive(Debug, Deserialize)]
struct DevToolsTarget {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    url: String,
}

#[derive(Debug, Deserialize)]
struct DevToolsVersion {
    #[serde(rename = "webSocketDebuggerUrl", default)]
    websocket_debugger_url: String,
}

impl ChromiumConfig {
    pub fn load_from_gateway_config(path: impl AsRef<Path>) -> Result<Self, ChromiumDriverError> {
        let raw = fs::read_to_string(path)?;
        let envelope: ConfigEnvelope = toml::from_str(&raw)?;
        validate_chromium_config(&envelope.chromium)?;
        Ok(envelope.chromium)
    }
}

impl ChromiumDriver {
    pub fn new(
        config: ChromiumConfig,
        sessions: Arc<BrowserSessionStore>,
    ) -> Result<Self, ChromiumDriverError> {
        validate_chromium_config(&config)?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(3))
            .build()
            .map_err(ChromiumDriverError::DevToolsTransport)?;
        Ok(Self {
            config: Arc::new(config),
            sessions,
            client,
            processes: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn reconcile_interval_seconds(&self) -> u64 {
        self.config.reconcile_interval_seconds
    }

    pub async fn launch(&self, session_id: &str) -> Result<ChromiumLaunchView, ChromiumDriverError> {
        let driver_session = self.driver_session(session_id)?.clone();
        if !driver_session.enabled {
            return Err(ChromiumDriverError::SessionDisabled(session_id.to_string()));
        }
        let session = self.sessions.session(session_id).await?;
        if !session.enabled {
            return Err(ChromiumDriverError::SessionDisabled(session_id.to_string()));
        }

        self.remove_finished_process(session_id).await;
        let existing = self.status(session_id).await?;
        if existing.running {
            return Err(ChromiumDriverError::AlreadyRunning(session_id.to_string()));
        }

        let executable = resolve_executable(self.config.executable.as_deref())?;
        let profile_dir = PathBuf::from(&session.profile_dir);
        let devtools_file = profile_dir.join("DevToolsActivePort");
        if existing.debugger_port.is_some() && !existing.debugger_reachable {
            let _ = fs::remove_file(&devtools_file);
        }

        let login = self.sessions.begin_login(session_id).await?;
        let mut command = Command::new(&executable);
        command
            .arg(format!("--user-data-dir={}", profile_dir.display()))
            .arg("--remote-debugging-address=127.0.0.1")
            .arg("--remote-debugging-port=0")
            .arg("--no-first-run")
            .arg("--no-default-browser-check");
        for arg in &self.config.extra_args {
            command.arg(arg);
        }
        command
            .arg("--new-window")
            .arg(&login.login_url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(false);

        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = self
                    .sessions
                    .mark_failed(session_id, &format!("Chromium launch failed: {error}"))
                    .await;
                return Err(ChromiumDriverError::Launch(error));
            }
        };
        let pid = child.id();
        {
            let mut processes = self.processes.lock().await;
            processes.insert(
                session_id.to_string(),
                ManagedProcess {
                    child,
                    executable: executable.clone(),
                    started_at: now_string(),
                    pid,
                },
            );
        }

        let debugger_port = match wait_for_debugger_port(
            &devtools_file,
            Duration::from_secs(self.config.startup_timeout_seconds),
        )
        .await
        {
            Ok(port) => port,
            Err(error) => {
                let _ = self.stop(session_id).await;
                let _ = self.sessions.mark_failed(session_id, &error.to_string()).await;
                return Err(error);
            }
        };

        Ok(ChromiumLaunchView {
            session_id: session_id.to_string(),
            login_attempt_id: login.login_attempt_id,
            executable,
            pid,
            debugger_port,
            profile_dir: login.profile_dir,
            login_url: login.login_url,
        })
    }

    pub async fn status(&self, session_id: &str) -> Result<ChromiumStatusView, ChromiumDriverError> {
        self.driver_session(session_id)?;
        let session = self.sessions.session(session_id).await?;
        self.remove_finished_process(session_id).await;

        let process_meta = {
            let processes = self.processes.lock().await;
            processes.get(session_id).map(|process| {
                (
                    process.pid,
                    process.executable.clone(),
                    process.started_at.clone(),
                )
            })
        };

        let profile_dir = PathBuf::from(&session.profile_dir);
        let debugger_port = read_debugger_port(&profile_dir.join("DevToolsActivePort")).ok();
        let (debugger_reachable, pages) = if let Some(port) = debugger_port {
            match self.devtools_pages(port).await {
                Ok(pages) => (true, pages),
                Err(_) => (false, Vec::new()),
            }
        } else {
            (false, Vec::new())
        };
        let ready_match = self.match_ready_url(session_id, &pages)?;
        let managed = process_meta.is_some();

        Ok(ChromiumStatusView {
            session_id: session_id.to_string(),
            running: managed || debugger_reachable,
            managed,
            pid: process_meta.as_ref().and_then(|meta| meta.0),
            executable: process_meta.as_ref().map(|meta| meta.1.clone()),
            started_at: process_meta.as_ref().map(|meta| meta.2.clone()),
            debugger_port,
            debugger_reachable,
            pages,
            ready_match,
        })
    }

    pub async fn verify(&self, session_id: &str) -> Result<ChromiumVerifyView, ChromiumDriverError> {
        let status = self.status(session_id).await?;
        let authenticated = status.ready_match.is_some();
        let session = self.sessions.session(session_id).await?;

        if authenticated {
            self.sessions.mark_ready(session_id).await?;
        } else if status.running {
            if matches!(
                session.status.as_str(),
                STATUS_STARTING | STATUS_DEGRADED | STATUS_FAILED
            ) {
                self.sessions.mark_login_required(session_id, None).await?;
            }
        } else {
            match session.status.as_str() {
                STATUS_READY | STATUS_DEGRADED => {
                    self.sessions
                        .mark_degraded(session_id, "Chromium runtime is not reachable")
                        .await?;
                }
                STATUS_STARTING => {
                    self.sessions
                        .mark_failed(session_id, "Chromium exited before login was verified")
                        .await?;
                }
                _ => {}
            }
        }

        Ok(ChromiumVerifyView {
            authenticated,
            session_id: session_id.to_string(),
            ready_match: status.ready_match.clone(),
            status,
        })
    }

    pub async fn stop(&self, session_id: &str) -> Result<ChromiumStatusView, ChromiumDriverError> {
        self.driver_session(session_id)?;
        let before = self.status(session_id).await?;
        let mut process = {
            let mut processes = self.processes.lock().await;
            processes.remove(session_id)
        };
        if let Some(ref mut managed) = process {
            let _ = managed.child.kill().await;
            let _ = managed.child.wait().await;
        } else if before.debugger_reachable {
            if let Some(port) = before.debugger_port {
                let _ = self.close_external_browser(port).await;
                let deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < deadline {
                    if self.devtools_pages(port).await.is_err() {
                        break;
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }

        let session = self.sessions.session(session_id).await?;
        let devtools_file = PathBuf::from(&session.profile_dir).join("DevToolsActivePort");
        if devtools_file.exists() {
            let _ = fs::remove_file(&devtools_file);
        }
        self.sessions.mark_stopped(session_id).await?;
        self.status(session_id).await
    }

    pub async fn reconcile_all(&self) -> ChromiumReconcileSummary {
        if !self.enabled() {
            return ChromiumReconcileSummary {
                checked: 0,
                ready: 0,
                recovered: 0,
                attention: 0,
                sessions: Vec::new(),
            };
        }

        let session_ids = self.config.sessions.keys().cloned().collect::<Vec<_>>();
        let mut results = Vec::with_capacity(session_ids.len());
        for session_id in session_ids {
            results.push(self.reconcile_session(&session_id).await);
        }

        ChromiumReconcileSummary {
            checked: results.len(),
            ready: results.iter().filter(|item| item.ready).count(),
            recovered: results.iter().filter(|item| item.action == "recovered").count(),
            attention: results
                .iter()
                .filter(|item| {
                    matches!(
                        item.session_status.as_str(),
                        STATUS_LOGIN_REQUIRED | STATUS_REQUIRES_ATTENTION | STATUS_FAILED
                    )
                })
                .count(),
            sessions: results,
        }
    }

    async fn reconcile_session(&self, session_id: &str) -> ChromiumReconcileView {
        let session = match self.sessions.session(session_id).await {
            Ok(session) => session,
            Err(error) => {
                return ChromiumReconcileView {
                    session_id: session_id.to_string(),
                    action: "error".into(),
                    session_status: STATUS_FAILED.into(),
                    running: false,
                    ready: false,
                    error: Some(error.to_string()),
                }
            }
        };

        if !session.enabled {
            return ChromiumReconcileView {
                session_id: session_id.to_string(),
                action: "disabled".into(),
                session_status: session.status,
                running: false,
                ready: false,
                error: None,
            };
        }

        let status = match self.status(session_id).await {
            Ok(status) => status,
            Err(error) => {
                let _ = self.sessions.mark_failed(session_id, &error.to_string()).await;
                return ChromiumReconcileView {
                    session_id: session_id.to_string(),
                    action: "status_error".into(),
                    session_status: STATUS_FAILED.into(),
                    running: false,
                    ready: false,
                    error: Some(error.to_string()),
                };
            }
        };

        if status.running {
            if status.ready_match.is_some() {
                let action = if session.status == STATUS_READY {
                    "verified"
                } else {
                    "reconnected"
                };
                let current = self
                    .sessions
                    .mark_ready(session_id)
                    .await
                    .unwrap_or(session.clone());
                return ChromiumReconcileView {
                    session_id: session_id.to_string(),
                    action: action.into(),
                    session_status: current.status,
                    running: true,
                    ready: true,
                    error: None,
                };
            }

            let current = if session.status == STATUS_READY {
                self.sessions
                    .mark_degraded(session_id, "Authenticated provider page is not visible")
                    .await
                    .unwrap_or(session.clone())
            } else if matches!(
                session.status.as_str(),
                STATUS_STARTING | STATUS_DEGRADED | STATUS_FAILED
            ) {
                self.sessions
                    .mark_login_required(session_id, None)
                    .await
                    .unwrap_or(session.clone())
            } else {
                session.clone()
            };
            return ChromiumReconcileView {
                session_id: session_id.to_string(),
                action: "login_required".into(),
                session_status: current.status,
                running: true,
                ready: false,
                error: current.last_error,
            };
        }

        if status.debugger_port.is_some() && !status.debugger_reachable {
            let devtools_file = PathBuf::from(&session.profile_dir).join("DevToolsActivePort");
            let _ = fs::remove_file(devtools_file);
        }

        if matches!(
            session.status.as_str(),
            STATUS_STOPPED | STATUS_LOGIN_REQUIRED | STATUS_REQUIRES_ATTENTION
        ) {
            return ChromiumReconcileView {
                session_id: session_id.to_string(),
                action: "idle".into(),
                session_status: session.status,
                running: false,
                ready: false,
                error: session.last_error,
            };
        }

        if !self.config.auto_recover {
            let current = self
                .sessions
                .mark_degraded(session_id, "Chromium runtime is not reachable")
                .await
                .unwrap_or(session.clone());
            return ChromiumReconcileView {
                session_id: session_id.to_string(),
                action: "recovery_disabled".into(),
                session_status: current.status,
                running: false,
                ready: false,
                error: current.last_error,
            };
        }

        if !matches!(
            session.status.as_str(),
            STATUS_READY | STATUS_DEGRADED | STATUS_STARTING
        ) {
            return ChromiumReconcileView {
                session_id: session_id.to_string(),
                action: "idle".into(),
                session_status: session.status,
                running: false,
                ready: false,
                error: session.last_error,
            };
        }

        if let Err(error) = self.launch(session_id).await {
            let current = self
                .sessions
                .mark_failed(session_id, &format!("Automatic browser recovery failed: {error}"))
                .await
                .unwrap_or(session.clone());
            return ChromiumReconcileView {
                session_id: session_id.to_string(),
                action: "recovery_failed".into(),
                session_status: current.status,
                running: false,
                ready: false,
                error: Some(error.to_string()),
            };
        }

        let wait = Duration::from_secs(self.config.startup_timeout_seconds.min(5).max(1));
        let deadline = Instant::now() + wait;
        loop {
            match self.verify(session_id).await {
                Ok(verification) if verification.authenticated => {
                    let current = self
                        .sessions
                        .session(session_id)
                        .await
                        .unwrap_or(session.clone());
                    return ChromiumReconcileView {
                        session_id: session_id.to_string(),
                        action: "recovered".into(),
                        session_status: current.status,
                        running: true,
                        ready: true,
                        error: None,
                    };
                }
                Ok(_) => {}
                Err(error) => {
                    let current = self
                        .sessions
                        .mark_failed(session_id, &format!("Automatic verification failed: {error}"))
                        .await
                        .unwrap_or(session.clone());
                    return ChromiumReconcileView {
                        session_id: session_id.to_string(),
                        action: "recovery_failed".into(),
                        session_status: current.status,
                        running: false,
                        ready: false,
                        error: Some(error.to_string()),
                    };
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            sleep(Duration::from_millis(250)).await;
        }

        let current = self
            .sessions
            .mark_login_required(session_id, None)
            .await
            .unwrap_or(session);
        ChromiumReconcileView {
            session_id: session_id.to_string(),
            action: "login_required".into(),
            session_status: current.status,
            running: true,
            ready: false,
            error: current.last_error,
        }
    }

    fn driver_session(&self, session_id: &str) -> Result<&ChromiumSessionConfig, ChromiumDriverError> {
        if !self.enabled() {
            return Err(ChromiumDriverError::Disabled);
        }
        self.config
            .sessions
            .get(session_id)
            .ok_or_else(|| ChromiumDriverError::SessionNotConfigured(session_id.to_string()))
    }

    fn match_ready_url(
        &self,
        session_id: &str,
        pages: &[ChromiumPageView],
    ) -> Result<Option<String>, ChromiumDriverError> {
        let config = self.driver_session(session_id)?;
        for page in pages {
            for prefix in &config.ready_url_prefixes {
                if page.location.starts_with(prefix) {
                    return Ok(Some(page.location.clone()));
                }
            }
        }
        Ok(None)
    }

    async fn devtools_pages(&self, port: u16) -> Result<Vec<ChromiumPageView>, ChromiumDriverError> {
        let response = self
            .client
            .get(format!("http://127.0.0.1:{port}/json/list"))
            .send()
            .await
            .map_err(ChromiumDriverError::DevToolsTransport)?;
        if !response.status().is_success() {
            return Err(ChromiumDriverError::DevToolsResponse(format!(
                "HTTP {}",
                response.status()
            )));
        }
        let targets: Vec<DevToolsTarget> = response
            .json()
            .await
            .map_err(|error| ChromiumDriverError::DevToolsResponse(error.to_string()))?;
        Ok(targets
            .into_iter()
            .filter(|target| target.kind == "page" && !target.url.is_empty())
            .map(|target| ChromiumPageView {
                kind: target.kind,
                location: sanitize_url(&target.url),
            })
            .collect())
    }

    async fn close_external_browser(&self, port: u16) -> Result<(), ChromiumDriverError> {
        let response = self
            .client
            .get(format!("http://127.0.0.1:{port}/json/version"))
            .send()
            .await
            .map_err(ChromiumDriverError::DevToolsTransport)?;
        if !response.status().is_success() {
            return Err(ChromiumDriverError::DevToolsResponse(format!(
                "DevTools version returned HTTP {}",
                response.status()
            )));
        }
        let version = response
            .json::<DevToolsVersion>()
            .await
            .map_err(|error| ChromiumDriverError::DevToolsResponse(error.to_string()))?;
        if version.websocket_debugger_url.trim().is_empty() {
            return Err(ChromiumDriverError::DevToolsResponse(
                "DevTools version did not expose a browser websocket".into(),
            ));
        }

        let (mut socket, _) = connect_async(&version.websocket_debugger_url)
            .await
            .map_err(|error| ChromiumDriverError::DevToolsResponse(error.to_string()))?;
        socket
            .send(Message::Text(
                r#"{"id":1,"method":"Browser.close"}"#.to_string().into(),
            ))
            .await
            .map_err(|error| ChromiumDriverError::DevToolsResponse(error.to_string()))?;
        let _ = socket.close(None).await;
        Ok(())
    }

    async fn remove_finished_process(&self, session_id: &str) {
        let mut processes = self.processes.lock().await;
        let finished = processes
            .get_mut(session_id)
            .and_then(|process| process.child.try_wait().ok().flatten())
            .is_some();
        if finished {
            processes.remove(session_id);
        }
    }
}

fn validate_chromium_config(config: &ChromiumConfig) -> Result<(), ChromiumDriverError> {
    if config.startup_timeout_seconds == 0 || config.startup_timeout_seconds > 120 {
        return Err(ChromiumDriverError::InvalidConfig(
            "chromium.startup_timeout_seconds must be between 1 and 120".into(),
        ));
    }
    if config.reconcile_interval_seconds < 5 || config.reconcile_interval_seconds > 3600 {
        return Err(ChromiumDriverError::InvalidConfig(
            "chromium.reconcile_interval_seconds must be between 5 and 3600".into(),
        ));
    }
    for (session_id, session) in &config.sessions {
        if session_id.is_empty()
            || !session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(ChromiumDriverError::InvalidConfig(format!(
                "chromium session id '{session_id}' is invalid"
            )));
        }
        for prefix in &session.ready_url_prefixes {
            validate_ready_prefix(session_id, prefix)?;
        }
    }
    Ok(())
}

fn validate_ready_prefix(session_id: &str, prefix: &str) -> Result<(), ChromiumDriverError> {
    let url = Url::parse(prefix).map_err(|error| {
        ChromiumDriverError::InvalidConfig(format!(
            "chromium session '{session_id}' has invalid ready_url_prefix '{prefix}': {error}"
        ))
    })?;
    let local_http = url.scheme() == "http"
        && matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"));
    if url.scheme() != "https" && !local_http {
        return Err(ChromiumDriverError::InvalidConfig(format!(
            "chromium session '{session_id}' ready_url_prefix must use HTTPS (localhost HTTP is allowed for tests)"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ChromiumDriverError::InvalidConfig(format!(
            "chromium session '{session_id}' ready_url_prefix must not contain credentials"
        )));
    }
    Ok(())
}

async fn wait_for_debugger_port(
    path: &Path,
    timeout: Duration,
) -> Result<u16, ChromiumDriverError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(port) = read_debugger_port(path) {
            return Ok(port);
        }
        if Instant::now() >= deadline {
            return Err(ChromiumDriverError::StartupTimeout);
        }
        sleep(Duration::from_millis(100)).await;
    }
}

fn read_debugger_port(path: &Path) -> Result<u16, ChromiumDriverError> {
    let raw = fs::read_to_string(path).map_err(|error| {
        ChromiumDriverError::InvalidDevToolsPort(format!("{}: {error}", path.display()))
    })?;
    let first = raw
        .lines()
        .next()
        .ok_or_else(|| ChromiumDriverError::InvalidDevToolsPort("missing port".into()))?;
    first
        .parse::<u16>()
        .map_err(|error| ChromiumDriverError::InvalidDevToolsPort(error.to_string()))
}

fn resolve_executable(configured: Option<&str>) -> Result<String, ChromiumDriverError> {
    if let Some(configured) = configured.map(str::trim).filter(|value| !value.is_empty()) {
        if let Some(path) = find_executable(configured) {
            return Ok(path.display().to_string());
        }
        return Err(ChromiumDriverError::ExecutableNotFound);
    }

    #[cfg(target_os = "macos")]
    let candidates = vec![
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ];
    #[cfg(not(target_os = "macos"))]
    let candidates = vec![
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
    ];
    for candidate in candidates {
        if let Some(path) = find_executable(candidate) {
            return Ok(path.display().to_string());
        }
    }

    #[cfg(target_os = "windows")]
    {
        for root in [env::var_os("PROGRAMFILES"), env::var_os("PROGRAMFILES(X86)"), env::var_os("LOCALAPPDATA")]
            .into_iter()
            .flatten()
        {
            for suffix in [
                "Google/Chrome/Application/chrome.exe",
                "Chromium/Application/chrome.exe",
            ] {
                let path = PathBuf::from(&root).join(suffix);
                if path.is_file() {
                    return Ok(path.display().to_string());
                }
            }
        }
    }

    Err(ChromiumDriverError::ExecutableNotFound)
}

fn find_executable(candidate: &str) -> Option<PathBuf> {
    let path = Path::new(candidate);
    if path.components().count() > 1 || path.is_absolute() {
        return is_executable_file(path).then(|| path.to_path_buf());
    }
    let path_env = env::var_os("PATH")?;
    for directory in env::split_paths(&path_env) {
        let path = directory.join(candidate);
        if is_executable_file(&path) {
            return Some(path);
        }
        #[cfg(target_os = "windows")]
        {
            let exe = directory.join(format!("{candidate}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return metadata.permissions().mode() & 0o111 != 0;
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn sanitize_url(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else {
        return "<unparseable>".into();
    };
    if !matches!(url.scheme(), "http" | "https") {
        return format!("{}:", url.scheme());
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

fn now_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn default_true() -> bool {
    true
}

fn default_startup_timeout_seconds() -> u64 {
    15
}

fn default_reconcile_interval_seconds() -> u64 {
    15
}

#[cfg(test)]
mod tests {
    use super::{read_debugger_port, sanitize_url, validate_chromium_config, ChromiumConfig, ChromiumSessionConfig};
    use std::{collections::BTreeMap, fs};

    #[test]
    fn chromium_driver_is_opt_in_disabled() {
        let config = ChromiumConfig::default();
        assert!(!config.enabled);
        assert!(config.auto_recover);
        assert_eq!(config.reconcile_interval_seconds, 15);
    }

    #[test]
    fn sanitizes_query_and_fragment_from_observed_urls() {
        assert_eq!(
            sanitize_url("https://example.com/app?token=secret#fragment"),
            "https://example.com/app"
        );
    }

    #[test]
    fn parses_devtools_active_port() {
        let path = std::env::temp_dir().join(format!("llmgateway-devtools-{}", std::process::id()));
        fs::write(&path, "9222\n/devtools/browser/test\n").unwrap();
        assert_eq!(read_debugger_port(&path).unwrap(), 9222);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_insecure_ready_url_prefixes() {
        let mut sessions = BTreeMap::new();
        sessions.insert(
            "gemini-web".into(),
            ChromiumSessionConfig {
                enabled: true,
                ready_url_prefixes: vec!["http://example.com/app".into()],
            },
        );
        let config = ChromiumConfig {
            enabled: true,
            executable: None,
            extra_args: vec![],
            startup_timeout_seconds: 10,
            auto_recover: true,
            reconcile_interval_seconds: 15,
            sessions,
        };
        assert!(validate_chromium_config(&config).is_err());
    }
}
