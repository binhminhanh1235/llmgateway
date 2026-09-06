use reqwest::{Client, Method};
use serde_json::{json, Value};
use std::{env, time::Duration};
use thiserror::Error;

pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:7331";

#[derive(Clone, Copy, Debug)]
pub enum LocalAuth {
    None,
    Execution,
    Admin,
}

#[derive(Clone)]
pub struct LocalGatewayClient {
    client: Client,
    base_url: String,
    execution_key: Option<String>,
    admin_key: Option<String>,
}

#[derive(Debug, Error)]
pub enum LocalClientError {
    #[error("execution key missing: set LLMGATEWAY_CLIENT_API_KEY or LLMGATEWAY_API_KEY")]
    MissingExecutionKey,
    #[error("admin key missing: set LLMGATEWAY_API_KEY")]
    MissingAdminKey,
    #[error("request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("HTTP {status} {path}: {body}")]
    Http {
        status: u16,
        path: String,
        body: String,
    },
}

impl LocalGatewayClient {
    pub fn from_env() -> Result<Self, LocalClientError> {
        let base_url =
            env::var("LLMGATEWAY_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let admin_key = env::var("LLMGATEWAY_API_KEY").ok();
        let execution_key = env::var("LLMGATEWAY_CLIENT_API_KEY")
            .ok()
            .or_else(|| admin_key.clone());
        Self::new(base_url, execution_key, admin_key)
    }

    pub fn new(
        base_url: impl Into<String>,
        execution_key: Option<String>,
        admin_key: Option<String>,
    ) -> Result<Self, LocalClientError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(
                env::var("LLMGATEWAY_CLIENT_TIMEOUT_SECONDS")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|value| *value > 0)
                    .unwrap_or(60),
            ))
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            execution_key,
            admin_key,
        })
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        auth: LocalAuth,
        anthropic: bool,
    ) -> Result<Value, LocalClientError> {
        let mut request = self
            .client
            .request(method, format!("{}{}", self.base_url, path));
        if let Some(body) = body {
            request = request.json(body);
        }

        request = match auth {
            LocalAuth::None => request,
            LocalAuth::Execution => {
                let key = self
                    .execution_key
                    .as_deref()
                    .ok_or(LocalClientError::MissingExecutionKey)?;
                if anthropic {
                    request
                        .header("x-api-key", key)
                        .header("anthropic-version", "2023-06-01")
                } else {
                    request.bearer_auth(key)
                }
            }
            LocalAuth::Admin => {
                let key = self
                    .admin_key
                    .as_deref()
                    .ok_or(LocalClientError::MissingAdminKey)?;
                request.bearer_auth(key)
            }
        };

        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(LocalClientError::Http {
                status: status.as_u16(),
                path: path.to_string(),
                body: String::from_utf8_lossy(&bytes).to_string(),
            });
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => Ok(value),
            Err(_) => Ok(json!({
                "text": String::from_utf8_lossy(&bytes)
            })),
        }
    }

    pub async fn health(&self) -> Result<Value, LocalClientError> {
        self.request(
            Method::GET,
            "/_llmgateway/health",
            None,
            LocalAuth::None,
            false,
        )
        .await
    }

    pub async fn models(&self) -> Result<Value, LocalClientError> {
        self.request(Method::GET, "/v1/models", None, LocalAuth::Execution, false)
            .await
    }

    pub async fn capabilities(&self) -> Result<Value, LocalClientError> {
        self.request(
            Method::GET,
            "/_llmgateway/agent/capabilities",
            None,
            LocalAuth::Execution,
            false,
        )
        .await
    }

    pub async fn resolve(
        &self,
        diagnostics: bool,
        body: &Value,
    ) -> Result<Value, LocalClientError> {
        let path = if diagnostics {
            "/_llmgateway/agent/diagnostics"
        } else {
            "/_llmgateway/agent/resolve"
        };
        self.request(Method::POST, path, Some(body), LocalAuth::Execution, false)
            .await
    }

    pub async fn chat(&self, body: &Value) -> Result<Value, LocalClientError> {
        self.request(
            Method::POST,
            "/v1/chat/completions",
            Some(body),
            LocalAuth::Execution,
            false,
        )
        .await
    }

    pub async fn responses(&self, body: &Value) -> Result<Value, LocalClientError> {
        self.request(
            Method::POST,
            "/v1/responses",
            Some(body),
            LocalAuth::Execution,
            false,
        )
        .await
    }

    pub async fn messages(&self, body: &Value) -> Result<Value, LocalClientError> {
        self.request(
            Method::POST,
            "/v1/messages",
            Some(body),
            LocalAuth::Execution,
            true,
        )
        .await
    }
}
