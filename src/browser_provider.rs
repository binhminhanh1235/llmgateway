use crate::{
    browser_session_runtime, chromium_driver_runtime,
    config::{AccountConfig, ProviderConfig, RouteConfig},
};
use async_trait::async_trait;
use axum::http::Response as HttpResponse;
use futures_util::{SinkExt, StreamExt};
use reqwest::{header::CONTENT_TYPE, Client};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{sync::RwLock, time::timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const MAX_ADAPTER_SCRIPT_BYTES: u64 = 512 * 1024;
const CDP_EXECUTION_TIMEOUT_SECONDS: u64 = 600;
pub const BROWSER_ADAPTER_CONTRACT_VERSION: u32 = 1;
const ADAPTER_HEALTH_TTL_SECONDS: u64 = 30;
const GEMINI_WEB_ADAPTER: &str = include_str!("../adapters/gemini-web.js");
const QWEN_WEB_ADAPTER: &str = include_str!("../adapters/qwen-web.js");

#[derive(Clone, Debug, Default, Deserialize)]
pub struct BrowserProviderConfig {
    #[serde(default)]
    pub bindings: BTreeMap<String, BrowserAccountBinding>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BrowserAccountBinding {
    pub session: String,
    #[serde(default)]
    pub target_url_prefix: Option<String>,
    #[serde(default)]
    pub adapter_script: Option<String>,
    #[serde(default)]
    pub adapter_contract_version: Option<u32>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub model_labels: BTreeMap<String, String>,
    #[serde(default)]
    pub selector_overrides: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub ephemeral_chat: Option<bool>,
    #[serde(default)]
    pub probe_timeout_ms: Option<u64>,
    #[serde(default)]
    pub response_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserAdapterDiagnostics {
    pub account_id: String,
    pub provider_kind: String,
    pub adapter_id: Option<String>,
    pub adapter_version: Option<String>,
    pub contract_version: Option<u32>,
    pub expected_contract_version: u32,
    pub status: String,
    pub message: String,
    pub page_signature: Option<String>,
    pub target_url_prefix: Option<String>,
    pub configured_models: Vec<String>,
}

#[derive(Clone, Debug)]
struct CachedAdapterDiagnostics {
    checked_at: Instant,
    diagnostics: BrowserAdapterDiagnostics,
}

#[derive(Clone, Debug, Deserialize)]
struct AdapterMeta {
    contract_version: u32,
    id: String,
    provider: String,
    #[serde(default)]
    adapter_version: String,
}

#[derive(Clone, Debug, Deserialize)]
struct AdapterProbeResult {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    page_signature: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct AdapterContractError {
    code: String,
    message: String,
}

#[derive(Clone, Debug, Deserialize)]
struct AdapterContractEnvelope {
    #[serde(default)]
    meta: Option<AdapterMeta>,
    #[serde(default)]
    probe: Option<AdapterProbeResult>,
    #[serde(default)]
    result: Option<CdpAdapterResult>,
    #[serde(default)]
    error: Option<AdapterContractError>,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigEnvelope {
    #[serde(default)]
    browser: BrowserProviderConfig,
}

#[derive(Clone, Debug)]
pub struct BrowserAdapterRequest {
    pub provider: ProviderConfig,
    pub account: AccountConfig,
    pub route: RouteConfig,
    pub body: Value,
    pub session_id: String,
    pub profile_dir: String,
    pub binding: BrowserAccountBinding,
}

#[derive(Debug, Error)]
pub enum BrowserProviderError {
    #[error("browser provider config error: {0}")]
    InvalidConfig(String),
    #[error("browser provider adapter '{0}' is not registered")]
    UnsupportedAdapter(String),
    #[error("browser account '{0}' has no [browser.bindings] entry")]
    MissingBinding(String),
    #[error("browser session '{session_id}' for account '{account_id}' is not ready")]
    SessionUnavailable {
        account_id: String,
        session_id: String,
    },
    #[error("browser adapter incompatible for account '{account_id}' ({code}): {message}")]
    AdapterIncompatible {
        account_id: String,
        code: String,
        message: String,
    },
    #[error("browser model '{model}' is not available for account '{account_id}'")]
    ModelUnavailable { account_id: String, model: String },
    #[error("browser provider transport error: {0}")]
    Transport(String),
    #[error("browser provider config read error: {0}")]
    Io(#[from] std::io::Error),
    #[error("browser provider config TOML error: {0}")]
    Toml(#[from] toml::de::Error),
}

#[async_trait]
pub trait BrowserProviderAdapter: Send + Sync {
    fn kind(&self) -> &'static str;
    fn adapter_id(&self) -> &'static str;
    fn is_cdp(&self) -> bool {
        false
    }

    async fn diagnose(
        &self,
        account_id: &str,
        profile_dir: &str,
        binding: &BrowserAccountBinding,
    ) -> BrowserAdapterDiagnostics;

    async fn execute_chat(
        &self,
        request: BrowserAdapterRequest,
    ) -> Result<reqwest::Response, BrowserProviderError>;
}

pub struct BrowserProviderRegistry {
    config: Arc<BrowserProviderConfig>,
    adapters: BTreeMap<String, Arc<dyn BrowserProviderAdapter>>,
    adapter_health: Arc<RwLock<BTreeMap<String, CachedAdapterDiagnostics>>>,
}

impl BrowserProviderConfig {
    pub fn load_from_gateway_config(path: impl AsRef<Path>) -> Result<Self, BrowserProviderError> {
        let raw = fs::read_to_string(path)?;
        let envelope: ConfigEnvelope = toml::from_str(&raw)?;
        for (account, binding) in &envelope.browser.bindings {
            if account.trim().is_empty() {
                return Err(BrowserProviderError::InvalidConfig(
                    "browser binding account id cannot be empty".into(),
                ));
            }
            if binding.session.trim().is_empty() {
                return Err(BrowserProviderError::InvalidConfig(format!(
                    "browser binding '{account}' must name a session"
                )));
            }
            if binding
                .target_url_prefix
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(BrowserProviderError::InvalidConfig(format!(
                    "browser binding '{account}' target_url_prefix cannot be empty"
                )));
            }
            if binding
                .adapter_script
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(BrowserProviderError::InvalidConfig(format!(
                    "browser binding '{account}' adapter_script cannot be empty"
                )));
            }
            if binding
                .adapter_contract_version
                .is_some_and(|value| value != BROWSER_ADAPTER_CONTRACT_VERSION)
            {
                return Err(BrowserProviderError::InvalidConfig(format!(
                    "browser binding '{account}' adapter_contract_version must be {}",
                    BROWSER_ADAPTER_CONTRACT_VERSION
                )));
            }
            if binding.models.iter().any(|model| model.trim().is_empty()) {
                return Err(BrowserProviderError::InvalidConfig(format!(
                    "browser binding '{account}' models cannot contain empty values"
                )));
            }
            for (model, label) in &binding.model_labels {
                if model.trim().is_empty() || label.trim().is_empty() {
                    return Err(BrowserProviderError::InvalidConfig(format!(
                        "browser binding '{account}' model_labels keys and values cannot be empty"
                    )));
                }
            }
            for (group, selectors) in &binding.selector_overrides {
                if group.trim().is_empty()
                    || selectors.is_empty()
                    || selectors.iter().any(|selector| selector.trim().is_empty())
                {
                    return Err(BrowserProviderError::InvalidConfig(format!(
                        "browser binding '{account}' selector_overrides must contain non-empty groups and selectors"
                    )));
                }
            }
            if binding
                .probe_timeout_ms
                .is_some_and(|value| !(500..=60_000).contains(&value))
            {
                return Err(BrowserProviderError::InvalidConfig(format!(
                    "browser binding '{account}' probe_timeout_ms must be between 500 and 60000"
                )));
            }
            if binding
                .response_timeout_ms
                .is_some_and(|value| !(1_000..=600_000).contains(&value))
            {
                return Err(BrowserProviderError::InvalidConfig(format!(
                    "browser binding '{account}' response_timeout_ms must be between 1000 and 600000"
                )));
            }
        }
        Ok(envelope.browser)
    }
}

impl BrowserProviderRegistry {
    pub fn new(config: BrowserProviderConfig) -> Result<Self, BrowserProviderError> {
        let http = Arc::new(HttpBrowserAdapter::new()?);
        let cdp = Arc::new(CdpBrowserAdapter::custom()?);
        let gemini = Arc::new(CdpBrowserAdapter::gemini()?);
        let qwen = Arc::new(CdpBrowserAdapter::qwen()?);
        let mut adapters: BTreeMap<String, Arc<dyn BrowserProviderAdapter>> = BTreeMap::new();
        adapters.insert(http.kind().to_string(), http);
        adapters.insert(cdp.kind().to_string(), cdp);
        adapters.insert(gemini.kind().to_string(), gemini);
        adapters.insert(qwen.kind().to_string(), qwen);
        Ok(Self {
            config: Arc::new(config),
            adapters,
            adapter_health: Arc::new(RwLock::new(BTreeMap::new())),
        })
    }

    pub fn binding_count(&self) -> usize {
        self.config.bindings.len()
    }

    pub fn is_browser_kind(kind: &str) -> bool {
        kind.starts_with("browser-")
    }

    pub fn supports(&self, kind: &str) -> bool {
        self.adapters.contains_key(kind)
    }

    pub fn session_id_for_account(&self, account_id: &str) -> Option<&str> {
        self.config
            .bindings
            .get(account_id)
            .map(|binding| binding.session.as_str())
    }

    pub async fn adapter_diagnostics(
        &self,
        provider_kind: &str,
        account_id: &str,
    ) -> BrowserAdapterDiagnostics {
        if let Some(cached) = self.adapter_health.read().await.get(account_id).cloned() {
            if cached.checked_at.elapsed() < Duration::from_secs(ADAPTER_HEALTH_TTL_SECONDS) {
                return cached.diagnostics;
            }
        }

        let Some(adapter) = self.adapters.get(provider_kind) else {
            return self
                .cache_diagnostics(BrowserAdapterDiagnostics {
                    account_id: account_id.to_string(),
                    provider_kind: provider_kind.to_string(),
                    adapter_id: None,
                    adapter_version: None,
                    contract_version: None,
                    expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                    status: "unsupported".into(),
                    message: format!("browser adapter kind '{provider_kind}' is not registered"),
                    page_signature: None,
                    target_url_prefix: None,
                    configured_models: Vec::new(),
                })
                .await;
        };
        let Some(binding) = self.config.bindings.get(account_id) else {
            return self
                .cache_diagnostics(BrowserAdapterDiagnostics {
                    account_id: account_id.to_string(),
                    provider_kind: provider_kind.to_string(),
                    adapter_id: Some(adapter.adapter_id().to_string()),
                    adapter_version: None,
                    contract_version: None,
                    expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                    status: "unconfigured".into(),
                    message: "browser account has no binding".into(),
                    page_signature: None,
                    target_url_prefix: None,
                    configured_models: Vec::new(),
                })
                .await;
        };
        let Some(store) = browser_session_runtime::get() else {
            return self
                .cache_diagnostics(unavailable_diagnostics(
                    account_id,
                    provider_kind,
                    adapter.adapter_id(),
                    binding,
                    "browser session runtime is not initialized",
                ))
                .await;
        };
        let session = match store.session(&binding.session).await {
            Ok(session) => session,
            Err(error) => {
                return self
                    .cache_diagnostics(unavailable_diagnostics(
                        account_id,
                        provider_kind,
                        adapter.adapter_id(),
                        binding,
                        &error.to_string(),
                    ))
                    .await;
            }
        };
        if !session.enabled || session.status != "ready" {
            return self
                .cache_diagnostics(unavailable_diagnostics(
                    account_id,
                    provider_kind,
                    adapter.adapter_id(),
                    binding,
                    &format!("browser session is {}", session.status),
                ))
                .await;
        }

        let diagnostics = adapter
            .diagnose(account_id, &session.profile_dir, binding)
            .await;
        self.cache_diagnostics(diagnostics).await
    }

    async fn cache_diagnostics(
        &self,
        diagnostics: BrowserAdapterDiagnostics,
    ) -> BrowserAdapterDiagnostics {
        self.adapter_health.write().await.insert(
            diagnostics.account_id.clone(),
            CachedAdapterDiagnostics {
                checked_at: Instant::now(),
                diagnostics: diagnostics.clone(),
            },
        );
        diagnostics
    }

    async fn invalidate_diagnostics(&self, account_id: &str) {
        self.adapter_health.write().await.remove(account_id);
    }

    pub async fn route_available(&self, provider_kind: &str, account_id: &str) -> bool {
        if !Self::is_browser_kind(provider_kind) || !self.supports(provider_kind) {
            return false;
        }
        let Some(binding) = self.config.bindings.get(account_id) else {
            return false;
        };
        if provider_kind == "browser-cdp" && binding.adapter_script.is_none() {
            return false;
        }
        let Some(store) = browser_session_runtime::get() else {
            return false;
        };
        let session_ready = match store.session(&binding.session).await {
            Ok(session) => session.enabled && session.status == "ready",
            Err(_) => false,
        };
        if !session_ready {
            return false;
        }

        let adapter = match self.adapters.get(provider_kind) {
            Some(adapter) => adapter,
            None => return false,
        };
        if adapter.is_cdp() {
            let Some(driver) = chromium_driver_runtime::get() else {
                return false;
            };
            let live = driver
                .status(&binding.session)
                .await
                .is_ok_and(|status| {
                    status.running
                        && status.debugger_reachable
                        && status.ready_match.is_some()
                });
            if !live {
                return false;
            }
            let diagnostics = self.adapter_diagnostics(provider_kind, account_id).await;
            return diagnostics.status == "ready";
        }

        true
    }

    pub async fn mark_degraded(
        &self,
        account_id: &str,
        error: &str,
    ) -> Result<(), BrowserProviderError> {
        self.invalidate_diagnostics(account_id).await;
        let binding = self
            .config
            .bindings
            .get(account_id)
            .ok_or_else(|| BrowserProviderError::MissingBinding(account_id.to_string()))?;
        let Some(store) = browser_session_runtime::get() else {
            return Err(BrowserProviderError::SessionUnavailable {
                account_id: account_id.to_string(),
                session_id: binding.session.clone(),
            });
        };
        store
            .mark_degraded(&binding.session, error)
            .await
            .map_err(|_| BrowserProviderError::SessionUnavailable {
                account_id: account_id.to_string(),
                session_id: binding.session.clone(),
            })?;
        Ok(())
    }

    pub async fn require_attention(
        &self,
        account_id: &str,
        error: &str,
    ) -> Result<(), BrowserProviderError> {
        self.invalidate_diagnostics(account_id).await;
        let binding = self
            .config
            .bindings
            .get(account_id)
            .ok_or_else(|| BrowserProviderError::MissingBinding(account_id.to_string()))?;
        let Some(store) = browser_session_runtime::get() else {
            return Err(BrowserProviderError::SessionUnavailable {
                account_id: account_id.to_string(),
                session_id: binding.session.clone(),
            });
        };
        store
            .require_attention(&binding.session, error)
            .await
            .map_err(|_| BrowserProviderError::SessionUnavailable {
                account_id: account_id.to_string(),
                session_id: binding.session.clone(),
            })?;
        Ok(())
    }

    pub async fn execute_chat(
        &self,
        provider: &ProviderConfig,
        account: &AccountConfig,
        route: &RouteConfig,
        body: &Value,
    ) -> Result<reqwest::Response, BrowserProviderError> {
        let adapter = self
            .adapters
            .get(&provider.kind)
            .ok_or_else(|| BrowserProviderError::UnsupportedAdapter(provider.kind.clone()))?;
        let binding = self
            .config
            .bindings
            .get(&account.id)
            .ok_or_else(|| BrowserProviderError::MissingBinding(account.id.clone()))?;

        if !binding.models.is_empty() && !binding.models.iter().any(|model| model == &route.model) {
            return Err(BrowserProviderError::ModelUnavailable {
                account_id: account.id.clone(),
                model: route.model.clone(),
            });
        }

        let Some(store) = browser_session_runtime::get() else {
            return Err(BrowserProviderError::SessionUnavailable {
                account_id: account.id.clone(),
                session_id: binding.session.clone(),
            });
        };
        let session = store
            .session(&binding.session)
            .await
            .map_err(|_| BrowserProviderError::SessionUnavailable {
                account_id: account.id.clone(),
                session_id: binding.session.clone(),
            })?;
        if !session.enabled || session.status != "ready" {
            return Err(BrowserProviderError::SessionUnavailable {
                account_id: account.id.clone(),
                session_id: binding.session.clone(),
            });
        }

        let result = adapter
            .execute_chat(BrowserAdapterRequest {
                provider: provider.clone(),
                account: account.clone(),
                route: route.clone(),
                body: body.clone(),
                session_id: binding.session.clone(),
                profile_dir: session.profile_dir,
                binding: binding.clone(),
            })
            .await;

        match &result {
            Ok(_) => {
                self.invalidate_diagnostics(&account.id).await;
            }
            Err(BrowserProviderError::AdapterIncompatible { code, message, .. }) => {
                let status = if code == "login_required" {
                    let _ = self
                        .require_attention(
                            &account.id,
                            &format!("browser adapter login required: {message}"),
                        )
                        .await;
                    "login_required"
                } else {
                    "adapter_incompatible"
                };
                self.cache_diagnostics(BrowserAdapterDiagnostics {
                    account_id: account.id.clone(),
                    provider_kind: provider.kind.clone(),
                    adapter_id: Some(adapter.adapter_id().to_string()),
                    adapter_version: None,
                    contract_version: None,
                    expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                    status: status.into(),
                    message: format!("{code}: {message}"),
                    page_signature: None,
                    target_url_prefix: effective_target_url_prefix(&provider.kind, binding),
                    configured_models: binding.models.clone(),
                })
                .await;
            }
            Err(BrowserProviderError::SessionUnavailable { .. })
            | Err(BrowserProviderError::Transport(_))
                if adapter.is_cdp() =>
            {
                let error_text = result
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "browser CDP runtime unavailable".into());
                let _ = self.mark_degraded(&account.id, &error_text).await;
            }
            _ => {}
        }
        result
    }
}


#[derive(Clone)]
struct HttpBrowserAdapter {
    client: Client,
}

impl HttpBrowserAdapter {
    fn new() -> Result<Self, BrowserProviderError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl BrowserProviderAdapter for HttpBrowserAdapter {
    fn kind(&self) -> &'static str {
        "browser-http"
    }

    fn adapter_id(&self) -> &'static str {
        "browser-http"
    }

    async fn diagnose(
        &self,
        account_id: &str,
        _profile_dir: &str,
        binding: &BrowserAccountBinding,
    ) -> BrowserAdapterDiagnostics {
        BrowserAdapterDiagnostics {
            account_id: account_id.to_string(),
            provider_kind: self.kind().to_string(),
            adapter_id: Some(self.adapter_id().to_string()),
            adapter_version: Some("bridge-v1".into()),
            contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
            expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
            status: "ready".into(),
            message: "browser-http bridge is configured".into(),
            page_signature: None,
            target_url_prefix: binding.target_url_prefix.clone(),
            configured_models: binding.models.clone(),
        }
    }

    async fn execute_chat(
        &self,
        request: BrowserAdapterRequest,
    ) -> Result<reqwest::Response, BrowserProviderError> {
        let mut upstream_body = request.body;
        let object = upstream_body.as_object_mut().ok_or_else(|| {
            BrowserProviderError::InvalidConfig("chat request body must be a JSON object".into())
        })?;
        object.insert("model".into(), Value::String(request.route.model.clone()));

        let url = format!(
            "{}/chat/completions",
            request.provider.base_url.trim_end_matches('/')
        );
        self.client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header("x-llmgateway-browser-session", request.session_id)
            .header("x-llmgateway-browser-account", request.account.id)
            .header("x-llmgateway-route", request.route.id)
            .json(&upstream_body)
            .send()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))
    }
}

#[derive(Clone, Copy)]
struct CdpAdapterSpec {
    kind: &'static str,
    adapter_id: &'static str,
    provider: &'static str,
    builtin_script: Option<&'static str>,
    default_target_url_prefix: Option<&'static str>,
    new_chat_url: Option<&'static str>,
    ephemeral_default: bool,
}

#[derive(Clone)]
struct CdpBrowserAdapter {
    client: Client,
    spec: CdpAdapterSpec,
}

#[derive(Clone, Debug, Deserialize)]
struct CdpTarget {
    #[serde(default)]
    id: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    websocket_debugger_url: String,
}

#[derive(Clone, Debug, Deserialize)]
struct CdpAdapterResult {
    #[serde(default = "default_ok_status")]
    status: u16,
    #[serde(default = "default_json_content_type")]
    content_type: String,
    body: Value,
}

impl CdpBrowserAdapter {
    fn custom() -> Result<Self, BrowserProviderError> {
        Self::new(CdpAdapterSpec {
            kind: "browser-cdp",
            adapter_id: "custom-cdp",
            provider: "custom",
            builtin_script: None,
            default_target_url_prefix: None,
            new_chat_url: None,
            ephemeral_default: false,
        })
    }

    fn gemini() -> Result<Self, BrowserProviderError> {
        Self::new(CdpAdapterSpec {
            kind: "browser-gemini",
            adapter_id: "gemini-web",
            provider: "gemini",
            builtin_script: Some(GEMINI_WEB_ADAPTER),
            default_target_url_prefix: Some("https://gemini.google.com/app"),
            new_chat_url: Some("https://gemini.google.com/app"),
            ephemeral_default: true,
        })
    }

    fn qwen() -> Result<Self, BrowserProviderError> {
        Self::new(CdpAdapterSpec {
            kind: "browser-qwen",
            adapter_id: "qwen-web",
            provider: "qwen",
            builtin_script: Some(QWEN_WEB_ADAPTER),
            default_target_url_prefix: Some("https://chat.qwen.ai/"),
            new_chat_url: Some("https://chat.qwen.ai/c/new-chat"),
            ephemeral_default: true,
        })
    }

    fn new(spec: CdpAdapterSpec) -> Result<Self, BrowserProviderError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        Ok(Self { client, spec })
    }

    fn script(&self, binding: &BrowserAccountBinding) -> Result<String, BrowserProviderError> {
        if let Some(script) = self.spec.builtin_script {
            if binding.adapter_script.is_some() {
                return Err(BrowserProviderError::InvalidConfig(format!(
                    "{} uses a built-in adapter; adapter_script is not allowed",
                    self.spec.kind
                )));
            }
            return Ok(script.to_string());
        }

        let script_path = binding.adapter_script.as_deref().ok_or_else(|| {
            BrowserProviderError::InvalidConfig(
                "browser-cdp requires browser.bindings.<account>.adapter_script".into(),
            )
        })?;
        read_adapter_script(script_path)
    }

    fn target_url_prefix<'a>(&self, binding: &'a BrowserAccountBinding) -> Option<&'a str> {
        binding
            .target_url_prefix
            .as_deref()
            .or(self.spec.default_target_url_prefix)
    }

    fn use_ephemeral_chat(&self, binding: &BrowserAccountBinding) -> bool {
        binding.ephemeral_chat.unwrap_or(self.spec.ephemeral_default)
    }

    fn model_label(&self, binding: &BrowserAccountBinding, model: &str) -> Option<String> {
        if let Some(label) = binding.model_labels.get(model) {
            return Some(label.clone());
        }
        if self.spec.builtin_script.is_none() {
            return Some(model.to_string());
        }
        None
    }

    fn context(&self, binding: &BrowserAccountBinding, model: Option<&str>) -> Value {
        json!({
            "provider": self.spec.provider,
            "adapter_id": self.spec.adapter_id,
            "contract_version": BROWSER_ADAPTER_CONTRACT_VERSION,
            "model_label": model.and_then(|value| self.model_label(binding, value)),
            "selectors": binding.selector_overrides,
            "probe_timeout_ms": binding.probe_timeout_ms.unwrap_or(8_000),
            "response_timeout_ms": binding.response_timeout_ms.unwrap_or(180_000),
        })
    }

    async fn targets(&self, profile_dir: &str) -> Result<Vec<CdpTarget>, BrowserProviderError> {
        let port = read_debugger_port(profile_dir)?;
        let response = self
            .client
            .get(format!("http://127.0.0.1:{port}/json/list"))
            .send()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        if !response.status().is_success() {
            return Err(BrowserProviderError::Transport(format!(
                "DevTools target list returned HTTP {}",
                response.status()
            )));
        }
        response
            .json::<Vec<CdpTarget>>()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))
    }

    async fn open_ephemeral_target(
        &self,
        profile_dir: &str,
    ) -> Result<CdpTarget, BrowserProviderError> {
        let url = self.spec.new_chat_url.ok_or_else(|| {
            BrowserProviderError::InvalidConfig(format!(
                "{} does not define an ephemeral new-chat URL",
                self.spec.kind
            ))
        })?;
        let port = read_debugger_port(profile_dir)?;
        let endpoint = format!("http://127.0.0.1:{port}/json/new?{url}");
        let response = self
            .client
            .put(endpoint)
            .send()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        if !response.status().is_success() {
            return Err(BrowserProviderError::Transport(format!(
                "DevTools create target returned HTTP {}",
                response.status()
            )));
        }
        response
            .json::<CdpTarget>()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))
    }

    async fn close_target(&self, profile_dir: &str, target_id: &str) {
        if target_id.trim().is_empty() {
            return;
        }
        if let Ok(port) = read_debugger_port(profile_dir) {
            let _ = self
                .client
                .get(format!("http://127.0.0.1:{port}/json/close/{target_id}"))
                .send()
                .await;
        }
    }

    fn select_target<'a>(
        &self,
        targets: &'a [CdpTarget],
        prefix: Option<&str>,
    ) -> Option<&'a CdpTarget> {
        targets.iter().find(|target| {
            target.kind == "page"
                && !target.websocket_debugger_url.is_empty()
                && prefix.is_none_or(|prefix| target.url.starts_with(prefix))
        })
    }

    async fn evaluate_contract(
        &self,
        target: &CdpTarget,
        script: &str,
        operation: &str,
        request: Option<&Value>,
        context: &Value,
        account_id: &str,
    ) -> Result<AdapterContractEnvelope, BrowserProviderError> {
        let expression = build_contract_expression(script, operation, request, context)?;
        let value = evaluate_cdp(&target.websocket_debugger_url, &expression).await?;
        let envelope: AdapterContractEnvelope = serde_json::from_value(value).map_err(|error| {
            BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "invalid_contract_envelope".into(),
                message: error.to_string(),
            }
        })?;
        validate_contract_envelope(account_id, self.spec, &envelope)?;
        Ok(envelope)
    }

    fn diagnostics_from_envelope(
        &self,
        account_id: &str,
        binding: &BrowserAccountBinding,
        envelope: AdapterContractEnvelope,
    ) -> BrowserAdapterDiagnostics {
        let meta = envelope.meta;
        let probe = envelope.probe;
        let error = envelope.error;
        let status = if let Some(error) = &error {
            if error.code == "login_required" {
                "login_required"
            } else {
                "adapter_incompatible"
            }
        } else if probe.as_ref().is_some_and(|probe| probe.ok) {
            "ready"
        } else {
            "adapter_incompatible"
        };
        let message = error
            .as_ref()
            .map(|error| error.message.clone())
            .or_else(|| probe.as_ref().map(|probe| probe.message.clone()))
            .filter(|message| !message.trim().is_empty())
            .unwrap_or_else(|| status.to_string());

        BrowserAdapterDiagnostics {
            account_id: account_id.to_string(),
            provider_kind: self.kind().to_string(),
            adapter_id: meta.as_ref().map(|meta| meta.id.clone()),
            adapter_version: meta
                .as_ref()
                .map(|meta| meta.adapter_version.clone())
                .filter(|value| !value.is_empty()),
            contract_version: meta.as_ref().map(|meta| meta.contract_version),
            expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
            status: status.into(),
            message,
            page_signature: probe.and_then(|probe| probe.page_signature),
            target_url_prefix: self.target_url_prefix(binding).map(str::to_string),
            configured_models: binding.models.clone(),
        }
    }
}

