use crate::{
    catalog::ModelCatalog,
    config::{AccountConfig, AppConfig, ProviderConfig, RouteConfig},
    routing::Router,
};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE},
    Client, StatusCode,
};
use serde_json::Value;
use std::{env, sync::Arc, time::Duration};
use thiserror::Error;

#[derive(Clone)]
pub struct Gateway {
    pub config: Arc<AppConfig>,
    pub router: Router,
    client: Client,
}

pub struct RoutedResponse {
    pub response: reqwest::Response,
    pub route: RouteConfig,
}

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("unknown or unavailable model '{0}'")]
    NoRoute(String),
    #[error("missing credential environment variable '{0}'")]
    MissingCredential(String),
    #[error("invalid upstream configuration: {0}")]
    InvalidConfig(String),
    #[error("upstream request failed: {0}")]
    Transport(String),
    #[error("upstream rejected request with {status}: {body}")]
    Upstream { status: StatusCode, body: String },
}

impl Gateway {
    pub fn new(config: Arc<AppConfig>, catalog: Arc<ModelCatalog>) -> Result<Self, GatewayError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|error| GatewayError::Transport(error.to_string()))?;
        let router = Router::new(config.clone(), catalog);
        Ok(Self {
            config,
            router,
            client,
        })
    }

    pub async fn execute_openai_chat(
        &self,
        requested_model: &str,
        body: &Value,
    ) -> Result<RoutedResponse, GatewayError> {
        self.execute_openai_chat_with_affinity(requested_model, body, None)
            .await
    }

    pub async fn execute_openai_chat_with_affinity(
        &self,
        requested_model: &str,
        body: &Value,
        preferred_route: Option<&str>,
    ) -> Result<RoutedResponse, GatewayError> {
        let mut routes = self.router.plan(requested_model).await;
        if let Some(preferred_route) = preferred_route {
            if let Some(index) = routes.iter().position(|route| route.id == preferred_route) {
                let preferred = routes.remove(index);
                routes.insert(0, preferred);
            }
        }
        if routes.is_empty() {
            return Err(GatewayError::NoRoute(requested_model.to_string()));
        }

        let mut last_error: Option<GatewayError> = None;

        for route in routes {
            let account = self.config.account(&route.account).ok_or_else(|| {
                GatewayError::InvalidConfig(format!("unknown account '{}'", route.account))
            })?;
            if !account.enabled {
                continue;
            }
            let provider = self.config.provider(&account.provider).ok_or_else(|| {
                GatewayError::InvalidConfig(format!("unknown provider '{}'", account.provider))
            })?;
            if provider.kind != "openai-compatible" {
                last_error = Some(GatewayError::InvalidConfig(format!(
                    "provider '{}' uses unsupported kind '{}'",
                    provider.id, provider.kind
                )));
                continue;
            }

            match self
                .send_openai_chat(provider, account, &route, body)
                .await
            {
                Ok(response) if response.status().is_success() => {
                    self.router.mark_success(&route.id).await;
                    return Ok(RoutedResponse { response, route });
                }
                Ok(response) => {
                    let status = response.status();
                    let body_text = response.text().await.unwrap_or_default();
                    let retryable = is_retryable_status(status);
                    let cooldown = cooldown_for(status);
                    self.router
                        .mark_failure(&route.id, format!("HTTP {status}: {body_text}"), cooldown)
                        .await;

                    let error = GatewayError::Upstream {
                        status,
                        body: body_text,
                    };
                    if !retryable {
                        return Err(error);
                    }
                    last_error = Some(error);
                }
                Err(error) => {
                    self.router
                        .mark_failure(&route.id, error.to_string(), 10)
                        .await;
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| GatewayError::NoRoute(requested_model.to_string())))
    }

    async fn send_openai_chat(
        &self,
        provider: &ProviderConfig,
        account: &AccountConfig,
        route: &RouteConfig,
        body: &Value,
    ) -> Result<reqwest::Response, GatewayError> {
        let key = env::var(&account.api_key_env)
            .map_err(|_| GatewayError::MissingCredential(account.api_key_env.clone()))?;
        let mut upstream_body = body.clone();
        let object = upstream_body.as_object_mut().ok_or_else(|| {
            GatewayError::InvalidConfig("chat request body must be a JSON object".into())
        })?;
        object.insert("model".into(), Value::String(route.model.clone()));

        let url = format!(
            "{}/chat/completions",
            provider.base_url.trim_end_matches('/')
        );
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        apply_auth(&mut headers, account, &key)?;

        self.client
            .post(url)
            .headers(headers)
            .json(&upstream_body)
            .send()
            .await
            .map_err(|error| GatewayError::Transport(error.to_string()))
    }
}

fn apply_auth(
    headers: &mut HeaderMap,
    account: &AccountConfig,
    key: &str,
) -> Result<(), GatewayError> {
    match account.auth_style.as_str() {
        "bearer" => {
            let value = HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|error| GatewayError::InvalidConfig(error.to_string()))?;
            headers.insert(AUTHORIZATION, value);
        }
        "x-api-key" => {
            let name = HeaderName::from_static("x-api-key");
            let value = HeaderValue::from_str(key)
                .map_err(|error| GatewayError::InvalidConfig(error.to_string()))?;
            headers.insert(name, value);
        }
        other => {
            return Err(GatewayError::InvalidConfig(format!(
                "unsupported auth_style '{other}'"
            )));
        }
    }
    Ok(())
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status.as_u16(),
        401 | 403 | 408 | 409 | 429 | 500 | 502 | 503 | 504
    )
}

fn cooldown_for(status: StatusCode) -> i64 {
    match status.as_u16() {
        401 | 403 => 300,
        429 => 60,
        500..=599 => 20,
        _ => 10,
    }
}
