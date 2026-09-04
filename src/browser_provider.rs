use crate::{
    browser_session_runtime, chromium_driver_runtime,
    config::{AccountConfig, ProviderConfig, RouteConfig},
};
use async_trait::async_trait;
use axum::http::Response as HttpResponse;
use futures_util::{SinkExt, StreamExt};
use reqwest::{header::CONTENT_TYPE, Client};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::Path, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const MAX_ADAPTER_SCRIPT_BYTES: u64 = 512 * 1024;
const CDP_EXECUTION_TIMEOUT_SECONDS: u64 = 600;

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
        }
        Ok(envelope.browser)
    }
}

impl BrowserProviderRegistry {
    pub fn new(config: BrowserProviderConfig) -> Result<Self, BrowserProviderError> {
        let http = Arc::new(HttpBrowserAdapter::new()?);
        let cdp = Arc::new(CdpBrowserAdapter::new()?);
        let mut adapters: BTreeMap<String, Arc<dyn BrowserProviderAdapter>> = BTreeMap::new();
        adapters.insert(http.kind().to_string(), http);
        adapters.insert(cdp.kind().to_string(), cdp);
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

    pub fn session_id_for_account(&self, account_id: &str) -> Option<&str> {
        self.config
            .bindings
            .get(account_id)
            .map(|binding| binding.session.as_str())
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

        if provider_kind == "browser-cdp" {
            let Some(driver) = chromium_driver_runtime::get() else {
                return false;
            };
            return driver
                .status(&binding.session)
                .await
                .is_ok_and(|status| {
                    status.running
                        && status.debugger_reachable
                        && status.ready_match.is_some()
                });
        }

        true
    }

    pub async fn mark_degraded(
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
        if provider.kind == "browser-cdp"
            && matches!(
                &result,
                Err(BrowserProviderError::SessionUnavailable { .. })
                    | Err(BrowserProviderError::Transport(_))
            )
        {
            let error_text = result
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_else(|| "browser CDP runtime unavailable".into());
            let _ = self.mark_degraded(&account.id, &error_text).await;
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

#[derive(Clone)]
struct CdpBrowserAdapter {
    client: Client,
}

#[derive(Debug, Deserialize)]
struct CdpTarget {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    websocket_debugger_url: String,
}

#[derive(Debug, Deserialize)]
struct CdpAdapterResult {
    #[serde(default = "default_ok_status")]
    status: u16,
    #[serde(default = "default_json_content_type")]
    content_type: String,
    body: Value,
}

impl CdpBrowserAdapter {
    fn new() -> Result<Self, BrowserProviderError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(4))
            .build()
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        Ok(Self { client })
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
}

#[async_trait]
impl BrowserProviderAdapter for CdpBrowserAdapter {
    fn kind(&self) -> &'static str {
        "browser-cdp"
    }

    async fn execute_chat(
        &self,
        request: BrowserAdapterRequest,
    ) -> Result<reqwest::Response, BrowserProviderError> {
        let script_path = request
            .binding
            .adapter_script
            .as_deref()
            .ok_or_else(|| BrowserProviderError::InvalidConfig(format!(
                "browser-cdp account '{}' requires browser.bindings.{}.adapter_script",
                request.account.id, request.account.id
            )))?;
        let script = read_adapter_script(script_path)?;
        let targets = self.targets(&request.profile_dir).await?;
        let target = self
            .select_target(&targets, request.binding.target_url_prefix.as_deref())
            .ok_or_else(|| BrowserProviderError::SessionUnavailable {
                account_id: request.account.id.clone(),
                session_id: request.session_id.clone(),
            })?;

        let mut normalized_body = request.body;
        let object = normalized_body.as_object_mut().ok_or_else(|| {
            BrowserProviderError::InvalidConfig("chat request body must be a JSON object".into())
        })?;
        object.insert("model".into(), Value::String(request.route.model));
        let request_json = serde_json::to_string(&normalized_body)
            .map_err(|error| BrowserProviderError::InvalidConfig(error.to_string()))?;
        let expression = format!(
            "(async () => {{\n{script}\nconst adapter = globalThis.__LLMGATEWAY_ADAPTER__;\nif (!adapter || typeof adapter.chat !== 'function') throw new Error('adapter script must expose globalThis.__LLMGATEWAY_ADAPTER__.chat(request)');\nreturn await adapter.chat({request_json});\n}})()"
        );
        let value = evaluate_cdp(&target.websocket_debugger_url, &expression).await?;
        let result: CdpAdapterResult = serde_json::from_value(value).map_err(|error| {
            BrowserProviderError::Transport(format!(
                "browser CDP adapter returned an invalid result envelope: {error}"
            ))
        })?;
        synthetic_response(result)
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
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    #[test]
    fn browser_kind_is_explicit() {
        assert!(BrowserProviderRegistry::is_browser_kind("browser-http"));
        assert!(BrowserProviderRegistry::is_browser_kind("browser-cdp"));
        assert!(BrowserProviderRegistry::is_browser_kind("browser-gemini"));
        assert!(!BrowserProviderRegistry::is_browser_kind("openai-compatible"));
    }

    #[test]
    fn selects_matching_page_target() {
        let adapter = CdpBrowserAdapter::new().unwrap();
        let targets = vec![
            CdpTarget {
                kind: "page".into(),
                url: "https://example.test/login".into(),
                websocket_debugger_url: "ws://127.0.0.1/one".into(),
            },
            CdpTarget {
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