#[async_trait]
impl BrowserProviderAdapter for CdpBrowserAdapter {
    fn kind(&self) -> &'static str {
        self.spec.kind
    }

    fn adapter_id(&self) -> &'static str {
        self.spec.adapter_id
    }

    fn is_cdp(&self) -> bool {
        true
    }

    async fn diagnose(
        &self,
        account_id: &str,
        profile_dir: &str,
        binding: &BrowserAccountBinding,
    ) -> BrowserAdapterDiagnostics {
        let script = match self.script(binding) {
            Ok(script) => script,
            Err(error) => {
                return incompatible_diagnostics(
                    account_id,
                    self.kind(),
                    self.adapter_id(),
                    binding,
                    "adapter_config_error",
                    &error.to_string(),
                );
            }
        };
        let targets = match self.targets(profile_dir).await {
            Ok(targets) => targets,
            Err(error) => {
                return unavailable_diagnostics(
                    account_id,
                    self.kind(),
                    self.adapter_id(),
                    binding,
                    &error.to_string(),
                );
            }
        };
        let Some(target) = self.select_target(&targets, self.target_url_prefix(binding)) else {
            return incompatible_diagnostics(
                account_id,
                self.kind(),
                self.adapter_id(),
                binding,
                "target_not_found",
                "No authenticated provider page matched the configured target URL",
            );
        };
        let context = self.context(binding, None);
        match self
            .evaluate_contract(target, &script, "probe", None, &context, account_id)
            .await
        {
            Ok(envelope) => self.diagnostics_from_envelope(account_id, binding, envelope),
            Err(BrowserProviderError::AdapterIncompatible { code, message, .. }) => {
                incompatible_diagnostics(
                    account_id,
                    self.kind(),
                    self.adapter_id(),
                    binding,
                    &code,
                    &message,
                )
            }
            Err(error) => unavailable_diagnostics(
                account_id,
                self.kind(),
                self.adapter_id(),
                binding,
                &error.to_string(),
            ),
        }
    }

    async fn execute_chat(
        &self,
        request: BrowserAdapterRequest,
    ) -> Result<reqwest::Response, BrowserProviderError> {
        let script = self.script(&request.binding)?;
        let mut normalized_body = request.body;
        let object = normalized_body.as_object_mut().ok_or_else(|| {
            BrowserProviderError::InvalidConfig("chat request body must be a JSON object".into())
        })?;
        object.insert("model".into(), Value::String(request.route.model.clone()));

        let context = self.context(&request.binding, Some(&request.route.model));
        let ephemeral = self.use_ephemeral_chat(&request.binding);
        let target = if ephemeral {
            self.open_ephemeral_target(&request.profile_dir).await?
        } else {
            let targets = self.targets(&request.profile_dir).await?;
            self.select_target(&targets, self.target_url_prefix(&request.binding))
                .cloned()
                .ok_or_else(|| BrowserProviderError::SessionUnavailable {
                    account_id: request.account.id.clone(),
                    session_id: request.session_id.clone(),
                })?
        };

        let result = self
            .evaluate_contract(
                &target,
                &script,
                "chat",
                Some(&normalized_body),
                &context,
                &request.account.id,
            )
            .await;
        if ephemeral {
            self.close_target(&request.profile_dir, &target.id).await;
        }

        let envelope = result?;
        if let Some(error) = envelope.error {
            return contract_error_to_provider_error(&request.account.id, &request.route.model, error);
        }
        let result = envelope.result.ok_or_else(|| BrowserProviderError::AdapterIncompatible {
            account_id: request.account.id.clone(),
            code: "missing_result".into(),
            message: "adapter contract did not return a chat result".into(),
        })?;
        synthetic_response(result)
    }
}



