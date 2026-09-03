use crate::{
    catalog::ModelCatalog,
    config::{AccountConfig, AppConfig, ProviderConfig, RouteConfig},
    quota_usage::{QuotaUsageStore, UsageEvent},
    quota_usage_runtime,
    routing::Router,
};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE},
    Client, StatusCode,
};
use serde_json::Value;
use std::{env, sync::Arc, time::Duration};
use thiserror::Error;
use tracing::warn;

#[derive(Clone)]
pub struct Gateway {
    pub config: Arc<AppConfig>,
    pub router: Router,
    client: Client,
}

pub struct RoutedResponse {
    pub response: reqwest::Response,
    pub route: RouteConfig,
    pub usage_event_id: Option<String>,
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

        let estimated_input_tokens = QuotaUsageStore::estimate_input_tokens(body);
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
                    let status = response.status();
                    let usage_event_id = self
                        .record_usage(
                            account,
                            &route,
                            Some(status),
                            "success",
                            estimated_input_tokens,
                            None,
                        )
                        .await;
                    if let Some(usage) = quota_usage_runtime::get() {
                        if let Err(error) = usage.observe_headers(&account.id, response.headers()).await {
                            warn!(%error, account = %account.id, "failed to observe quota headers");
                        }
                        if let Err(error) = usage.mark_success(&account.id).await {
                            warn!(%error, account = %account.id, "failed to clear quota cooldown");
                        }
                    }
                    self.router.mark_success(&route.id).await;
                    return Ok(RoutedResponse {
                        response,
                        route,
                        usage_event_id,
                    });
                }
                Ok(response) => {
                    let status = response.status();
                    let retry_after = QuotaUsageStore::retry_after_seconds(response.headers());
                    if let Some(usage) = quota_usage_runtime::get() {
                        if let Err(error) = usage.observe_headers(&account.id, response.headers()).await {
                            warn!(%error, account = %account.id, "failed to observe quota headers");
                        }
                    }
                    let body_text = response.text().await.unwrap_or_default();
                    let retryable = is_retryable_status(status);
                    let cooldown = cooldown_for(status);
                    self.router
                        .mark_failure(&route.id, format!("HTTP {status}: {body_text}"), cooldown)
                        .await;

                    let outcome = if status == StatusCode::TOO_MANY_REQUESTS {
                        "rate_limited"
                    } else if matches!(status.as_u16(), 401 | 403) {
                        "authentication_error"
                    } else {
                        "upstream_error"
                    };
                    self.record_usage(
                        account,
                        &route,
                        Some(status),
                        outcome,
                        estimated_input_tokens,
                        Some(&body_text),
                    )
                    .await;
                    if let Some(usage) = quota_usage_runtime::get() {
                        if status == StatusCode::TOO_MANY_REQUESTS {
                            if let Err(error) = usage
                                .mark_rate_limited(&account.id, retry_after, &body_text)
                                .await
                            {
                                warn!(%error, account = %account.id, "failed to persist rate limit");
                            }
                        } else if matches!(status.as_u16(), 401 | 403) {
                            if let Err(error) = usage
                                .mark_account_cooldown(&account.id, cooldown, &body_text)
                                .await
                            {
                                warn!(%error, account = %account.id, "failed to persist account cooldown");
                            }
                        }
                    }

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
                    self.record_usage(
                        account,
                        &route,
                        None,
                        "transport_error",
                        estimated_input_tokens,
                        Some(&error.to_string()),
                    )
                    .await;
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| GatewayError::NoRoute(requested_model.to_string())))
    }

    async fn record_usage(
        &self,
        account: &AccountConfig,
        route: &RouteConfig,
        status: Option<StatusCode>,
        outcome: &str,
        input_tokens: u64,
        error: Option<&str>,
    ) -> Option<String> {
        let usage = quota_usage_runtime::get()?;
        match usage
            .record_event(UsageEvent {
                account_id: &account.id,
                route_id: &route.id,
                model: &route.model,
                status_code: status.map(|value| value.as_u16()),
                outcome,
                input_tokens,
                output_tokens: 0,
                usage_source: "estimated-input",
                error,
            })
            .await
        {
            Ok(event_id) => event_id,
            Err(error) => {
                warn!(%error, account = %account.id, route = %route.id, "failed to record usage event");
                None
            }
        }
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
