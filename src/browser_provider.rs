use crate::{
    browser_auth_runtime, browser_session_runtime, chromium_driver_runtime, conversation_runtime,
    chatgpt_web_transport::ChatGptWebHttpAdapter,
    config::{AccountConfig, ProviderConfig, RouteConfig},
    gemini_web_transport::GeminiWebHttpAdapter,
    qwen_web_transport::QwenWebHttpAdapter,
};
use async_trait::async_trait;
use axum::http::Response as HttpResponse;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use reqwest::{
    header::{CONTENT_TYPE, COOKIE, USER_AGENT},
    Client, Url,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::{Arc, RwLock as StdRwLock},
    time::{Duration, Instant},
};
use thiserror::Error;
use tracing::warn;
use tokio::{
    sync::RwLock,
    time::{sleep, timeout},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const MAX_ADAPTER_SCRIPT_BYTES: u64 = 512 * 1024;
const CDP_EXECUTION_TIMEOUT_SECONDS: u64 = 600;
pub const BROWSER_ADAPTER_CONTRACT_VERSION: u32 = 1;
const ADAPTER_HEALTH_TTL_SECONDS: u64 = 30;
const GEMINI_WEB_ADAPTER: &str = include_str!("../adapters/gemini-web.js");
const CHATGPT_WEB_ADAPTER: &str = include_str!("../adapters/chatgpt-web.js");
const QWEN_WEB_ADAPTER: &str = include_str!("../adapters/qwen-web.js");

#[derive(Clone, Debug, Default, Deserialize)]
pub struct BrowserProviderConfig {
    #[serde(default)]
    pub bindings: BTreeMap<String, BrowserAccountBinding>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserTransportMode {
    #[default]
    Auto,
    BrowserOnly,
    HttpPreferred,
}

impl BrowserTransportMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::BrowserOnly => "browser-only",
            Self::HttpPreferred => "http-preferred",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserTransportPolicy {
    BrowserOnly,
    BrowserlessPreferred,
}

impl BrowserTransportPolicy {
    pub fn parse(value: &str) -> Result<Self, BrowserProviderError> {
        match value {
            "browser-only" => Ok(Self::BrowserOnly),
            "browserless-preferred" => Ok(Self::BrowserlessPreferred),
            other => Err(BrowserProviderError::InvalidTransportPolicy(other.to_string())),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BrowserOnly => "browser-only",
            Self::BrowserlessPreferred => "browserless-preferred",
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct BrowserlessCapabilities {
    pub supported: bool,
    pub modes: Vec<BrowserTransportMode>,
    pub recommended_mode: Option<BrowserTransportMode>,
    pub requires_auth_snapshot: bool,
    pub supports_browser_fallback: bool,
    pub supports_direct_model_discovery: bool,
    pub supports_native_conversation: bool,
}

impl BrowserlessCapabilities {
    pub fn unsupported() -> Self {
        Self {
            supported: false,
            modes: vec![BrowserTransportMode::BrowserOnly],
            recommended_mode: None,
            requires_auth_snapshot: false,
            supports_browser_fallback: false,
            supports_direct_model_discovery: false,
            supports_native_conversation: false,
        }
    }

    pub fn preferred(
        recommended_mode: BrowserTransportMode,
        supports_auto: bool,
        supports_direct_model_discovery: bool,
        supports_native_conversation: bool,
    ) -> Self {
        let mut modes = vec![
            BrowserTransportMode::BrowserOnly,
            BrowserTransportMode::HttpPreferred,
        ];
        if supports_auto {
            modes.push(BrowserTransportMode::Auto);
        }
        Self {
            supported: true,
            modes,
            recommended_mode: Some(recommended_mode),
            requires_auth_snapshot: true,
            supports_browser_fallback: true,
            supports_direct_model_discovery,
            supports_native_conversation,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserAccountTransportState {
    pub account_id: String,
    pub provider_kind: String,
    pub desired_policy: BrowserTransportPolicy,
    pub configured_mode: BrowserTransportMode,
    pub browserless: BrowserlessCapabilities,
    pub effective_transport: String,
    pub effective_adapter_id: Option<String>,
    pub browser_fallback: bool,
    pub effective_recorded_at: Option<String>,
    pub effective_reason: String,
    pub auth_state: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BrowserAccountBinding {
    pub session: String,
    #[serde(default)]
    pub transport_mode: BrowserTransportMode,
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
    #[serde(default)]
    pub first_byte_timeout_ms: Option<u64>,
    #[serde(default)]
    pub idle_stream_timeout_ms: Option<u64>,
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
    session_marker: Option<String>,
    diagnostics: BrowserAdapterDiagnostics,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserTransportExecution {
    pub transport: String,
    pub adapter_id: String,
    pub model: String,
    pub browser_fallback: bool,
    pub recorded_at: String,
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
    stream: Option<Value>,
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
    pub thread_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum BrowserProviderError {
    #[error("browser provider config error: {0}")]
    InvalidConfig(String),
    #[error("browser provider adapter '{0}' is not registered")]
    UnsupportedAdapter(String),
    #[error("browserless transport is not supported for provider '{0}'")]
    UnsupportedBrowserless(String),
    #[error("invalid browser transport policy '{0}'")]
    InvalidTransportPolicy(String),
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
    #[error("model_recipe_stale: browser model recipe for '{model}' is stale for account '{account_id}'")]
    ModelRecipeStale { account_id: String, model: String },
    #[error("browser provider transport error: {0}")]
    Transport(String),
    #[error("browser provider config read error: {0}")]
    Io(#[from] std::io::Error),
    #[error("browser provider config TOML error: {0}")]
    Toml(#[from] toml::de::Error),
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserDiscoveredModel {
    pub external_id: String,
    pub display_name: String,
    pub owned_by: String,
    pub context_window: Option<i64>,
    pub capabilities: Vec<String>,
}

#[async_trait]
pub trait BrowserProviderAdapter: Send + Sync {
    fn kind(&self) -> &'static str;
    fn adapter_id(&self) -> &'static str;
    fn is_cdp(&self) -> bool {
        false
    }

    fn supports_model_discovery(&self) -> bool {
        false
    }

    fn browserless_capabilities(&self) -> BrowserlessCapabilities {
        BrowserlessCapabilities::unsupported()
    }

    async fn discover_models(
        &self,
        _account_id: &str,
        _binding: &BrowserAccountBinding,
        _force: bool,
    ) -> Result<Vec<BrowserDiscoveredModel>, BrowserProviderError> {
        Err(BrowserProviderError::UnsupportedAdapter(format!(
            "{} model discovery",
            self.adapter_id()
        )))
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
    config: Arc<StdRwLock<BrowserProviderConfig>>,
    adapters: BTreeMap<String, Arc<dyn BrowserProviderAdapter>>,
    direct_adapters: BTreeMap<String, Arc<dyn BrowserProviderAdapter>>,
    adapter_health: Arc<RwLock<BTreeMap<String, CachedAdapterDiagnostics>>>,
    last_transport: Arc<RwLock<BTreeMap<String, BrowserTransportExecution>>>,
    discovered_models: Arc<StdRwLock<BTreeMap<String, BTreeSet<String>>>>,
    model_catalog_refresh_required: Arc<StdRwLock<BTreeSet<String>>>,
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
            if binding
                .first_byte_timeout_ms
                .is_some_and(|value| !(500..=120_000).contains(&value))
            {
                return Err(BrowserProviderError::InvalidConfig(format!(
                    "browser binding '{account}' first_byte_timeout_ms must be between 500 and 120000"
                )));
            }
            if binding
                .idle_stream_timeout_ms
                .is_some_and(|value| !(500..=120_000).contains(&value))
            {
                return Err(BrowserProviderError::InvalidConfig(format!(
                    "browser binding '{account}' idle_stream_timeout_ms must be between 500 and 120000"
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
        let chatgpt = Arc::new(CdpBrowserAdapter::chatgpt()?);
        let qwen = Arc::new(CdpBrowserAdapter::qwen()?);
        let gemini_http = Arc::new(GeminiWebHttpAdapter::new()?);
        let chatgpt_http = Arc::new(ChatGptWebHttpAdapter::new()?);
        let qwen_http = Arc::new(QwenWebHttpAdapter::new()?);
        let mut adapters: BTreeMap<String, Arc<dyn BrowserProviderAdapter>> = BTreeMap::new();
        adapters.insert(http.kind().to_string(), http);
        adapters.insert(cdp.kind().to_string(), cdp);
        adapters.insert(gemini.kind().to_string(), gemini);
        adapters.insert(chatgpt.kind().to_string(), chatgpt);
        adapters.insert(qwen.kind().to_string(), qwen);

        let mut direct_adapters: BTreeMap<String, Arc<dyn BrowserProviderAdapter>> =
            BTreeMap::new();
        direct_adapters.insert("browser-gemini".into(), gemini_http);
        direct_adapters.insert("browser-chatgpt".into(), chatgpt_http);
        direct_adapters.insert("browser-qwen".into(), qwen_http);
        Ok(Self {
            config: Arc::new(StdRwLock::new(config)),
            adapters,
            direct_adapters,
            adapter_health: Arc::new(RwLock::new(BTreeMap::new())),
            last_transport: Arc::new(RwLock::new(BTreeMap::new())),
            discovered_models: Arc::new(StdRwLock::new(BTreeMap::new())),
            model_catalog_refresh_required: Arc::new(StdRwLock::new(BTreeSet::new())),
        })
    }

    pub fn binding_count(&self) -> usize {
        self.config_snapshot().bindings.len()
    }

    pub fn reload(&self, config: BrowserProviderConfig) -> Result<(), BrowserProviderError> {
        let mut guard = self
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = config;
        Ok(())
    }

    pub async fn clear_diagnostics(&self) {
        self.adapter_health.write().await.clear();
    }

    pub async fn last_transport_execution(
        &self,
        account_id: &str,
    ) -> Option<BrowserTransportExecution> {
        self.last_transport.read().await.get(account_id).cloned()
    }

    async fn record_transport_execution(
        &self,
        account_id: &str,
        model: &str,
        adapter: &dyn BrowserProviderAdapter,
        browser_fallback: bool,
    ) {
        let transport = if adapter.is_cdp() {
            "browser-cdp"
        } else if matches!(
            adapter.adapter_id(),
            "gemini-web-http" | "chatgpt-web-http" | "qwen-web-http"
        ) {
            "direct-http"
        } else {
            "browser-http"
        };
        self.last_transport.write().await.insert(
            account_id.to_string(),
            BrowserTransportExecution {
                transport: transport.into(),
                adapter_id: adapter.adapter_id().into(),
                model: model.to_string(),
                browser_fallback,
                recorded_at: chrono::Utc::now().to_rfc3339(),
            },
        );
    }

    fn config_snapshot(&self) -> BrowserProviderConfig {
        self.config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn is_browser_kind(kind: &str) -> bool {
        kind.starts_with("browser-")
    }

    pub fn transport_capabilities(&self, provider_kind: &str) -> BrowserlessCapabilities {
        self.direct_adapters
            .get(provider_kind)
            .map(|adapter| adapter.browserless_capabilities())
            .unwrap_or_else(BrowserlessCapabilities::unsupported)
    }

    pub fn resolve_transport_policy(
        &self,
        provider_kind: &str,
        policy: BrowserTransportPolicy,
    ) -> Result<BrowserTransportMode, BrowserProviderError> {
        match policy {
            BrowserTransportPolicy::BrowserOnly => Ok(BrowserTransportMode::BrowserOnly),
            BrowserTransportPolicy::BrowserlessPreferred => self
                .transport_capabilities(provider_kind)
                .recommended_mode
                .ok_or_else(|| BrowserProviderError::UnsupportedBrowserless(provider_kind.into())),
        }
    }

    pub async fn account_transport_state(
        &self,
        provider_kind: &str,
        account_id: &str,
    ) -> Result<BrowserAccountTransportState, BrowserProviderError> {
        let binding = self
            .config_snapshot()
            .bindings
            .get(account_id)
            .cloned()
            .ok_or_else(|| BrowserProviderError::MissingBinding(account_id.to_string()))?;
        let browserless = self.transport_capabilities(provider_kind);
        let desired_policy = match binding.transport_mode {
            BrowserTransportMode::BrowserOnly => BrowserTransportPolicy::BrowserOnly,
            BrowserTransportMode::HttpPreferred => BrowserTransportPolicy::BrowserlessPreferred,
            BrowserTransportMode::Auto if browserless.modes.contains(&BrowserTransportMode::Auto) => {
                BrowserTransportPolicy::BrowserlessPreferred
            }
            BrowserTransportMode::Auto => BrowserTransportPolicy::BrowserOnly,
        };
        let auth_state = if browserless.requires_auth_snapshot
            && self.auth_material_available(&binding.session)
        {
            "captured"
        } else {
            "unavailable"
        }
        .to_string();
        let last = self.last_transport_execution(account_id).await;
        let (effective_transport, effective_adapter_id, browser_fallback, effective_recorded_at, effective_reason) =
            match last {
                Some(execution) if execution.browser_fallback => (
                    "browser-fallback".to_string(),
                    Some(execution.adapter_id),
                    true,
                    Some(execution.recorded_at),
                    "last request used browser fallback".to_string(),
                ),
                Some(execution) if execution.transport == "direct-http" => (
                    "direct-http".to_string(),
                    Some(execution.adapter_id),
                    false,
                    Some(execution.recorded_at),
                    "last request used direct transport".to_string(),
                ),
                Some(execution) if matches!(execution.transport.as_str(), "browser-cdp" | "browser-http") => (
                    "browser".to_string(),
                    Some(execution.adapter_id),
                    false,
                    Some(execution.recorded_at),
                    "last request used browser transport".to_string(),
                ),
                Some(execution) => (
                    execution.transport,
                    Some(execution.adapter_id),
                    execution.browser_fallback,
                    Some(execution.recorded_at),
                    "last request transport was recorded by the adapter".to_string(),
                ),
                None => (
                    "unavailable".to_string(),
                    None,
                    false,
                    None,
                    "no transport execution recorded since startup or policy change".to_string(),
                ),
            };
        Ok(BrowserAccountTransportState {
            account_id: account_id.to_string(),
            provider_kind: provider_kind.to_string(),
            desired_policy,
            configured_mode: binding.transport_mode,
            browserless,
            effective_transport,
            effective_adapter_id,
            browser_fallback,
            effective_recorded_at,
            effective_reason,
            auth_state,
        })
    }

    pub async fn clear_last_transport_execution(&self, account_id: &str) {
        self.last_transport.write().await.remove(account_id);
    }

    pub fn supports(&self, kind: &str) -> bool {
        self.adapters.contains_key(kind)
    }

     pub fn account_supports_model_discovery(
        &self,
        provider_kind: &str,
        account_id: &str,
    ) -> bool {
        let config = self.config_snapshot();
        let Some(binding) = config.bindings.get(account_id) else {
            return false;
        };
        self.direct_adapter(provider_kind, binding)
            .is_some_and(|adapter| adapter.supports_model_discovery())
    }

    pub async fn discover_models(
        &self,
        provider_kind: &str,
        account_id: &str,
        force: bool,
    ) -> Result<Vec<BrowserDiscoveredModel>, BrowserProviderError> {
        let config = self.config_snapshot();
        let binding = config
            .bindings
            .get(account_id)
            .cloned()
            .ok_or_else(|| BrowserProviderError::MissingBinding(account_id.to_string()))?;
        let adapter = self
            .direct_adapter(provider_kind, &binding)
            .cloned()
            .ok_or_else(|| {
                BrowserProviderError::UnsupportedAdapter(format!(
                    "{provider_kind} direct model discovery"
                ))
            })?;
        if !adapter.supports_model_discovery() {
            return Err(BrowserProviderError::UnsupportedAdapter(format!(
                "{} model discovery",
                adapter.adapter_id()
            )));
        }
        let models = adapter.discover_models(account_id, &binding, force).await?;
        self.remember_discovered_models(account_id, &models);
        self.clear_model_catalog_refresh_required(account_id);
        Ok(models)
    }

    pub fn model_catalog_refresh_required(&self, account_id: &str) -> bool {
        self.model_catalog_refresh_required
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(account_id)
    }

    pub fn mark_model_catalog_refresh_required(&self, account_id: &str) {
        self.model_catalog_refresh_required
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(account_id.to_string());
    }

    pub fn clear_model_catalog_refresh_required(&self, account_id: &str) {
        self.model_catalog_refresh_required
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(account_id);
    }

    fn remember_discovered_models(
        &self,
        account_id: &str,
        models: &[BrowserDiscoveredModel],
    ) {
        let mut guard = self
            .discovered_models
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.insert(
            account_id.to_string(),
            models
                .iter()
                .map(|model| model.external_id.clone())
                .collect(),
        );
    }

    pub fn session_id_for_account(&self, account_id: &str) -> Option<String> {
        self.config_snapshot()
            .bindings
            .get(account_id)
            .map(|binding| binding.session.clone())
    }

    pub fn model_allowed(&self, account_id: &str, model: &str) -> bool {
        let configured = self
            .config_snapshot()
            .bindings
            .get(account_id)
            .is_none_or(|binding| {
                binding.models.is_empty() || binding.models.iter().any(|allowed| allowed == model)
            });
        if configured {
            return true;
        }
        self.discovered_models
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(account_id)
            .is_some_and(|models| models.contains(model))
    }

    pub fn discovered_models_for_account(&self, account_id: &str) -> Vec<String> {
        self.discovered_models
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(account_id)
            .map(|models| models.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn auth_material_available(&self, session_id: &str) -> bool {
        browser_auth_runtime::get()
            .is_some_and(|vault| vault.contains(session_id))
    }

    fn direct_adapter(
        &self,
        provider_kind: &str,
        binding: &BrowserAccountBinding,
    ) -> Option<&Arc<dyn BrowserProviderAdapter>> {
        let adapter = self.direct_adapters.get(provider_kind)?;
        let capabilities = adapter.browserless_capabilities();
        let enabled = match binding.transport_mode {
            BrowserTransportMode::HttpPreferred => capabilities.supported,
            BrowserTransportMode::Auto => {
                capabilities.supported && capabilities.modes.contains(&BrowserTransportMode::Auto)
            }
            BrowserTransportMode::BrowserOnly => false,
        };
        enabled.then_some(adapter)
    }

    pub async fn adapter_diagnostics(
        &self,
        provider_kind: &str,
        account_id: &str,
    ) -> BrowserAdapterDiagnostics {
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
        let config = self.config_snapshot();
        let Some(binding) = config.bindings.get(account_id).cloned() else {
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
                    &binding,
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
                        &binding,
                        &error.to_string(),
                    ))
                    .await;
            }
        };
        if !session.enabled {
            self.invalidate_diagnostics(account_id).await;
            return unavailable_diagnostics(
                account_id,
                provider_kind,
                adapter.adapter_id(),
                &binding,
                "browser session is disabled",
            );
        }
        let direct_adapter = self.direct_adapter(provider_kind, &binding).cloned();
        let direct_snapshot_ready = direct_adapter.is_some()
            && self.auth_material_available(&binding.session);
        let auth_snapshot_ready = (provider_kind == "browser-http"
            && self.auth_material_available(&binding.session))
            || direct_snapshot_ready;
        let can_reprobe = adapter.is_cdp()
            && cdp_session_status_probeable(&session.status)
            && self.cdp_session_live(&binding.session).await;
        if session.status != "ready" && !auth_snapshot_ready && !can_reprobe {
            // Session lifecycle can transition immediately during login/re-auth.
            // Drop any prior ready probe so a later verified generation must be
            // diagnosed again rather than borrowing stale adapter health.
            self.invalidate_diagnostics(account_id).await;
            return unavailable_diagnostics(
                account_id,
                provider_kind,
                adapter.adapter_id(),
                &binding,
                &format!("browser session is {}", session.status),
            );
        }

        let mut session_marker = session.updated_at.clone();
        if let Some(cached) = self.adapter_health.read().await.get(account_id).cloned() {
            let cache_ttl = if session.status == "ready" || auth_snapshot_ready {
                Duration::from_secs(ADAPTER_HEALTH_TTL_SECONDS)
            } else {
                // Recoverable non-ready states must re-probe quickly after the user
                // completes login or the provider page repairs itself.
                Duration::from_secs(2)
            };
            if cached.checked_at.elapsed() < cache_ttl
                && cached.session_marker == session_marker
                && cached.diagnostics.provider_kind == provider_kind
            {
                return cached.diagnostics;
            }
        }

        let diagnostics = if direct_snapshot_ready {
            let direct = direct_adapter.expect("direct adapter checked above");
            let direct_diagnostics = direct
                .diagnose(account_id, &session.profile_dir, &binding)
                .await;
            if direct_diagnostics.status == "ready" && direct.supports_model_discovery() {
                match direct.discover_models(account_id, &binding, false).await {
                    Ok(models) => self.remember_discovered_models(account_id, &models),
                    Err(error) => warn!(
                        %error,
                        account = %account_id,
                        adapter = %direct.adapter_id(),
                        "browser direct model discovery warmup failed"
                    ),
                }
            }
            if direct_diagnostics.status == "ready" || !can_reprobe {
                direct_diagnostics
            } else {
                adapter
                    .diagnose(account_id, &session.profile_dir, &binding)
                    .await
            }
        } else {
            adapter
                .diagnose(account_id, &session.profile_dir, &binding)
                .await
        };

        if diagnostics.status == "ready"
            && session.status != "ready"
            && !auth_snapshot_ready
        {
            if let Ok(recovered) = store.mark_ready(&binding.session).await {
                session_marker = recovered.updated_at;
            }
        } else if diagnostics.status == "login_required" && session.status != "login_required" {
            if let Ok(updated) = store
                .mark_login_required(&binding.session, Some(&diagnostics.message))
                .await
            {
                session_marker = updated.updated_at;
            }
        }

        self.cache_session_diagnostics(diagnostics, session_marker).await
    }

    async fn cache_diagnostics(
        &self,
        diagnostics: BrowserAdapterDiagnostics,
    ) -> BrowserAdapterDiagnostics {
        self.adapter_health.write().await.insert(
            diagnostics.account_id.clone(),
            CachedAdapterDiagnostics {
                checked_at: Instant::now(),
                session_marker: None,
                diagnostics: diagnostics.clone(),
            },
        );
        diagnostics
    }

    async fn cache_session_diagnostics(
        &self,
        diagnostics: BrowserAdapterDiagnostics,
        session_marker: Option<String>,
    ) -> BrowserAdapterDiagnostics {
        self.adapter_health.write().await.insert(
            diagnostics.account_id.clone(),
            CachedAdapterDiagnostics {
                checked_at: Instant::now(),
                session_marker,
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
        let config = self.config_snapshot();
        let Some(binding) = config.bindings.get(account_id).cloned() else {
            return false;
        };
        if provider_kind == "browser-cdp" && binding.adapter_script.is_none() {
            return false;
        }
        let Some(store) = browser_session_runtime::get() else {
            return false;
        };
        let session = match store.session(&binding.session).await {
            Ok(session) => session,
            Err(_) => return false,
        };
        if !session.enabled {
            return false;
        }

        if provider_kind == "browser-http" && self.auth_material_available(&binding.session) {
            return true;
        }

        if self.direct_adapter(provider_kind, &binding).is_some()
            && self.auth_material_available(&binding.session)
        {
            let diagnostics = self.adapter_diagnostics(provider_kind, account_id).await;
            if matches!(
                diagnostics.status.as_str(),
                "ready" | "browser_fallback_required"
            ) {
                return true;
            }
        }

        let adapter = match self.adapters.get(provider_kind) {
            Some(adapter) => adapter,
            None => return false,
        };
        if adapter.is_cdp() {
            if !cdp_session_status_probeable(&session.status) {
                return false;
            }
            if !self.cdp_session_live(&binding.session).await {
                return false;
            }
            let diagnostics = self.adapter_diagnostics(provider_kind, account_id).await;
            return diagnostics.status == "ready";
        }

        session.status == "ready"
    }

    async fn cdp_session_live(&self, session_id: &str) -> bool {
        let Some(driver) = chromium_driver_runtime::get() else {
            return false;
        };
        driver.status(session_id).await.is_ok_and(|status| {
            status.running && status.debugger_reachable && status.ready_match.is_some()
        })
    }

    async fn ensure_cdp_session_ready(&self, session_id: &str) -> bool {
        if self.cdp_session_live(session_id).await {
            return true;
        }
        let Some(driver) = chromium_driver_runtime::get() else {
            return false;
        };

        if driver.launch(session_id).await.is_err()
            && !driver
                .status(session_id)
                .await
                .is_ok_and(|status| status.running)
        {
            return false;
        }

        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if driver
                .verify(session_id)
                .await
                .is_ok_and(|verification| verification.authenticated)
            {
                return true;
            }
            sleep(Duration::from_millis(250)).await;
        }
        false
    }

    async fn mark_direct_state_unsynced(
        &self,
        request: &BrowserAdapterRequest,
    ) -> Result<(), BrowserProviderError> {
        let Some(thread_id) = request.thread_id.as_deref() else {
            return Ok(());
        };
        let Some(store) = conversation_runtime::get() else {
            return Ok(());
        };
        store
            .upsert_provider_conversation_state(
                thread_id,
                &request.provider.id,
                &request.account.id,
                &json!({
                    "transport": "browser-cdp",
                    "needs_resync": true
                }),
            )
            .await
            .map_err(|error| BrowserProviderError::Transport(format!(
                "failed to mark provider state unsynced before browser fallback: {error}"
            )))
    }

    pub async fn mark_degraded(
        &self,
        account_id: &str,
        error: &str,
    ) -> Result<(), BrowserProviderError> {
        self.invalidate_diagnostics(account_id).await;
        let binding = self
            .config_snapshot()
            .bindings
            .get(account_id)
            .cloned()
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

    pub async fn mark_login_required(
        &self,
        account_id: &str,
        error: &str,
    ) -> Result<(), BrowserProviderError> {
        self.invalidate_diagnostics(account_id).await;
        let binding = self
            .config_snapshot()
            .bindings
            .get(account_id)
            .cloned()
            .ok_or_else(|| BrowserProviderError::MissingBinding(account_id.to_string()))?;
        let Some(store) = browser_session_runtime::get() else {
            return Err(BrowserProviderError::SessionUnavailable {
                account_id: account_id.to_string(),
                session_id: binding.session.clone(),
            });
        };
        store
            .mark_login_required(&binding.session, Some(error))
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
            .config_snapshot()
            .bindings
            .get(account_id)
            .cloned()
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
        thread_id: Option<&str>,
    ) -> Result<reqwest::Response, BrowserProviderError> {
        let browser_adapter = self
            .adapters
            .get(&provider.kind)
            .cloned()
            .ok_or_else(|| BrowserProviderError::UnsupportedAdapter(provider.kind.clone()))?;
        let binding = self
            .config_snapshot()
            .bindings
            .get(&account.id)
            .cloned()
            .ok_or_else(|| BrowserProviderError::MissingBinding(account.id.clone()))?;

        if !self.model_allowed(&account.id, &route.model)
            && self.account_supports_model_discovery(&provider.kind, &account.id)
        {
            // The SQLite catalog survives process restarts while the registry's fast
            // discovered-model allow-list is intentionally in-memory. Rehydrate that
            // allow-list through the direct, non-generating discovery RPC before
            // rejecting a physical model selected from the persisted catalog.
            self.discover_models(&provider.kind, &account.id, false).await?;
        }
        if !self.model_allowed(&account.id, &route.model) {
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

        let direct_adapter = self.direct_adapter(&provider.kind, &binding).cloned();
        let direct_snapshot_ready = direct_adapter.is_some()
            && self.auth_material_available(&binding.session);
        let auth_snapshot_ready = (provider.kind == "browser-http"
            && self.auth_material_available(&binding.session))
            || direct_snapshot_ready;
        if !session.enabled || (session.status != "ready" && !auth_snapshot_ready) {
            return Err(BrowserProviderError::SessionUnavailable {
                account_id: account.id.clone(),
                session_id: binding.session.clone(),
            });
        }

        let adapter_request = BrowserAdapterRequest {
            provider: provider.clone(),
            account: account.clone(),
            route: route.clone(),
            body: body.clone(),
            session_id: binding.session.clone(),
            profile_dir: session.profile_dir.clone(),
            binding: binding.clone(),
            thread_id: thread_id.map(str::to_string),
        };

        let mut used_adapter = browser_adapter.clone();
        let mut browser_fallback_used = false;
        let result = if direct_snapshot_ready {
            let direct = direct_adapter.expect("direct adapter checked above");
            used_adapter = direct.clone();
            let direct_result = direct.execute_chat(adapter_request.clone()).await;
            let dynamic_model = self
                .discovered_models
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&account.id)
                .is_some_and(|models| models.contains(&route.model))
                && !binding.models.iter().any(|model| model == &route.model);
            let safe_fallback_candidate = direct_result.as_ref().err().is_some_and(|error| {
                direct_error_allows_browser_fallback(
                    error,
                    dynamic_model,
                    browser_adapter.is_cdp(),
                )
            });
            let browser_was_live = safe_fallback_candidate
                && self.cdp_session_live(&binding.session).await;
            let safe_browser_fallback = if safe_fallback_candidate {
                self.ensure_cdp_session_ready(&binding.session).await
            } else {
                false
            };

            if safe_browser_fallback {
                browser_fallback_used = true;
                used_adapter = browser_adapter.clone();
                if let Err(error) = self.mark_direct_state_unsynced(&adapter_request).await {
                    if !browser_was_live {
                        stop_browser_runtime_soon(binding.session.clone());
                    }
                    Err(error)
                } else {
                    let fallback_result = browser_adapter.execute_chat(adapter_request).await;
                    if browser_was_live {
                        fallback_result
                    } else {
                        match fallback_result {
                            Ok(response) => wrap_response_with_browser_stop(
                                response,
                                binding.session.clone(),
                            ),
                            Err(error) => {
                                stop_browser_runtime_soon(binding.session.clone());
                                Err(error)
                            }
                        }
                    }
                }
            } else {
                direct_result
            }
        } else {
            browser_adapter.execute_chat(adapter_request).await
        };

        match &result {
            Ok(_) => {
                self.record_transport_execution(
                    &account.id,
                    &route.model,
                    used_adapter.as_ref(),
                    browser_fallback_used,
                )
                .await;
                self.invalidate_diagnostics(&account.id).await;
            }
            Err(BrowserProviderError::AdapterIncompatible { code, message, .. }) => {
                let status = if code == "login_required" {
                    let _ = self
                        .mark_login_required(
                            &account.id,
                            &format!("browserless adapter login required: {message}"),
                        )
                        .await;
                    "login_required"
                } else if code == "browser_challenge_required" {
                    "browser_fallback_required"
                } else {
                    "adapter_incompatible"
                };
                self.cache_diagnostics(BrowserAdapterDiagnostics {
                    account_id: account.id.clone(),
                    provider_kind: provider.kind.clone(),
                    adapter_id: Some(used_adapter.adapter_id().to_string()),
                    adapter_version: None,
                    contract_version: None,
                    expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                    status: status.into(),
                    message: format!("{code}: {message}"),
                    page_signature: None,
                    target_url_prefix: effective_target_url_prefix(&provider.kind, &binding),
                    configured_models: binding.models.clone(),
                })
                .await;
            }
            Err(BrowserProviderError::SessionUnavailable { .. })
            | Err(BrowserProviderError::Transport(_))
                if used_adapter.is_cdp() =>
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


struct BrowserFallbackStopGuard {
    session_id: Option<String>,
}

impl BrowserFallbackStopGuard {
    fn new(session_id: String) -> Self {
        Self {
            session_id: Some(session_id),
        }
    }
}

impl Drop for BrowserFallbackStopGuard {
    fn drop(&mut self) {
        if let Some(session_id) = self.session_id.take() {
            stop_browser_runtime_soon(session_id);
        }
    }
}

fn stop_browser_runtime_soon(session_id: String) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        if let Some(driver) = chromium_driver_runtime::get() {
            let _ = driver.stop(&session_id).await;
        }
    });
}

fn wrap_response_with_browser_stop(
    response: reqwest::Response,
    session_id: String,
) -> Result<reqwest::Response, BrowserProviderError> {
    let status = response.status();
    let headers = response.headers().clone();
    // Construct the guard outside the generator so the response owns it even if
    // the body is dropped before its first poll.
    let guard = BrowserFallbackStopGuard::new(session_id);
    let stream = async_stream::stream! {
        let _guard = guard;
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(bytes) => yield Ok::<Bytes, std::io::Error>(bytes),
                Err(error) => {
                    yield Err(std::io::Error::other(error.to_string()));
                    break;
                }
            }
        }
    };
    let mut wrapped = HttpResponse::builder()
        .status(status)
        .body(reqwest::Body::wrap_stream(stream))
        .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
    *wrapped.headers_mut() = headers;
    Ok(reqwest::Response::from(wrapped))
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
        let mut upstream = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header("x-llmgateway-browser-session", &request.session_id)
            .header("x-llmgateway-browser-account", &request.account.id)
            .header("x-llmgateway-route", &request.route.id);

        if let Some(vault) = browser_auth_runtime::get() {
            if let Ok(material) = vault.load(&request.session_id) {
                let cookie_header = material.cookie_header();
                if !cookie_header.is_empty() {
                    upstream = upstream.header(COOKIE, cookie_header);
                }
                if !material.user_agent.trim().is_empty() {
                    upstream = upstream.header(USER_AGENT, material.user_agent);
                }
            }
        }

        upstream
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

#[derive(Clone, Debug, Deserialize)]
struct CdpStreamStart {
    stream_id: String,
    #[serde(default = "default_ok_status")]
    status: u16,
    #[serde(default = "default_sse_content_type")]
    content_type: String,
}

#[derive(Clone, Debug, Deserialize)]
struct CdpStreamPoll {
    #[serde(default)]
    events: Vec<Value>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    error: Option<AdapterContractError>,
    #[serde(default)]
    progress_seq: Option<u64>,
}

struct CdpStreamCleanup {
    adapter: CdpBrowserAdapter,
    target: CdpTarget,
    provider_id: String,
    account_id: String,
    thread_id: Option<String>,
    stream_id: String,
    profile_dir: String,
    adapter_script: String,
    ephemeral: bool,
    armed: bool,
}

impl CdpStreamCleanup {
    async fn complete(&mut self) {
        self.armed = false;
        if self.ephemeral {
            self.adapter
                .close_target(&self.profile_dir, &self.target.id)
                .await;
        }
    }
}

impl Drop for CdpStreamCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let adapter = self.adapter.clone();
        let target = self.target.clone();
        let provider_id = self.provider_id.clone();
        let account_id = self.account_id.clone();
        let thread_id = self.thread_id.clone();
        let stream_id = self.stream_id.clone();
        let profile_dir = self.profile_dir.clone();
        let adapter_script = self.adapter_script.clone();
        let ephemeral = self.ephemeral;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = adapter
                    .evaluate_stream_control(
                        &target,
                        &adapter_script,
                        "chat_stream_cancel",
                        &json!({"stream_id": stream_id}),
                        &account_id,
                    )
                    .await;
                if ephemeral || thread_id.is_some() {
                    adapter.close_target(&profile_dir, &target.id).await;
                }
                if let (Some(thread_id), Some(store)) = (thread_id, conversation_runtime::get()) {
                    if let Err(error) = store
                        .delete_provider_conversation(&thread_id, &provider_id, &account_id)
                        .await
                    {
                        warn!(%error, %thread_id, %provider_id, %account_id, "failed to clear native conversation affinity after browser stream abort");
                    }
                }
            });
        }
    }
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

    fn chatgpt() -> Result<Self, BrowserProviderError> {
        Self::new(CdpAdapterSpec {
            kind: "browser-chatgpt",
            adapter_id: "chatgpt-web",
            provider: "chatgpt",
            builtin_script: Some(CHATGPT_WEB_ADAPTER),
            default_target_url_prefix: Some("https://chatgpt.com/"),
            new_chat_url: Some("https://chatgpt.com/"),
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
        let configured_probe_timeout = binding.probe_timeout_ms.unwrap_or(8_000);
        let probe_timeout_ms = if self.spec.builtin_script.is_some() {
            configured_probe_timeout.max(10_000)
        } else {
            configured_probe_timeout
        };
        json!({
            "provider": self.spec.provider,
            "adapter_id": self.spec.adapter_id,
            "contract_version": BROWSER_ADAPTER_CONTRACT_VERSION,
            "model_label": model.and_then(|value| self.model_label(binding, value)),
            "selectors": binding.selector_overrides,
            "probe_timeout_ms": probe_timeout_ms,
            "response_timeout_ms": binding.response_timeout_ms.unwrap_or(180_000),
            "first_byte_timeout_ms": binding.first_byte_timeout_ms.unwrap_or(30_000),
            "idle_stream_timeout_ms": binding.idle_stream_timeout_ms.unwrap_or(30_000),
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

    async fn open_target_url(
        &self,
        profile_dir: &str,
        url: &str,
    ) -> Result<CdpTarget, BrowserProviderError> {
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
        self.open_target_url(profile_dir, url).await
    }

    fn supports_native_conversation_affinity(&self) -> bool {
        matches!(self.spec.kind, "browser-gemini" | "browser-chatgpt")
    }

    async fn discover_native_conversation_url(
        &self,
        profile_dir: &str,
        target: &CdpTarget,
        timeout_duration: Duration,
    ) -> Option<String> {
        let new_chat_url = self.spec.new_chat_url?;
        let deadline = Instant::now() + timeout_duration;
        loop {
            if !target.websocket_debugger_url.is_empty() {
                let runtime_href = timeout(
                    Duration::from_secs(2),
                    evaluate_cdp(
                        &target.websocket_debugger_url,
                        "String(globalThis.location?.href || '')",
                    ),
                )
                .await;
                if let Ok(Ok(Value::String(href))) = runtime_href {
                    warn!(
                        target_id = %target.id,
                        href = %diagnostic_target_location(&href),
                        native = is_native_conversation_url(new_chat_url, &href),
                        "inspected browser runtime URL for native conversation"
                    );
                    if is_native_conversation_url(new_chat_url, &href) {
                        return Some(href);
                    }
                }
            }

            if let Ok(targets) = self.targets(profile_dir).await {
                if let Some(refreshed) = targets.into_iter().find(|item| item.id == target.id) {
                    warn!(
                        target_id = %target.id,
                        url = %diagnostic_target_location(&refreshed.url),
                        native = is_native_conversation_url(new_chat_url, &refreshed.url),
                        "inspected CDP target URL for native conversation"
                    );
                    if is_native_conversation_url(new_chat_url, &refreshed.url) {
                        return Some(refreshed.url);
                    }
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    async fn find_open_conversation_target(
        &self,
        profile_dir: &str,
        conversation_url: &str,
    ) -> Result<Option<CdpTarget>, BrowserProviderError> {
        let targets = self.targets(profile_dir).await?;
        for target in targets.into_iter().filter(|target| {
            target.kind == "page" && !target.websocket_debugger_url.is_empty()
        }) {
            if same_conversation_url(&target.url, conversation_url) {
                return Ok(Some(target));
            }
            let runtime_href = timeout(
                Duration::from_secs(1),
                evaluate_cdp(
                    &target.websocket_debugger_url,
                    "String(globalThis.location?.href || '')",
                ),
            )
            .await;
            if let Ok(Ok(Value::String(href))) = runtime_href {
                if same_conversation_url(&href, conversation_url) {
                    return Ok(Some(CdpTarget {
                        url: href,
                        ..target
                    }));
                }
            }
        }
        Ok(None)
    }

    async fn persist_native_conversation(
        &self,
        request: &BrowserAdapterRequest,
        target: &CdpTarget,
        wait: Duration,
    ) -> bool {
        if !self.supports_native_conversation_affinity() {
            return false;
        }
        let Some(thread_id) = request.thread_id.as_deref() else {
            warn!(account = %request.account.id, "native conversation persistence skipped because logical thread id is missing");
            return false;
        };
        let Some(store) = conversation_runtime::get() else {
            warn!(thread_id, account = %request.account.id, "native conversation persistence skipped because conversation runtime is unavailable");
            return false;
        };
        let Some(conversation_url) = self
            .discover_native_conversation_url(&request.profile_dir, target, wait)
            .await
        else {
            return false;
        };
        warn!(
            thread_id,
            account = %request.account.id,
            url = %diagnostic_target_location(&conversation_url),
            "observed browser provider native conversation URL"
        );
        match store
            .upsert_provider_conversation(
                thread_id,
                &request.provider.id,
                &request.account.id,
                &conversation_url,
            )
            .await
        {
            Ok(()) => {
                warn!(
                    thread_id,
                    account = %request.account.id,
                    "persisted browser provider native conversation affinity"
                );
                true
            },
            Err(error) => {
                warn!(
                    %error,
                    thread_id,
                    account = %request.account.id,
                    "failed to persist browser provider native conversation affinity"
                );
                false
            }
        }
    }

    async fn wait_for_target_navigation(
        &self,
        profile_dir: &str,
        initial: CdpTarget,
        prefix: Option<&str>,
        timeout_duration: Duration,
    ) -> Result<CdpTarget, BrowserProviderError> {
        let Some(prefix) = prefix else {
            return Ok(initial);
        };
        if initial.id.trim().is_empty() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: "<unknown>".into(),
                code: "invalid_target".into(),
                message: "DevTools created a browser target without an id".into(),
            });
        }

        let expected_host = Url::parse(prefix)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string));
        let target_id = initial.id.clone();
        let mut current = initial;
        let mut last_runtime_host = String::new();
        let deadline = Instant::now() + timeout_duration;

        loop {
            if current.kind == "page"
                && !current.websocket_debugger_url.is_empty()
                && target_url_matches_prefix(&current.url, prefix)
            {
                let runtime_host = timeout(
                    Duration::from_secs(2),
                    evaluate_cdp(
                        &current.websocket_debugger_url,
                        "String(globalThis.location?.hostname || '')",
                    ),
                )
                .await;

                if let Ok(Ok(Value::String(host))) = runtime_host {
                    last_runtime_host = host.clone();
                    if expected_host.as_deref().is_none_or(|expected| host == expected) {
                        return Ok(current);
                    }
                }
            }

            if Instant::now() >= deadline {
                let runtime = if last_runtime_host.trim().is_empty() {
                    "<empty>"
                } else {
                    last_runtime_host.as_str()
                };
                return Err(BrowserProviderError::AdapterIncompatible {
                    account_id: "<unknown>".into(),
                    code: "target_navigation_timeout".into(),
                    message: format!(
                        "new browser tab did not become runtime-ready for '{prefix}' before adapter injection (target page: {}; runtime host: {runtime})",
                        diagnostic_target_location(&current.url)
                    ),
                });
            }

            let targets = self.targets(profile_dir).await?;
            if let Some(refreshed) = targets.into_iter().find(|target| target.id == target_id) {
                current = refreshed;
            }
            sleep(Duration::from_millis(120)).await;
        }
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

    async fn select_runtime_ready_target(
        &self,
        profile_dir: &str,
        prefix: Option<&str>,
        timeout_duration: Duration,
    ) -> Result<CdpTarget, BrowserProviderError> {
        let Some(prefix) = prefix else {
            let targets = self.targets(profile_dir).await?;
            return self
                .select_target(&targets, None)
                .cloned()
                .ok_or_else(|| BrowserProviderError::AdapterIncompatible {
                    account_id: "<unknown>".into(),
                    code: "target_not_found".into(),
                    message: "No browser page target is available for adapter execution".into(),
                });
        };

        let expected_host = Url::parse(prefix)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string));
        let deadline = Instant::now() + timeout_duration;
        let mut saw_matching_target = false;
        let mut last_url = String::new();
        let mut last_runtime_host = String::new();

        loop {
            let targets = self.targets(profile_dir).await?;
            for target in targets.into_iter().filter(|target| {
                target.kind == "page"
                    && !target.websocket_debugger_url.is_empty()
                    && target_url_matches_prefix(&target.url, prefix)
            }) {
                saw_matching_target = true;
                last_url = target.url.clone();
                let runtime_host = timeout(
                    Duration::from_secs(1),
                    evaluate_cdp(
                        &target.websocket_debugger_url,
                        "String(globalThis.location?.hostname || '')",
                    ),
                )
                .await;

                if let Ok(Ok(Value::String(host))) = runtime_host {
                    last_runtime_host = host.clone();
                    if expected_host.as_deref().is_none_or(|expected| host == expected) {
                        return Ok(target);
                    }
                }
            }

            if Instant::now() >= deadline {
                if !saw_matching_target {
                    return Err(BrowserProviderError::AdapterIncompatible {
                        account_id: "<unknown>".into(),
                        code: "target_not_found".into(),
                        message: format!(
                            "No authenticated provider page matched the configured target URL '{prefix}'"
                        ),
                    });
                }

                let runtime = if last_runtime_host.trim().is_empty() {
                    "<empty>"
                } else {
                    last_runtime_host.as_str()
                };
                return Err(BrowserProviderError::AdapterIncompatible {
                    account_id: "<unknown>".into(),
                    code: "target_runtime_not_ready".into(),
                    message: format!(
                        "provider page metadata matched '{prefix}', but no matching CDP target became runtime-ready (target page: {}; runtime host: {runtime})",
                        diagnostic_target_location(&last_url)
                    ),
                });
            }

            sleep(Duration::from_millis(120)).await;
        }
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

    async fn evaluate_stream_control(
        &self,
        target: &CdpTarget,
        script: &str,
        operation: &str,
        request: &Value,
        account_id: &str,
    ) -> Result<AdapterContractEnvelope, BrowserProviderError> {
        let expression = build_stream_control_expression(script, operation, request)?;
        let value = evaluate_cdp(&target.websocket_debugger_url, &expression).await?;
        let envelope: AdapterContractEnvelope = serde_json::from_value(value).map_err(|error| {
            BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "invalid_stream_contract_envelope".into(),
                message: error.to_string(),
            }
        })?;
        validate_contract_envelope(account_id, self.spec, &envelope)?;
        Ok(envelope)
    }

    async fn execute_streaming_chat(
        &self,
        request: &BrowserAdapterRequest,
        target: CdpTarget,
        script: String,
        context: Value,
        normalized_body: Value,
        ephemeral: bool,
    ) -> Result<reqwest::Response, BrowserProviderError> {
        let envelope = match self
            .evaluate_contract(
                &target,
                &script,
                "chat_stream_start",
                Some(&normalized_body),
                &context,
                &request.account.id,
            )
            .await
        {
            Ok(envelope) => envelope,
            Err(error) => {
                if ephemeral {
                    self.close_target(&request.profile_dir, &target.id).await;
                }
                return Err(error);
            }
        };
        if let Some(error) = envelope.error {
            let result = contract_error_to_provider_error(
                &request.account.id,
                &request.route.model,
                error,
            );
            if ephemeral {
                self.close_target(&request.profile_dir, &target.id).await;
            }
            return result;
        }
        let stream_value = match envelope.stream {
            Some(stream) => stream,
            None => {
                if ephemeral {
                    self.close_target(&request.profile_dir, &target.id).await;
                }
                return Err(BrowserProviderError::AdapterIncompatible {
                    account_id: request.account.id.clone(),
                    code: "missing_stream_start".into(),
                    message: "adapter did not return browser stream start metadata".into(),
                });
            }
        };
        let start: CdpStreamStart = match serde_json::from_value(stream_value) {
            Ok(start) => start,
            Err(error) => {
                if ephemeral {
                    self.close_target(&request.profile_dir, &target.id).await;
                }
                return Err(BrowserProviderError::AdapterIncompatible {
                    account_id: request.account.id.clone(),
                    code: "invalid_stream_start".into(),
                    message: error.to_string(),
                });
            }
        };
        if start.stream_id.trim().is_empty() {
            if ephemeral {
                self.close_target(&request.profile_dir, &target.id).await;
            }
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "invalid_stream_start".into(),
                message: "browser stream id cannot be empty".into(),
            });
        }

        warn!(
            thread_id = ?request.thread_id,
            provider_kind = self.spec.kind,
            account = %request.account.id,
            "evaluating streaming native conversation affinity"
        );
        let native_affinity_request =
            (request.thread_id.is_some() && self.supports_native_conversation_affinity())
                .then(|| request.clone());
        let status = match reqwest::StatusCode::from_u16(start.status) {
            Ok(status) => status,
            Err(error) => {
                if ephemeral {
                    self.close_target(&request.profile_dir, &target.id).await;
                }
                return Err(BrowserProviderError::Transport(format!(
                    "invalid browser stream HTTP status: {error}"
                )));
            }
        };
        let adapter = self.clone();
        let account_id = request.account.id.clone();
        let stream_id = start.stream_id.clone();
        let profile_dir = request.profile_dir.clone();
        let first_byte_timeout = Duration::from_millis(
            request.binding.first_byte_timeout_ms.unwrap_or(30_000),
        );
        let idle_stream_timeout = Duration::from_millis(
            request.binding.idle_stream_timeout_ms.unwrap_or(30_000),
        );
        let stream_target = target.clone();
        let stream_native_request = native_affinity_request.clone();
        let stream_provider_id = request.provider.id.clone();
        let stream_thread_id = request.thread_id.clone();

        let stream = async_stream::stream! {
            let mut cleanup = CdpStreamCleanup {
                adapter: adapter.clone(),
                target: stream_target.clone(),
                provider_id: stream_provider_id,
                account_id: account_id.clone(),
                thread_id: stream_thread_id,
                stream_id: stream_id.clone(),
                profile_dir,
                adapter_script: script.clone(),
                ephemeral,
                armed: true,
            };
            let mut saw_event = false;
            let mut saw_assistant_output = false;
            let mut last_progress = Instant::now();
            let mut last_progress_seq = None;

            loop {
                let limit = if saw_event {
                    idle_stream_timeout
                } else {
                    first_byte_timeout
                };
                let elapsed = last_progress.elapsed();
                if elapsed >= limit {
                    let kind = if saw_event { "idle stream" } else { "first byte" };
                    warn!(%kind, "browser stream timed out");
                    yield Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("browser {kind} timeout"),
                    ));
                    break;
                }
                let remaining = limit.saturating_sub(elapsed);
                let polled = timeout(
                    remaining,
                    adapter.evaluate_stream_control(
                        &stream_target,
                        &script,
                        "chat_stream_poll",
                        &json!({"stream_id": stream_id}),
                        &account_id,
                    ),
                )
                .await;

                let envelope = match polled {
                    Ok(Ok(envelope)) => envelope,
                    Ok(Err(error)) => {
                        warn!(%error, "browser stream poll failed");
                        yield Err(std::io::Error::other(error.to_string()));
                        break;
                    }
                    Err(_) => {
                        let kind = if saw_event { "idle stream" } else { "first byte" };
                        warn!(%kind, "browser stream poll timed out");
                        yield Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!("browser {kind} timeout"),
                        ));
                        break;
                    }
                };

                if let Some(error) = envelope.error {
                    warn!(code = %error.code, message = %error.message, "browser stream adapter returned an error");
                    yield Err(std::io::Error::other(format!(
                        "{}: {}",
                        error.code, error.message
                    )));
                    break;
                }
                let Some(stream_value) = envelope.stream else {
                    warn!("browser stream adapter poll returned no stream payload");
                    yield Err(std::io::Error::other(
                        "browser stream poll returned no stream payload",
                    ));
                    break;
                };
                let poll: CdpStreamPoll = match serde_json::from_value(stream_value) {
                    Ok(poll) => poll,
                    Err(error) => {
                        warn!(%error, "browser stream adapter returned an invalid poll payload");
                        yield Err(std::io::Error::other(format!(
                            "invalid browser stream poll payload: {error}"
                        )));
                        break;
                    }
                };
                if let Some(error) = poll.error {
                    warn!(code = %error.code, message = %error.message, "browser stream job returned an error");
                    yield Err(std::io::Error::other(format!(
                        "{}: {}",
                        error.code, error.message
                    )));
                    break;
                }

                if browser_stream_poll_advanced(&poll, &mut last_progress_seq) {
                    last_progress = Instant::now();
                }

                let poll_done = poll.done;
                let mut encoded_events = Vec::with_capacity(poll.events.len());
                if !poll.events.is_empty() {
                    saw_event = true;
                    last_progress = Instant::now();
                    for event in &poll.events {
                        let has_assistant_output = browser_stream_event_has_assistant_output(event);
                        if !has_assistant_output {
                            warn!(event = %event, "browser stream event has no assistant output");
                        }
                        saw_assistant_output |= has_assistant_output;
                        match serde_json::to_string(event) {
                            Ok(data) => {
                                encoded_events.push(Bytes::from(format!("data: {data}\n\n")));
                            }
                            Err(error) => {
                                warn!(%error, "browser stream event could not be serialized");
                                yield Err(std::io::Error::other(error.to_string()));
                                return;
                            }
                        }
                    }
                }

                if poll_done {
                    if !saw_assistant_output {
                        warn!("browser stream completed without assistant output");
                        yield Err(std::io::Error::other(
                            "browser stream completed without assistant output",
                        ));
                        break;
                    }
                    if let Some(native_request) = stream_native_request.as_ref() {
                        let persisted = adapter
                            .persist_native_conversation(
                                native_request,
                                &stream_target,
                                Duration::from_secs(5),
                            )
                            .await;
                        if !persisted {
                            warn!(
                                thread_id = native_request.thread_id.as_deref().unwrap_or("<unknown>"),
                                account = %native_request.account.id,
                                "browser provider native conversation URL was still unavailable when the stream completed"
                            );
                        }
                    }

                    // Finish browser-side cleanup before exposing the logical terminal event.
                    // Some OpenAI-compatible clients stop reading as soon as finish_reason is
                    // non-null. Emit terminal events and [DONE] in one body chunk so a normal
                    // client close cannot look like downstream cancellation.
                    cleanup.complete().await;
                    let terminal_len = encoded_events.iter().map(|bytes| bytes.len()).sum::<usize>()
                        + b"data: [DONE]\n\n".len();
                    let mut terminal = Vec::with_capacity(terminal_len);
                    for bytes in encoded_events {
                        terminal.extend_from_slice(&bytes);
                    }
                    terminal.extend_from_slice(b"data: [DONE]\n\n");
                    yield Ok(Bytes::from(terminal));
                    break;
                }

                for bytes in encoded_events {
                    yield Ok(bytes);
                }

                sleep(Duration::from_millis(80)).await;
            }
        };

        let response = HttpResponse::builder()
            .status(status)
            .header(CONTENT_TYPE, start.content_type)
            .body(reqwest::Body::wrap_stream(stream))
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        Ok(reqwest::Response::from(response))
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
            .or_else(|| {
                probe.as_ref().map(|probe| match (
                    probe.code.trim().is_empty(),
                    probe.message.trim().is_empty(),
                ) {
                    (false, false) => format!("{}: {}", probe.code, probe.message),
                    (false, true) => probe.code.clone(),
                    (true, false) => probe.message.clone(),
                    (true, true) => String::new(),
                })
            })
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
        let configured_probe_timeout = binding.probe_timeout_ms.unwrap_or(8_000);
        let runtime_target_timeout = Duration::from_millis(if self.spec.builtin_script.is_some() {
            configured_probe_timeout.max(10_000)
        } else {
            configured_probe_timeout
        });
        let target = match self
            .select_runtime_ready_target(
                profile_dir,
                self.target_url_prefix(binding),
                runtime_target_timeout,
            )
            .await
        {
            Ok(target) => target,
            Err(BrowserProviderError::AdapterIncompatible { code, message, .. }) => {
                return incompatible_diagnostics(
                    account_id,
                    self.kind(),
                    self.adapter_id(),
                    binding,
                    &code,
                    &message,
                );
            }
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
        let context = self.context(binding, None);
        match self
            .evaluate_contract(&target, &script, "probe", None, &context, account_id)
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
        let mut normalized_body = request.body.clone();
        let object = normalized_body.as_object_mut().ok_or_else(|| {
            BrowserProviderError::InvalidConfig("chat request body must be a JSON object".into())
        })?;
        object.insert("model".into(), Value::String(request.route.model.clone()));

        let mut context = self.context(&request.binding, Some(&request.route.model));
        let thread_affinity = request
            .thread_id
            .as_deref()
            .filter(|_| self.supports_native_conversation_affinity());
        let mut unsynced_messages = Vec::new();
        let existing_conversation = if let Some(thread_id) = thread_affinity {
            match conversation_runtime::get() {
                Some(store) => {
                    let conversation = store
                        .provider_conversation(thread_id, &request.provider.id, &request.account.id)
                        .await
                        .map_err(|error| {
                            BrowserProviderError::Transport(format!(
                                "provider conversation lookup failed: {error}"
                            ))
                        })?;
                    if let Some(conversation) = conversation.as_ref() {
                        let detail = store.thread(thread_id).await.map_err(|error| {
                            BrowserProviderError::Transport(format!(
                                "provider conversation history lookup failed: {error}"
                            ))
                        })?;
                        unsynced_messages = detail
                            .messages
                            .into_iter()
                            .filter(|message| message.ordinal > conversation.last_synced_ordinal)
                            .map(|message| message.message)
                            .collect();
                    }
                    conversation
                }
                None => None,
            }
        } else {
            None
        };

        if existing_conversation.is_some() {
            normalized_body = incremental_browser_body(&normalized_body, &unsynced_messages);
        }

        let persistent_url = existing_conversation
            .as_ref()
            .map(|conversation| conversation.conversation_url.as_str());
        let ephemeral = thread_affinity.is_none() && self.use_ephemeral_chat(&request.binding);
        let start_new_conversation = persistent_url.is_none() && (thread_affinity.is_some() || ephemeral);
        if let Some(context) = context.as_object_mut() {
            context.insert(
                "start_new_conversation".into(),
                Value::Bool(start_new_conversation),
            );
            context.insert(
                "reuse_native_conversation".into(),
                Value::Bool(persistent_url.is_some()),
            );
        }
        let target = if let Some(url) = persistent_url {
            let new_chat_url = self.spec.new_chat_url.ok_or_else(|| {
                BrowserProviderError::InvalidConfig(
                    "native browser conversation affinity requires a new-chat URL".into(),
                )
            })?;
            if !is_native_conversation_url(new_chat_url, url) {
                return Err(BrowserProviderError::InvalidConfig(format!(
                    "stored provider conversation URL is not a recognized {} conversation: {}",
                    self.spec.provider,
                    diagnostic_target_location(url)
                )));
            }

            if let Some(target) = self
                .find_open_conversation_target(&request.profile_dir, url)
                .await?
            {
                target
            } else {
                let opened = self.open_target_url(&request.profile_dir, url).await?;
                let navigation_timeout = Duration::from_millis(
                    request.binding.probe_timeout_ms.unwrap_or(8_000).max(15_000),
                );
                match self
                    .wait_for_target_navigation(
                        &request.profile_dir,
                        opened.clone(),
                        self.target_url_prefix(&request.binding),
                        navigation_timeout,
                    )
                    .await
                {
                    Ok(target) => target,
                    Err(BrowserProviderError::AdapterIncompatible { code, message, .. }) => {
                        self.close_target(&request.profile_dir, &opened.id).await;
                        return Err(BrowserProviderError::AdapterIncompatible {
                            account_id: request.account.id.clone(),
                            code,
                            message,
                        });
                    }
                    Err(error) => {
                        self.close_target(&request.profile_dir, &opened.id).await;
                        return Err(error);
                    }
                }
            }
        } else if thread_affinity.is_some() || ephemeral {
            let opened = self.open_ephemeral_target(&request.profile_dir).await?;
            let navigation_timeout = Duration::from_millis(
                request.binding.probe_timeout_ms.unwrap_or(8_000).max(15_000),
            );
            match self
                .wait_for_target_navigation(
                    &request.profile_dir,
                    opened.clone(),
                    self.target_url_prefix(&request.binding),
                    navigation_timeout,
                )
                .await
            {
                Ok(target) => target,
                Err(BrowserProviderError::AdapterIncompatible { code, message, .. }) => {
                    self.close_target(&request.profile_dir, &opened.id).await;
                    return Err(BrowserProviderError::AdapterIncompatible {
                        account_id: request.account.id.clone(),
                        code,
                        message,
                    });
                }
                Err(error) => {
                    self.close_target(&request.profile_dir, &opened.id).await;
                    return Err(error);
                }
            }
        } else {
            let runtime_target_timeout = Duration::from_millis(
                request.binding.probe_timeout_ms.unwrap_or(8_000).max(10_000),
            );
            match self
                .select_runtime_ready_target(
                    &request.profile_dir,
                    self.target_url_prefix(&request.binding),
                    runtime_target_timeout,
                )
                .await
            {
                Ok(target) => target,
                Err(BrowserProviderError::AdapterIncompatible { code, message, .. }) => {
                    return Err(BrowserProviderError::AdapterIncompatible {
                        account_id: request.account.id.clone(),
                        code,
                        message,
                    });
                }
                Err(error) => return Err(error),
            }
        };

        if normalized_body
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && self.spec.builtin_script.is_some()
        {
            return self
                .execute_streaming_chat(
                    &request,
                    target,
                    script,
                    context,
                    normalized_body,
                    ephemeral,
                )
                .await;
        }

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
        if result.is_ok() && thread_affinity.is_some() {
            let _ = self
                .persist_native_conversation(&request, &target, Duration::from_secs(5))
                .await;
        }
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



fn incremental_browser_body(body: &Value, unsynced_messages: &[Value]) -> Value {
    let mut incremental = body.clone();
    let Some(object) = incremental.as_object_mut() else {
        return incremental;
    };
    let Some(messages) = object.get("messages").and_then(Value::as_array) else {
        return incremental;
    };
    if messages.is_empty() {
        return incremental;
    }
    let start = messages
        .iter()
        .rposition(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .unwrap_or(messages.len() - 1);
    let mut delta = unsynced_messages.to_vec();
    delta.extend_from_slice(&messages[start..]);
    object.insert("messages".into(), Value::Array(delta));
    incremental
}

fn target_url_matches_prefix(candidate: &str, prefix: &str) -> bool {
    if candidate.starts_with(prefix) {
        return true;
    }
    let (Ok(candidate), Ok(prefix)) = (Url::parse(candidate), Url::parse(prefix)) else {
        return false;
    };
    if candidate.scheme() != prefix.scheme() || candidate.host_str() != prefix.host_str() {
        return false;
    }
    if prefix.host_str() == Some("gemini.google.com") {
        let segments = candidate
            .path_segments()
            .map(|segments| segments.filter(|segment| !segment.is_empty()).collect::<Vec<_>>())
            .unwrap_or_default();
        return segments.iter().any(|segment| *segment == "app" || *segment == "gem");
    }
    false
}

fn is_native_conversation_url(new_chat_url: &str, candidate: &str) -> bool {
    let Ok(base) = Url::parse(new_chat_url) else {
        return false;
    };
    let Ok(url) = Url::parse(candidate) else {
        return false;
    };
    if base.scheme() != url.scheme() || base.host_str() != url.host_str() {
        return false;
    }

    let segments = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect::<Vec<_>>())
        .unwrap_or_default();

    if base.host_str() == Some("chatgpt.com") {
        return segments
            .iter()
            .position(|segment| *segment == "c")
            .and_then(|index| segments.get(index + 1))
            .is_some_and(|value| !value.is_empty());
    }

    let app_index = segments.iter().position(|segment| *segment == "app");
    if let Some(index) = app_index {
        return segments.get(index + 1).is_some_and(|value| !value.is_empty());
    }

    if let Some(index) = segments.iter().position(|segment| *segment == "gem") {
        return segments.get(index + 2).is_some_and(|value| !value.is_empty());
    }

    false
}

fn native_conversation_id(raw: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    let host = url.host_str().map(str::to_string);
    let segments = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if host.as_deref() == Some("chatgpt.com") {
        return segments
            .iter()
            .position(|segment| *segment == "c")
            .and_then(|index| segments.get(index + 1))
            .map(|value| (*value).to_string());
    }
    if let Some(index) = segments.iter().position(|segment| *segment == "app") {
        return segments.get(index + 1).map(|value| (*value).to_string());
    }
    if let Some(index) = segments.iter().position(|segment| *segment == "gem") {
        return segments.get(index + 2).map(|value| (*value).to_string());
    }
    None
}

fn same_conversation_url(left: &str, right: &str) -> bool {
    let (Ok(left_url), Ok(right_url)) = (Url::parse(left), Url::parse(right)) else {
        return left == right;
    };
    if left_url.scheme() != right_url.scheme() || left_url.host_str() != right_url.host_str() {
        return false;
    }
    match (native_conversation_id(left), native_conversation_id(right)) {
        (Some(left_id), Some(right_id)) => left_id == right_id,
        _ => left_url.path().trim_end_matches('/') == right_url.path().trim_end_matches('/'),
    }
}

fn diagnostic_target_location(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "<empty>".into();
    }
    let without_query = trimmed
        .split_once('?')
        .map(|(head, _)| head)
        .unwrap_or(trimmed);
    without_query
        .split_once('#')
        .map(|(head, _)| head)
        .unwrap_or(without_query)
        .to_string()
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
            "browser-chatgpt" => Some("https://chatgpt.com/".into()),
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
if (__operation === "chat_stream_start") {
  if (typeof __adapter.streamStart !== "function") {
    return {
      meta: __meta,
      probe: __probe,
      error: { code: "stream_unsupported", message: "adapter contract is missing streamStart(request, context)" }
    };
  }
  try {
    const __stream = await __adapter.streamStart(__request, __context);
    return { meta: __meta, probe: __probe, stream: __stream };
  } catch (error) {
    return {
      meta: __meta,
      probe: __probe,
      error: {
        code: "stream_start_error",
        message: String(error?.message || error || "browser stream failed to start")
      }
    };
  }
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

fn build_stream_control_expression(
    script: &str,
    operation: &str,
    request: &Value,
) -> Result<String, BrowserProviderError> {
    let operation_json = serde_json::to_string(operation)
        .map_err(|error| BrowserProviderError::InvalidConfig(error.to_string()))?;
    let request_json = serde_json::to_string(request)
        .map_err(|error| BrowserProviderError::InvalidConfig(error.to_string()))?;
    Ok(format!(
        r#"(async () => {{
{script}
const __adapter = globalThis.__LLMGATEWAY_ADAPTER__;
const __operation = {operation_json};
const __request = {request_json};
const __meta = __adapter?.meta || null;
if (!__adapter || typeof __adapter !== "object" || !__meta) {{
  return {{ error: {{ code: "contract_missing", message: "browser adapter is not initialized" }} }};
}}
if (__operation === "chat_stream_poll") {{
  if (typeof __adapter.streamPoll !== "function") {{
    return {{ meta: __meta, error: {{ code: "stream_unsupported", message: "adapter contract is missing streamPoll(request)" }} }};
  }}
  try {{
    return {{ meta: __meta, stream: await __adapter.streamPoll(__request) }};
  }} catch (error) {{
    return {{ meta: __meta, error: {{ code: "stream_poll_error", message: String(error?.message || error || "browser stream poll failed") }} }};
  }}
}}
if (__operation === "chat_stream_cancel") {{
  if (typeof __adapter.streamCancel !== "function") {{
    return {{ meta: __meta, stream: {{ cancelled: false, unsupported: true }} }};
  }}
  try {{
    return {{ meta: __meta, stream: await __adapter.streamCancel(__request) }};
  }} catch (error) {{
    return {{ meta: __meta, error: {{ code: "stream_cancel_error", message: String(error?.message || error || "browser stream cancel failed") }} }};
  }}
}}
return {{ meta: __meta, error: {{ code: "invalid_stream_operation", message: "unknown browser stream operation" }} }};
}})()"#
    ))
}

fn browser_stream_poll_advanced(
    poll: &CdpStreamPoll,
    last_progress_seq: &mut Option<u64>,
) -> bool {
    let Some(progress_seq) = poll.progress_seq else {
        return false;
    };
    if last_progress_seq.is_some_and(|previous| previous == progress_seq) {
        return false;
    }
    *last_progress_seq = Some(progress_seq);
    true
}

fn browser_stream_event_has_assistant_output(event: &Value) -> bool {
    event
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                let Some(delta) = choice.get("delta") else {
                    return false;
                };
                delta
                    .get("content")
                    .is_some_and(browser_stream_content_has_output)
                    || delta
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .is_some_and(|calls| !calls.is_empty())
            })
        })
}

fn browser_stream_content_has_output(content: &Value) -> bool {
    match content {
        Value::String(text) => !text.is_empty(),
        Value::Array(parts) => parts.iter().any(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty())
        }),
        _ => false,
    }
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


fn direct_error_allows_browser_fallback(
    error: &BrowserProviderError,
    dynamic_model: bool,
    browser_adapter_is_cdp: bool,
) -> bool {
    !dynamic_model
        && browser_adapter_is_cdp
        && matches!(
            error,
            BrowserProviderError::AdapterIncompatible { .. }
                | BrowserProviderError::ModelUnavailable { .. }
        )
}

fn cdp_session_status_probeable(status: &str) -> bool {
    matches!(
        status,
        "ready" | "degraded" | "requires_attention" | "login_required"
    )
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

fn default_sse_content_type() -> String {
    "text/event-stream".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Json, Router};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use uuid::Uuid;

    #[test]
    fn stale_model_recipe_never_allows_browser_fallback() {
        let stale = BrowserProviderError::ModelRecipeStale {
            account_id: "account-a".into(),
            model: "gemini-web-pro".into(),
        };
        assert!(!direct_error_allows_browser_fallback(&stale, false, true));
    }

    #[test]
    fn only_pre_submit_compatible_errors_allow_browser_fallback() {
        let incompatible = BrowserProviderError::AdapterIncompatible {
            account_id: "account-a".into(),
            code: "challenge".into(),
            message: "browser challenge".into(),
        };
        let unavailable = BrowserProviderError::ModelUnavailable {
            account_id: "account-a".into(),
            model: "configured-model".into(),
        };
        let transport = BrowserProviderError::Transport("post-submit stream failed".into());

        assert!(direct_error_allows_browser_fallback(&incompatible, false, true));
        assert!(direct_error_allows_browser_fallback(&unavailable, false, true));
        assert!(!direct_error_allows_browser_fallback(&transport, false, true));
        assert!(!direct_error_allows_browser_fallback(&incompatible, true, true));
        assert!(!direct_error_allows_browser_fallback(&incompatible, false, false));
    }

    #[test]
    fn cdp_session_recovery_states_are_probeable() {
        for status in ["ready", "degraded", "requires_attention", "login_required"] {
            assert!(cdp_session_status_probeable(status), "{status}");
        }
        for status in ["starting", "stopped", "failed"] {
            assert!(!cdp_session_status_probeable(status), "{status}");
        }
    }

    #[test]
    fn browser_kind_is_explicit() {
        assert!(BrowserProviderRegistry::is_browser_kind("browser-http"));
        assert!(BrowserProviderRegistry::is_browser_kind("browser-cdp"));
        assert!(BrowserProviderRegistry::is_browser_kind("browser-gemini"));
        assert!(BrowserProviderRegistry::is_browser_kind("browser-chatgpt"));
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
    fn browser_binding_model_allowlist_is_enforced() {
        let mut binding = test_binding();
        binding.models = vec!["model-a".into(), "model-b".into()];
        let mut config = BrowserProviderConfig::default();
        config.bindings.insert("account".into(), binding);
        let registry = BrowserProviderRegistry::new(config).unwrap();
        assert!(registry.model_allowed("account", "model-a"));
        assert!(registry.model_allowed("account", "model-b"));
        assert!(!registry.model_allowed("account", "model-c"));
        assert!(registry.model_allowed("unbound-account", "model-c"));
    }

    #[test]
    fn qwen_direct_http_requires_http_preferred_mode() {
        let registry = BrowserProviderRegistry::new(BrowserProviderConfig::default()).unwrap();
        let mut binding = test_binding();
        binding.transport_mode = BrowserTransportMode::Auto;
        assert!(registry.direct_adapter("browser-qwen", &binding).is_none());

        binding.transport_mode = BrowserTransportMode::HttpPreferred;
        let direct = registry
            .direct_adapter("browser-qwen", &binding)
            .expect("Qwen direct adapter must be registered");
        assert_eq!(direct.adapter_id(), "qwen-web-http");

        binding.transport_mode = BrowserTransportMode::BrowserOnly;
        assert!(registry.direct_adapter("browser-qwen", &binding).is_none());
    }

    #[test]
    fn discovered_models_extend_the_binding_allowlist() {
        let mut binding = test_binding();
        binding.models = vec!["gemini-web-default".into()];
        let mut config = BrowserProviderConfig::default();
        config.bindings.insert("account".into(), binding);
        let registry = BrowserProviderRegistry::new(config).unwrap();

        assert!(!registry.model_allowed("account", "gemini-web-pro"));
        registry.remember_discovered_models(
            "account",
            &[BrowserDiscoveredModel {
                external_id: "gemini-web-pro".into(),
                display_name: "Gemini Pro".into(),
                owned_by: "Google".into(),
                context_window: None,
                capabilities: vec!["chat".into(), "reasoning".into()],
            }],
        );
        assert!(registry.model_allowed("account", "gemini-web-pro"));
        assert!(!registry.model_allowed("account", "gemini-web-flash"));
    }

    #[test]
    fn built_in_provider_defaults_are_first_class() {
        let gemini = CdpBrowserAdapter::gemini().unwrap();
        let chatgpt = CdpBrowserAdapter::chatgpt().unwrap();
        let qwen = CdpBrowserAdapter::qwen().unwrap();
        assert_eq!(gemini.kind(), "browser-gemini");
        assert_eq!(gemini.adapter_id(), "gemini-web");
        assert!(gemini.spec.ephemeral_default);
        assert_eq!(
            gemini.spec.default_target_url_prefix,
            Some("https://gemini.google.com/app")
        );
        assert_eq!(chatgpt.kind(), "browser-chatgpt");
        assert_eq!(chatgpt.adapter_id(), "chatgpt-web");
        assert!(chatgpt.spec.ephemeral_default);
        assert_eq!(
            chatgpt.spec.default_target_url_prefix,
            Some("https://chatgpt.com/")
        );
        assert_eq!(chatgpt.spec.new_chat_url, Some("https://chatgpt.com/"));
        assert_eq!(qwen.kind(), "browser-qwen");
        assert_eq!(qwen.adapter_id(), "qwen-web");
        assert!(qwen.spec.ephemeral_default);
        assert_eq!(qwen.spec.new_chat_url, Some("https://chat.qwen.ai/c/new-chat"));
    }

    #[test]
    fn gemini_adapter_can_force_a_fresh_conversation() {
        assert!(GEMINI_WEB_ADAPTER.contains("start_new_conversation"));
        assert!(GEMINI_WEB_ADAPTER.contains("Gemini New chat control was not found"));
        assert!(GEMINI_WEB_ADAPTER.contains("responseLeaves"));
        assert!(GEMINI_WEB_ADAPTER.contains("document.querySelectorAll(selector)"));
        assert!(GEMINI_WEB_ADAPTER.contains("Gemini conversation history did not stabilize"));
        assert!(GEMINI_WEB_ADAPTER.contains("newResponseText(before, responses)"));
        assert!(GEMINI_WEB_ADAPTER.contains("div.markdown.markdown-main-panel"));
        assert!(GEMINI_WEB_ADAPTER.contains("model-response message-content"));
        assert!(GEMINI_WEB_ADAPTER.contains("reuse_native_conversation"));
    }

    #[test]
    fn chatgpt_adapter_can_force_a_fresh_conversation() {
        assert!(CHATGPT_WEB_ADAPTER.contains("start_new_conversation"));
        assert!(CHATGPT_WEB_ADAPTER.contains("ChatGPT New chat control was not found"));
        assert!(CHATGPT_WEB_ADAPTER.contains("responseLeaves"));
        assert!(CHATGPT_WEB_ADAPTER.contains("ChatGPT conversation history did not stabilize"));
        assert!(CHATGPT_WEB_ADAPTER.contains("newResponseText(before, responses)"));
        assert!(CHATGPT_WEB_ADAPTER.contains("#prompt-textarea"));
        assert!(CHATGPT_WEB_ADAPTER.contains("reuse_native_conversation"));
    }

    #[test]
    fn gemini_account_scoped_app_urls_match_provider_prefix() {
        assert!(target_url_matches_prefix(
            "https://gemini.google.com/u/1/app/abc123",
            "https://gemini.google.com/app"
        ));
        assert!(target_url_matches_prefix(
            "https://gemini.google.com/gem/gem123/chat456",
            "https://gemini.google.com/app"
        ));
        assert!(!target_url_matches_prefix(
            "https://accounts.google.com/ServiceLogin",
            "https://gemini.google.com/app"
        ));
    }

    #[test]
    fn gemini_native_conversation_url_is_distinct_from_new_chat() {
        assert!(!is_native_conversation_url(
            "https://gemini.google.com/app",
            "https://gemini.google.com/app"
        ));
        assert!(is_native_conversation_url(
            "https://gemini.google.com/app",
            "https://gemini.google.com/app/abc123"
        ));
        assert!(is_native_conversation_url(
            "https://gemini.google.com/app",
            "https://gemini.google.com/u/1/app/abc123"
        ));
        assert!(is_native_conversation_url(
            "https://gemini.google.com/app",
            "https://gemini.google.com/gem/gem123/chat456"
        ));
        assert!(!is_native_conversation_url(
            "https://gemini.google.com/app",
            "https://example.test/app/abc123"
        ));
    }

    #[test]
    fn chatgpt_native_conversation_url_is_distinct_from_new_chat() {
        assert!(!is_native_conversation_url(
            "https://chatgpt.com/",
            "https://chatgpt.com/"
        ));
        assert!(is_native_conversation_url(
            "https://chatgpt.com/",
            "https://chatgpt.com/c/abc123"
        ));
        assert!(is_native_conversation_url(
            "https://chatgpt.com/",
            "https://chatgpt.com/g/gpt-id/c/abc123"
        ));
        assert!(!is_native_conversation_url(
            "https://chatgpt.com/",
            "https://chatgpt.com/g/gpt-id"
        ));
        assert!(!is_native_conversation_url(
            "https://chatgpt.com/",
            "https://example.test/c/abc123"
        ));
    }

    #[test]
    fn chatgpt_conversation_url_comparison_uses_c_identity() {
        assert!(same_conversation_url(
            "https://chatgpt.com/c/abc123?foo=bar",
            "https://chatgpt.com/g/gpt-id/c/abc123#answer"
        ));
        assert!(!same_conversation_url(
            "https://chatgpt.com/c/abc123",
            "https://chatgpt.com/c/other"
        ));
    }

    #[test]
    fn conversation_url_comparison_uses_native_chat_identity() {
        assert!(same_conversation_url(
            "https://gemini.google.com/u/1/app/abc123?foo=bar",
            "https://gemini.google.com/app/abc123#answer"
        ));
        assert!(same_conversation_url(
            "https://gemini.google.com/gem/gem123/abc123",
            "https://gemini.google.com/u/1/app/abc123"
        ));
        assert!(!same_conversation_url(
            "https://gemini.google.com/u/1/app/abc123",
            "https://gemini.google.com/u/1/app/other"
        ));
    }

    #[test]
    fn incremental_browser_body_keeps_missed_turns_and_current_user() {
        let body = json!({
            "model": "gemini-web-default",
            "messages": [
                {"role":"system","content":"system"},
                {"role":"user","content":"old user"},
                {"role":"assistant","content":"old assistant"},
                {"role":"user","content":"current user"}
            ],
            "stream": false
        });
        let missed = vec![
            json!({"role":"user","content":"missed user"}),
            json!({"role":"assistant","content":"missed assistant"})
        ];
        let delta = incremental_browser_body(&body, &missed);
        assert_eq!(
            delta["messages"],
            json!([
                {"role":"user","content":"missed user"},
                {"role":"assistant","content":"missed assistant"},
                {"role":"user","content":"current user"}
            ])
        );
    }

    #[test]
    fn browser_stream_progress_heartbeat_advances_without_output_events() {
        let mut last = None;
        let first = CdpStreamPoll {
            events: vec![],
            done: false,
            error: None,
            progress_seq: Some(1),
        };
        assert!(browser_stream_poll_advanced(&first, &mut last));
        assert_eq!(last, Some(1));
        assert!(!browser_stream_poll_advanced(&first, &mut last));

        let generating = CdpStreamPoll {
            progress_seq: Some(2),
            ..first
        };
        assert!(browser_stream_poll_advanced(&generating, &mut last));
        assert_eq!(last, Some(2));

        let legacy = CdpStreamPoll {
            events: vec![],
            done: false,
            error: None,
            progress_seq: None,
        };
        assert!(!browser_stream_poll_advanced(&legacy, &mut last));
    }

    #[test]
    fn stream_output_detection_ignores_metadata_only_events() {
        assert!(!browser_stream_event_has_assistant_output(&json!({
            "choices": [{"delta": {"role": "assistant"}}]
        })));
        assert!(!browser_stream_event_has_assistant_output(&json!({
            "choices": [{"delta": {"content": ""}}]
        })));
        assert!(browser_stream_event_has_assistant_output(&json!({
            "choices": [{"delta": {"content": "answer"}}]
        })));
        assert!(browser_stream_event_has_assistant_output(&json!({
            "choices": [{"delta": {"tool_calls": [{"id": "call_1"}]}}]
        })));
    }

    #[test]
    fn stream_control_expression_reinjects_adapter_contract() {
        let expression = build_stream_control_expression(
            "globalThis.__LLMGATEWAY_ADAPTER__ = {meta:{contract_version:1,id:'x',provider:'x'},streamPoll:async()=>({events:[],done:false})};",
            "chat_stream_poll",
            &json!({"stream_id":"stream-1"}),
        )
        .unwrap();
        assert!(expression.contains("globalThis.__LLMGATEWAY_ADAPTER__ ="));
        assert!(expression.contains("streamPoll"));
        let injected = expression.find("globalThis.__LLMGATEWAY_ADAPTER__ =").unwrap();
        let read = expression.find("const __adapter = globalThis.__LLMGATEWAY_ADAPTER__;").unwrap();
        assert!(injected < read);
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

    #[tokio::test]
    async fn runtime_ready_target_skips_stale_matching_page() {
        async fn serve_runtime_host(listener: TcpListener, host: &'static str) {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let message = socket.next().await.unwrap().unwrap();
            let command: Value =
                serde_json::from_str(message.into_text().unwrap().as_ref()).unwrap();
            assert_eq!(command["method"], "Runtime.evaluate");
            assert_eq!(
                command["params"]["expression"].as_str(),
                Some("String(globalThis.location?.hostname || '')")
            );
            let id = command["id"].as_u64().unwrap();
            let response = json!({
                "id": id,
                "result": {
                    "result": {
                        "type": "string",
                        "value": host
                    }
                }
            });
            socket
                .send(Message::Text(response.to_string().into()))
                .await
                .unwrap();
        }

        let stale_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stale_address = stale_listener.local_addr().unwrap();
        let stale_task = tokio::spawn(serve_runtime_host(stale_listener, ""));

        let ready_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ready_address = ready_listener.local_addr().unwrap();
        let ready_task = tokio::spawn(serve_runtime_host(
            ready_listener,
            "gemini.google.com",
        ));

        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_address = http_listener.local_addr().unwrap();
        let stale_ws = format!("ws://{stale_address}/devtools/page/stale");
        let ready_ws = format!("ws://{ready_address}/devtools/page/ready");
        let app = Router::new().route(
            "/json/list",
            get(move || {
                let stale_ws = stale_ws.clone();
                let ready_ws = ready_ws.clone();
                async move {
                    Json(json!([
                        {
                            "id": "stale-target",
                            "type": "page",
                            "url": "https://gemini.google.com/app",
                            "webSocketDebuggerUrl": stale_ws
                        },
                        {
                            "id": "ready-target",
                            "type": "page",
                            "url": "https://gemini.google.com/app",
                            "webSocketDebuggerUrl": ready_ws
                        }
                    ]))
                }
            }),
        );
        let http_task = tokio::spawn(async move {
            axum::serve(http_listener, app).await.unwrap();
        });

        let profile_dir = std::env::temp_dir().join(format!(
            "llmgateway-runtime-ready-target-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&profile_dir).unwrap();
        fs::write(
            profile_dir.join("DevToolsActivePort"),
            format!("{}\n/devtools/browser/fixture\n", http_address.port()),
        )
        .unwrap();

        let adapter = CdpBrowserAdapter::gemini().unwrap();
        let selected = adapter
            .select_runtime_ready_target(
                profile_dir.to_str().unwrap(),
                Some("https://gemini.google.com/app"),
                Duration::from_secs(2),
            )
            .await
            .unwrap();

        assert_eq!(selected.id, "ready-target");

        stale_task.await.unwrap();
        ready_task.await.unwrap();
        http_task.abort();
        let _ = fs::remove_dir_all(profile_dir);
    }

    fn test_binding() -> BrowserAccountBinding {
        BrowserAccountBinding {
            session: "fixture-session".into(),
            transport_mode: BrowserTransportMode::BrowserOnly,
            target_url_prefix: None,
            adapter_script: None,
            adapter_contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
            models: vec!["gemini-test".into()],
            model_labels: BTreeMap::new(),
            selector_overrides: BTreeMap::new(),
            ephemeral_chat: Some(false),
            probe_timeout_ms: Some(500),
            response_timeout_ms: Some(1_000),
            first_byte_timeout_ms: Some(1_000),
            idle_stream_timeout_ms: Some(1_000),
        }
    }

    async fn fake_cdp_diagnostics(envelope: Value) -> BrowserAdapterDiagnostics {
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_address = ws_listener.local_addr().unwrap();
        let websocket_url = format!("ws://{ws_address}/devtools/page/fixture");
        let ws_task = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = ws_listener.accept().await.unwrap();
                let mut socket = accept_async(stream).await.unwrap();
                let message = socket.next().await.unwrap().unwrap();
                let command: Value =
                    serde_json::from_str(message.into_text().unwrap().as_ref()).unwrap();
                assert_eq!(command["method"], "Runtime.evaluate");
                let expression = command["params"]["expression"].as_str().unwrap();
                let value = if expression == "String(globalThis.location?.hostname || '')" {
                    json!("gemini.google.com")
                } else {
                    assert!(expression.contains("gemini-web"));
                    envelope.clone()
                };
                let id = command["id"].as_u64().unwrap();
                let response = json!({
                    "id": id,
                    "result": {
                        "result": {
                            "type": "object",
                            "value": value
                        }
                    }
                });
                socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
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
    fn diagnostic_target_location_redacts_query_and_fragment() {
        assert_eq!(
            diagnostic_target_location("https://gemini.google.com/app?token=secret#fragment"),
            "https://gemini.google.com/app"
        );
        assert_eq!(diagnostic_target_location("about:blank"), "about:blank");
        assert_eq!(diagnostic_target_location(""), "<empty>");
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


#[cfg(test)]
mod browser_transport_policy_tests {
    use super::*;

    #[test]
    fn built_in_direct_adapters_publish_provider_neutral_capabilities() {
        let registry = BrowserProviderRegistry::new(BrowserProviderConfig::default()).unwrap();

        let gemini = registry.transport_capabilities("browser-gemini");
        assert!(gemini.supported);
        assert_eq!(gemini.recommended_mode, Some(BrowserTransportMode::Auto));
        assert!(gemini.modes.contains(&BrowserTransportMode::HttpPreferred));
        assert!(gemini.supports_direct_model_discovery);
        assert!(gemini.supports_native_conversation);

        let chatgpt = registry.transport_capabilities("browser-chatgpt");
        assert!(chatgpt.supported);
        assert_eq!(chatgpt.recommended_mode, Some(BrowserTransportMode::Auto));
        assert!(!chatgpt.supports_direct_model_discovery);
        assert!(chatgpt.supports_native_conversation);

        let qwen = registry.transport_capabilities("browser-qwen");
        assert!(qwen.supported);
        assert_eq!(
            qwen.recommended_mode,
            Some(BrowserTransportMode::HttpPreferred)
        );
        assert!(!qwen.modes.contains(&BrowserTransportMode::Auto));
        assert!(qwen.supports_direct_model_discovery);
        assert!(qwen.supports_native_conversation);

        let unsupported = registry.transport_capabilities("browser-custom");
        assert!(!unsupported.supported);
        assert_eq!(unsupported.recommended_mode, None);
    }

    #[test]
    fn logical_policy_resolves_without_provider_specific_ui_knowledge() {
        let registry = BrowserProviderRegistry::new(BrowserProviderConfig::default()).unwrap();
        assert_eq!(
            registry
                .resolve_transport_policy(
                    "browser-gemini",
                    BrowserTransportPolicy::BrowserlessPreferred,
                )
                .unwrap(),
            BrowserTransportMode::Auto
        );
        assert_eq!(
            registry
                .resolve_transport_policy(
                    "browser-qwen",
                    BrowserTransportPolicy::BrowserlessPreferred,
                )
                .unwrap(),
            BrowserTransportMode::HttpPreferred
        );
        assert_eq!(
            registry
                .resolve_transport_policy(
                    "browser-custom",
                    BrowserTransportPolicy::BrowserOnly,
                )
                .unwrap(),
            BrowserTransportMode::BrowserOnly
        );
        assert!(matches!(
            registry.resolve_transport_policy(
                "browser-custom",
                BrowserTransportPolicy::BrowserlessPreferred,
            ),
            Err(BrowserProviderError::UnsupportedBrowserless(_))
        ));
    }

    #[test]
    fn request_config_snapshot_stays_immutable_across_policy_reload() {
        let mut initial = BrowserProviderConfig::default();
        initial.bindings.insert(
            "account-a".into(),
            BrowserAccountBinding {
                session: "session-a".into(),
                transport_mode: BrowserTransportMode::BrowserOnly,
                target_url_prefix: None,
                adapter_script: None,
                adapter_contract_version: None,
                models: Vec::new(),
                model_labels: BTreeMap::new(),
                selector_overrides: BTreeMap::new(),
                ephemeral_chat: None,
                probe_timeout_ms: None,
                response_timeout_ms: None,
                first_byte_timeout_ms: None,
                idle_stream_timeout_ms: None,
            },
        );
        let registry = BrowserProviderRegistry::new(initial).unwrap();
        let request_snapshot = registry.config_snapshot();

        let mut updated = request_snapshot.clone();
        updated.bindings.get_mut("account-a").unwrap().transport_mode =
            BrowserTransportMode::HttpPreferred;
        registry.reload(updated).unwrap();

        assert_eq!(
            request_snapshot.bindings["account-a"].transport_mode,
            BrowserTransportMode::BrowserOnly
        );
        assert_eq!(
            registry.config_snapshot().bindings["account-a"].transport_mode,
            BrowserTransportMode::HttpPreferred
        );
    }

    #[test]
    fn transport_policy_parser_is_semantic_and_strict() {
        assert_eq!(
            BrowserTransportPolicy::parse("browser-only").unwrap(),
            BrowserTransportPolicy::BrowserOnly
        );
        assert_eq!(
            BrowserTransportPolicy::parse("browserless-preferred").unwrap(),
            BrowserTransportPolicy::BrowserlessPreferred
        );
        assert!(matches!(
            BrowserTransportPolicy::parse("http-preferred"),
            Err(BrowserProviderError::InvalidTransportPolicy(_))
        ));
    }
}