fn effective_target_url_prefix(
    provider_kind: &str,
    binding: &BrowserAccountBinding,
) -> Option<String> {
    binding
        .target_url_prefix
        .clone()
        .or_else(|| match provider_kind {
            "browser-gemini" => Some("https://gemini.google.com/app".into()),
            "browser-qwen" => Some("https://chat.qwen.ai/".into()),
            _ => None,
        })
}

fn unavailable_diagnostics(
    account_id: &str,
    provider_kind: &str,
    adapter_id: &str,
    binding: &BrowserAccountBinding,
    message: &str,
) -> BrowserAdapterDiagnostics {
    BrowserAdapterDiagnostics {
        account_id: account_id.to_string(),
        provider_kind: provider_kind.to_string(),
        adapter_id: Some(adapter_id.to_string()),
        adapter_version: None,
        contract_version: None,
        expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
        status: "unavailable".into(),
        message: message.to_string(),
        page_signature: None,
        target_url_prefix: effective_target_url_prefix(provider_kind, binding),
        configured_models: binding.models.clone(),
    }
}

fn incompatible_diagnostics(
    account_id: &str,
    provider_kind: &str,
    adapter_id: &str,
    binding: &BrowserAccountBinding,
    code: &str,
    message: &str,
) -> BrowserAdapterDiagnostics {
    BrowserAdapterDiagnostics {
        account_id: account_id.to_string(),
        provider_kind: provider_kind.to_string(),
        adapter_id: Some(adapter_id.to_string()),
        adapter_version: None,
        contract_version: None,
        expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
        status: if code == "login_required" {
            "login_required".into()
        } else {
            "adapter_incompatible".into()
        },
        message: format!("{code}: {message}"),
        page_signature: None,
        target_url_prefix: effective_target_url_prefix(provider_kind, binding),
        configured_models: binding.models.clone(),
    }
}

