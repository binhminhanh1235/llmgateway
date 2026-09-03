use crate::{
    browser_session_runtime,
    config::{AccountConfig, ProviderConfig, RouteConfig},
};
use async_trait::async_trait;
use reqwest::{header::CONTENT_TYPE, Client};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path, sync::Arc, time::Duration};
use thiserror::Error;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct BrowserProviderConfig {
    #[serde(default)]
    pub bindings: BTreeMap<String, BrowserAccountBinding>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BrowserAccountBinding {
    pub session: String,
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

    async fn execute_chat(
        &self,
        request: BrowserAdapterRequest,
    ) -> Result<reqwest::Response, BrowserProviderError>;
}

pub struct BrowserProviderRegistry {
    config: Arc<BrowserProviderConfig>,
    adapters: BTreeMap<String, Arc<dyn BrowserProviderAdapter>>,
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
        }
        Ok(envelope.browser)
    }
}

impl BrowserProviderRegistry {
    pub fn new(config: BrowserProviderConfig) -> Result<Self, BrowserProviderError> {
        let http = Arc::new(HttpBrowserAdapter::new()?);
        let mut adapters: BTreeMap<String, Arc<dyn BrowserProviderAdapter>> = BTreeMap::new();
        adapters.insert(http.kind().to_string(), http);
        Ok(Self {
            config: Arc::new(config),
            adapters,
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

    pub async fn route_available(&self, provider_kind: &str, account_id: &str) -> bool {
        if !Self::is_browser_kind(provider_kind) || !self.supports(provider_kind) {
            return false;
        }
        let Some(binding) = self.config.bindings.get(account_id) else {
            return false;
        };
        let Some(store) = browser_session_runtime::get() else {
            return false;
        };
        match store.session(&binding.session).await {
            Ok(session) => session.enabled && session.status == "ready",
            Err(_) => false,
        }
    }

    pub async fn require_attention(
        &self,
        account_id: &str,
        error: &str,
    ) -> Result<(), BrowserProviderError> {
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

        adapter
            .execute_chat(BrowserAdapterRequest {
                provider: provider.clone(),
                account: account.clone(),
                route: route.clone(),
                body: body.clone(),
                session_id: binding.session.clone(),
            })
            .await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_kind_is_explicit() {
        assert!(BrowserProviderRegistry::is_browser_kind("browser-http"));
        assert!(BrowserProviderRegistry::is_browser_kind("browser-gemini"));
        assert!(!BrowserProviderRegistry::is_browser_kind("openai-compatible"));
    }
}