fn build_contract_expression(
    script: &str,
    operation: &str,
    request: Option<&Value>,
    context: &Value,
) -> Result<String, BrowserProviderError> {
    let operation_json = serde_json::to_string(operation)
        .map_err(|error| BrowserProviderError::InvalidConfig(error.to_string()))?;
    let request_json = serde_json::to_string(request.unwrap_or(&Value::Null))
        .map_err(|error| BrowserProviderError::InvalidConfig(error.to_string()))?;
    let context_json = serde_json::to_string(context)
        .map_err(|error| BrowserProviderError::InvalidConfig(error.to_string()))?;

    let mut expression = String::from("(async () => {\n");
    expression.push_str(script);
    expression.push_str(
        r#"
const __adapter = globalThis.__LLMGATEWAY_ADAPTER__;
const __operation = "#,
    );
    expression.push_str(&operation_json);
    expression.push_str(";\nconst __request = ");
    expression.push_str(&request_json);
    expression.push_str(";\nconst __context = ");
    expression.push_str(&context_json);
    expression.push_str(
        r#";
const __expected = 1;
if (!__adapter || typeof __adapter !== "object") {
  return { error: { code: "contract_missing", message: "adapter did not expose globalThis.__LLMGATEWAY_ADAPTER__" } };
}
const __meta = __adapter.meta || null;
if (!__meta || Number(__meta.contract_version || 0) !== __expected) {
  return {
    meta: __meta,
    error: {
      code: "contract_version_mismatch",
      message: "expected browser adapter contract v" + __expected + " but adapter reported " + String(__meta?.contract_version || 0)
    }
  };
}
let __probe = { ok: true, code: "ready", message: "adapter probe not implemented" };
try {
  if (typeof __adapter.probe === "function") {
    __probe = await __adapter.probe(__context);
  }
} catch (error) {
  return {
    meta: __meta,
    error: {
      code: "adapter_incompatible",
      message: String(error?.message || error || "adapter probe failed")
    }
  };
}
if (!__probe || __probe.ok !== true) {
  return {
    meta: __meta,
    probe: __probe || null,
    error: {
      code: String(__probe?.code || "adapter_incompatible"),
      message: String(__probe?.message || "adapter probe reported incompatible page")
    }
  };
}
if (__operation === "probe") {
  return { meta: __meta, probe: __probe };
}
if (typeof __adapter.chat !== "function") {
  return {
    meta: __meta,
    probe: __probe,
    error: { code: "contract_missing", message: "adapter contract is missing chat(request, context)" }
  };
}
try {
  const __result = await __adapter.chat(__request, __context);
  return { meta: __meta, probe: __probe, result: __result };
} catch (error) {
  const __message = String(error?.message || error || "adapter execution failed");
  let __code = "adapter_execution_error";
  if (/^ADAPTER_INCOMPATIBLE:/i.test(__message)) __code = "adapter_incompatible";
  else if (/^(MODEL_NOT_FOUND|MODEL_PICKER_NOT_FOUND):/i.test(__message)) __code = "model_unavailable";
  else if (/^LOGIN_REQUIRED:/i.test(__message)) __code = "login_required";
  else if (/^RESPONSE_TIMEOUT:/i.test(__message)) __code = "response_timeout";
  else if (/^INVALID_REQUEST:/i.test(__message)) __code = "invalid_request";
  return { meta: __meta, probe: __probe, error: { code: __code, message: __message } };
}
})()"#,
    );
    Ok(expression)
}

fn validate_contract_envelope(
    account_id: &str,
    spec: CdpAdapterSpec,
    envelope: &AdapterContractEnvelope,
) -> Result<(), BrowserProviderError> {
    let Some(meta) = envelope.meta.as_ref() else {
        let error = envelope.error.as_ref();
        return Err(BrowserProviderError::AdapterIncompatible {
            account_id: account_id.to_string(),
            code: error
                .map(|error| error.code.clone())
                .unwrap_or_else(|| "contract_missing".into()),
            message: error
                .map(|error| error.message.clone())
                .unwrap_or_else(|| "adapter metadata is missing".into()),
        });
    };
    if meta.contract_version != BROWSER_ADAPTER_CONTRACT_VERSION {
        return Err(BrowserProviderError::AdapterIncompatible {
            account_id: account_id.to_string(),
            code: "contract_version_mismatch".into(),
            message: format!(
                "expected contract v{} but adapter reported v{}",
                BROWSER_ADAPTER_CONTRACT_VERSION, meta.contract_version
            ),
        });
    }
    if meta.id.trim().is_empty() || meta.provider.trim().is_empty() {
        return Err(BrowserProviderError::AdapterIncompatible {
            account_id: account_id.to_string(),
            code: "invalid_adapter_metadata".into(),
            message: "adapter id and provider must be non-empty".into(),
        });
    }
    if spec.builtin_script.is_some() && meta.id != spec.adapter_id {
        return Err(BrowserProviderError::AdapterIncompatible {
            account_id: account_id.to_string(),
            code: "adapter_id_mismatch".into(),
            message: format!(
                "{} expected adapter id '{}' but script reported '{}'",
                spec.kind, spec.adapter_id, meta.id
            ),
        });
    }
    if spec.builtin_script.is_some() && meta.provider != spec.provider {
        return Err(BrowserProviderError::AdapterIncompatible {
            account_id: account_id.to_string(),
            code: "adapter_provider_mismatch".into(),
            message: format!(
                "{} expected provider '{}' but script reported '{}'",
                spec.kind, spec.provider, meta.provider
            ),
        });
    }
    Ok(())
}

fn contract_error_to_provider_error(
    account_id: &str,
    model: &str,
    error: AdapterContractError,
) -> Result<reqwest::Response, BrowserProviderError> {
    match error.code.as_str() {
        "model_unavailable" => Err(BrowserProviderError::ModelUnavailable {
            account_id: account_id.to_string(),
            model: model.to_string(),
        }),
        "contract_missing"
        | "contract_version_mismatch"
        | "adapter_incompatible"
        | "wrong_page"
        | "target_not_found"
        | "login_required" => Err(BrowserProviderError::AdapterIncompatible {
            account_id: account_id.to_string(),
            code: error.code,
            message: error.message,
        }),
        "invalid_request" => Err(BrowserProviderError::InvalidConfig(error.message)),
        _ => Err(BrowserProviderError::Transport(format!(
            "{}: {}",
            error.code, error.message
        ))),
    }
}


fn read_debugger_port(profile_dir: &str) -> Result<u16, BrowserProviderError> {
    let path = Path::new(profile_dir).join("DevToolsActivePort");
    let raw = fs::read_to_string(&path).map_err(|error| {
        BrowserProviderError::Transport(format!("failed to read {}: {error}", path.display()))
    })?;
    raw.lines()
        .next()
        .ok_or_else(|| BrowserProviderError::Transport("DevToolsActivePort is empty".into()))?
        .parse::<u16>()
        .map_err(|error| BrowserProviderError::Transport(format!("invalid DevTools port: {error}")))
}

fn read_adapter_script(path: &str) -> Result<String, BrowserProviderError> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_ADAPTER_SCRIPT_BYTES {
        return Err(BrowserProviderError::InvalidConfig(format!(
            "browser adapter script '{path}' exceeds {} KiB",
            MAX_ADAPTER_SCRIPT_BYTES / 1024
        )));
    }
    let script = fs::read_to_string(path)?;
    if script.trim().is_empty() {
        return Err(BrowserProviderError::InvalidConfig(format!(
            "browser adapter script '{path}' is empty"
        )));
    }
    Ok(script)
}

async fn evaluate_cdp(
    websocket_url: &str,
    expression: &str,
) -> Result<Value, BrowserProviderError> {
    let (mut socket, _) = connect_async(websocket_url)
        .await
        .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
    let request_id = 1u64;
    let command = json!({
        "id": request_id,
        "method": "Runtime.evaluate",
        "params": {
            "expression": expression,
            "awaitPromise": true,
            "returnByValue": true,
            "userGesture": true
        }
    });
    socket
        .send(Message::Text(command.to_string().into()))
        .await
        .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;

    let value = timeout(Duration::from_secs(CDP_EXECUTION_TIMEOUT_SECONDS), async {
        while let Some(message) = socket.next().await {
            let message = message.map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
            match message {
                Message::Text(text) => {
                    let payload: Value = serde_json::from_str(text.as_ref())
                        .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
                    if payload.get("id").and_then(Value::as_u64) != Some(request_id) {
                        continue;
                    }
                    if let Some(error) = payload.get("error") {
                        return Err(BrowserProviderError::Transport(format!(
                            "CDP Runtime.evaluate failed: {error}"
                        )));
                    }
                    let result = payload.get("result").ok_or_else(|| {
                        BrowserProviderError::Transport("CDP response is missing result".into())
                    })?;
                    if let Some(exception) = result.get("exceptionDetails") {
                        return Err(BrowserProviderError::Transport(format!(
                            "browser adapter script threw an exception: {exception}"
                        )));
                    }
                    return result
                        .get("result")
                        .and_then(|remote| remote.get("value"))
                        .cloned()
                        .ok_or_else(|| {
                            BrowserProviderError::Transport(
                                "browser adapter script did not return a serializable value".into(),
                            )
                        });
                }
                Message::Ping(payload) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
                }
                Message::Close(_) => {
                    return Err(BrowserProviderError::Transport(
                        "CDP websocket closed before Runtime.evaluate completed".into(),
                    ));
                }
                _ => {}
            }
        }
        Err(BrowserProviderError::Transport(
            "CDP websocket ended before Runtime.evaluate completed".into(),
        ))
    })
    .await
    .map_err(|_| BrowserProviderError::Transport("browser CDP execution timed out".into()))??;

    let _ = socket.close(None).await;
    Ok(value)
}

fn synthetic_response(result: CdpAdapterResult) -> Result<reqwest::Response, BrowserProviderError> {
    let status = reqwest::StatusCode::from_u16(result.status).map_err(|error| {
        BrowserProviderError::Transport(format!("invalid adapter HTTP status: {error}"))
    })?;
    let body = match result.body {
        Value::String(text) => text,
        value => serde_json::to_string(&value)
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?,
    };
    let response = HttpResponse::builder()
        .status(status)
        .header(CONTENT_TYPE, result.content_type)
        .body(reqwest::Body::from(body))
        .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
    Ok(reqwest::Response::from(response))
}

fn default_ok_status() -> u16 {
    200
}

fn default_json_content_type() -> String {
    "application/json".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Json, Router};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use uuid::Uuid;

    #[test]
    fn browser_kind_is_explicit() {
        assert!(BrowserProviderRegistry::is_browser_kind("browser-http"));
        assert!(BrowserProviderRegistry::is_browser_kind("browser-cdp"));
        assert!(BrowserProviderRegistry::is_browser_kind("browser-gemini"));
        assert!(BrowserProviderRegistry::is_browser_kind("browser-qwen"));
        assert!(!BrowserProviderRegistry::is_browser_kind("openai-compatible"));
    }

    #[test]
    fn selects_matching_page_target() {
        let adapter = CdpBrowserAdapter::custom().unwrap();
        let targets = vec![
            CdpTarget {
                id: "one".into(),
                kind: "page".into(),
                url: "https://example.test/login".into(),
                websocket_debugger_url: "ws://127.0.0.1/one".into(),
            },
            CdpTarget {
                id: "two".into(),
                kind: "page".into(),
                url: "https://example.test/chat/123".into(),
                websocket_debugger_url: "ws://127.0.0.1/two".into(),
            },
        ];
        let selected = adapter
            .select_target(&targets, Some("https://example.test/chat"))
            .unwrap();
        assert_eq!(selected.websocket_debugger_url, "ws://127.0.0.1/two");
    }

    #[test]
    fn built_in_provider_defaults_are_first_class() {
        let gemini = CdpBrowserAdapter::gemini().unwrap();
        let qwen = CdpBrowserAdapter::qwen().unwrap();
        assert_eq!(gemini.kind(), "browser-gemini");
        assert_eq!(gemini.adapter_id(), "gemini-web");
        assert!(gemini.spec.ephemeral_default);
        assert_eq!(
            gemini.spec.default_target_url_prefix,
            Some("https://gemini.google.com/app")
        );
        assert_eq!(qwen.kind(), "browser-qwen");
        assert_eq!(qwen.adapter_id(), "qwen-web");
        assert!(qwen.spec.ephemeral_default);
        assert_eq!(qwen.spec.new_chat_url, Some("https://chat.qwen.ai/c/new-chat"));
    }

    #[test]
    fn contract_expression_requires_version_and_probe() {
        let expression = build_contract_expression(
            "globalThis.__LLMGATEWAY_ADAPTER__ = {meta:{contract_version:1,id:'x',provider:'x'},probe:async()=>({ok:true}),chat:async()=>({status:200,body:{ok:true}})};",
            "probe",
            None,
            &json!({"model_label":"x"}),
        )
        .unwrap();
        assert!(expression.contains("contract_version"));
        assert!(expression.contains("__adapter.probe"));
        assert!(expression.contains("contract_version_mismatch"));
    }

    #[tokio::test]
    async fn cdp_evaluation_returns_by_value_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let message = socket.next().await.unwrap().unwrap();
            let text = message.into_text().unwrap();
            let command: Value = serde_json::from_str(text.as_ref()).unwrap();
            assert_eq!(command["method"], "Runtime.evaluate");
            let id = command["id"].as_u64().unwrap();
            let response = json!({
                "id": id,
                "result": {
                    "result": {
                        "type": "object",
                        "value": {
                            "status": 200,
                            "content_type": "application/json",
                            "body": {"ok": true}
                        }
                    }
                }
            });
            socket
                .send(Message::Text(response.to_string().into()))
                .await
                .unwrap();
        });

        let value = evaluate_cdp(&format!("ws://{address}"), "Promise.resolve({ok:true})")
            .await
            .unwrap();
        assert_eq!(value["body"]["ok"], true);
        server.await.unwrap();
    }

    fn test_binding() -> BrowserAccountBinding {
        BrowserAccountBinding {
            session: "fixture-session".into(),
            target_url_prefix: None,
            adapter_script: None,
            adapter_contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
            models: vec!["gemini-test".into()],
            model_labels: BTreeMap::new(),
            selector_overrides: BTreeMap::new(),
            ephemeral_chat: Some(false),
            probe_timeout_ms: Some(500),
            response_timeout_ms: Some(1_000),
        }
    }

    async fn fake_cdp_diagnostics(envelope: Value) -> BrowserAdapterDiagnostics {
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_address = ws_listener.local_addr().unwrap();
        let websocket_url = format!("ws://{ws_address}/devtools/page/fixture");
        let ws_task = tokio::spawn(async move {
            let (stream, _) = ws_listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let message = socket.next().await.unwrap().unwrap();
            let command: Value =
                serde_json::from_str(message.into_text().unwrap().as_ref()).unwrap();
            assert_eq!(command["method"], "Runtime.evaluate");
            let expression = command["params"]["expression"].as_str().unwrap();
            assert!(expression.contains("gemini-web"));
            let id = command["id"].as_u64().unwrap();
            let response = json!({
                "id": id,
                "result": {
                    "result": {
                        "type": "object",
                        "value": envelope
                    }
                }
            });
            socket
                .send(Message::Text(response.to_string().into()))
                .await
                .unwrap();
        });

        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_address = http_listener.local_addr().unwrap();
        let target_ws = websocket_url.clone();
        let app = Router::new().route(
            "/json/list",
            get(move || {
                let target_ws = target_ws.clone();
                async move {
                    Json(json!([{
                        "id": "fixture-target",
                        "type": "page",
                        "url": "https://gemini.google.com/app",
                        "webSocketDebuggerUrl": target_ws
                    }]))
                }
            }),
        );
        let http_task = tokio::spawn(async move {
            axum::serve(http_listener, app).await.unwrap();
        });

        let profile_dir = std::env::temp_dir().join(format!(
            "llmgateway-adapter-fixture-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&profile_dir).unwrap();
        fs::write(
            profile_dir.join("DevToolsActivePort"),
            format!("{}\n/devtools/browser/fixture\n", http_address.port()),
        )
        .unwrap();

        let adapter = CdpBrowserAdapter::gemini().unwrap();
        let diagnostics = adapter
            .diagnose(
                "gemini-fixture-account",
                profile_dir.to_str().unwrap(),
                &test_binding(),
            )
            .await;

        ws_task.await.unwrap();
        http_task.abort();
        let _ = fs::remove_dir_all(profile_dir);
        diagnostics
    }

    #[tokio::test]
    async fn fake_cdp_fixture_reports_ready_adapter() {
        let diagnostics = fake_cdp_diagnostics(json!({
            "meta": {
                "contract_version": 1,
                "id": "gemini-web",
                "provider": "gemini",
                "adapter_version": "fixture-v1"
            },
            "probe": {
                "ok": true,
                "code": "ready",
                "message": "fixture page compatible",
                "page_signature": "fixture-gemini-v1"
            }
        }))
        .await;
        assert_eq!(diagnostics.status, "ready");
        assert_eq!(diagnostics.adapter_id.as_deref(), Some("gemini-web"));
        assert_eq!(diagnostics.adapter_version.as_deref(), Some("fixture-v1"));
        assert_eq!(
            diagnostics.page_signature.as_deref(),
            Some("fixture-gemini-v1")
        );
    }

    #[tokio::test]
    async fn fake_cdp_fixture_surfaces_page_drift() {
        let diagnostics = fake_cdp_diagnostics(json!({
            "meta": {
                "contract_version": 1,
                "id": "gemini-web",
                "provider": "gemini",
                "adapter_version": "fixture-v1"
            },
            "probe": {
                "ok": false,
                "code": "adapter_incompatible",
                "message": "prompt composer missing"
            },
            "error": {
                "code": "adapter_incompatible",
                "message": "prompt composer missing"
            }
        }))
        .await;
        assert_eq!(diagnostics.status, "adapter_incompatible");
        assert!(diagnostics.message.contains("prompt composer missing"));
    }

    #[test]
    fn synthetic_response_preserves_status_and_content_type() {
        let response = synthetic_response(CdpAdapterResult {
            status: 429,
            content_type: "application/json".into(),
            body: json!({"error":{"message":"quota"}}),
        })
        .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }
}
