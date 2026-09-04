use crate::{
    browser_provider::{BrowserProviderError, BrowserProviderRegistry},
    browser_provider_runtime,
    catalog::ModelCatalog,
    config::{AccountConfig, AppConfig, ProviderConfig, RouteConfig},
    execution_trace::{AttemptRecord, ExecutionTraceError, ExecutionTraceStore},
    live_config::LiveConfig,
    quota_usage::{QuotaUsageStore, UsageEvent},
    quota_usage_runtime,
    routing::Router,
};
use axum::http::Response as HttpResponse;
use futures_util::StreamExt;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE},
    Client, StatusCode,
};
use serde_json::Value;
use std::{
    env,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use tracing::warn;
use uuid::Uuid;

#[derive(Clone)]
pub struct Gateway {
    pub config: Arc<AppConfig>,
    pub live_config: LiveConfig,
    pub router: Router,
    pub execution_traces: Arc<ExecutionTraceStore>,
    client: Client,
}

pub struct RoutedResponse {
    pub response: reqwest::Response,
    pub route: RouteConfig,
    pub usage_event_id: Option<String>,
    pub request_id: String,
    pub started_at: Instant,
}

struct ExecutionStreamGuard {
    store: Arc<ExecutionTraceStore>,
    request_id: String,
    route_id: String,
    started_at: Instant,
    first_byte_ms: Option<u128>,
    chunk_count: u64,
    byte_count: u64,
    finished: bool,
}

impl ExecutionStreamGuard {
    fn observe(&mut self, bytes: usize) {
        if self.first_byte_ms.is_none() {
            self.first_byte_ms = Some(self.started_at.elapsed().as_millis());
        }
        self.chunk_count = self.chunk_count.saturating_add(1);
        self.byte_count = self.byte_count.saturating_add(bytes as u64);
    }

    async fn finish(&mut self, outcome: &str, error: Option<&str>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let partial = outcome != "completed" && self.chunk_count > 0;
        if let Err(trace_error) = self
            .store
            .finish_stream(
                &self.request_id,
                &self.route_id,
                self.first_byte_ms,
                self.chunk_count,
                self.byte_count,
                outcome,
                partial,
                error,
            )
            .await
        {
            warn!(%trace_error, request_id = %self.request_id, "failed to finish execution stream trace");
        }
    }
}

impl Drop for ExecutionStreamGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let store = self.store.clone();
        let request_id = self.request_id.clone();
        let route_id = self.route_id.clone();
        let first_byte_ms = self.first_byte_ms;
        let chunk_count = self.chunk_count;
        let byte_count = self.byte_count;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let error = "downstream stream dropped before completion";
                let _ = store
                    .finish_stream(
                        &request_id,
                        &route_id,
                        first_byte_ms,
                        chunk_count,
                        byte_count,
                        "cancelled",
                        chunk_count > 0,
                        Some(error),
                    )
                    .await;
            });
        }
    }
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
    #[error("browser session unavailable: {0}")]
    BrowserSessionUnavailable(String),
    #[error("browser transport failed: {0}")]
    BrowserTransport(String),
    #[error("browser adapter incompatible: {0}")]
    BrowserAdapterIncompatible(String),
    #[error("browser model unavailable: {0}")]
    BrowserModelUnavailable(String),
    #[error("upstream rejected request with {status}: {body}")]
    Upstream { status: StatusCode, body: String },
    #[error("{source}")]
    Execution {
        request_id: String,
        #[source]
        source: Box<GatewayError>,
    },
}

impl Gateway {
    pub fn new(
        config: Arc<AppConfig>,
        live_config: LiveConfig,
        catalog: Arc<ModelCatalog>,
        execution_traces: Arc<ExecutionTraceStore>,
    ) -> Result<Self, GatewayError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|error| GatewayError::Transport(error.to_string()))?;
        let router = Router::new(config.clone(), live_config.clone(), catalog);
        Ok(Self {
            config,
            live_config,
            router,
            execution_traces,
            client,
        })
    }

    pub fn config_snapshot(&self) -> Arc<AppConfig> {
        self.live_config.snapshot()
    }

    pub async fn restore_adaptive_from_traces(&self) -> Result<usize, ExecutionTraceError> {
        let samples = self
            .execution_traces
            .adaptive_samples(self.config.routing.adaptive_history_samples)
            .await?;
        Ok(self
            .router
            .restore_adaptive_samples(
                samples
                    .into_iter()
                    .map(|sample| (
                        sample.route_id,
                        sample.success,
                        sample.duration_ms,
                        sample.observed_at_ms,
                    )),
            )
            .await)
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
        let started_at = Instant::now();
        let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let request_id = match self
            .execution_traces
            .start(requested_model, preferred_route)
            .await
        {
            Ok(request_id) => request_id,
            Err(error) => {
                warn!(%error, "failed to start execution trace; continuing request");
                format!("req_{}", Uuid::new_v4().simple())
            }
        };

        let config = self.live_config.snapshot();
        let mut routes = self
            .router
            .plan_for_body_with_config(config.clone(), requested_model, Some(body))
            .await;
        if let Some(preferred_route) = preferred_route {
            if let Some(index) = routes.iter().position(|route| route.id == preferred_route) {
                let keep_sticky = index == 0
                    || self
                        .router
                        .sticky_route_matches_best_task_fit_with_config(
                            config.as_ref(),
                            requested_model,
                            body,
                            &routes[index],
                            &routes[0],
                        );
                if keep_sticky {
                    let preferred = routes.remove(index);
                    routes.insert(0, preferred);
                }
            }
        }
        if routes.is_empty() {
            let error = GatewayError::NoRoute(requested_model.to_string());
            return Err(self.finish_execution_error(&request_id, error).await);
        }

        let upstream_body = sanitized_upstream_body(body);
        let estimated_input_tokens = QuotaUsageStore::estimate_input_tokens(&upstream_body);
        let mut last_error: Option<GatewayError> = None;

        for (attempt_index, route) in routes.into_iter().enumerate() {
            let account = match config.account(&route.account) {
                Some(account) => account,
                None => {
                    let error = GatewayError::InvalidConfig(format!(
                        "unknown account '{}'",
                        route.account
                    ));
                    return Err(self.finish_execution_error(&request_id, error).await);
                }
            };
            if !account.enabled {
                continue;
            }
            let provider = match config.provider(&account.provider) {
                Some(provider) => provider,
                None => {
                    let error = GatewayError::InvalidConfig(format!(
                        "unknown provider '{}'",
                        account.provider
                    ));
                    return Err(self.finish_execution_error(&request_id, error).await);
                }
            };

            let attempt_started = Instant::now();
            match self
                .send_route_chat(provider, account, &route, &upstream_body)
                .await
            {
                Ok(response) if response.status().is_success() => {
                    let status = response.status();
                    let duration_ms = attempt_started.elapsed().as_millis();
                    let adaptive_latency_ms = duration_ms.min(u64::MAX as u128) as u64;
                    self.record_execution_attempt(AttemptRecord {
                        request_id: &request_id,
                        attempt_index,
                        route_id: &route.id,
                        account_id: &account.id,
                        model: &route.model,
                        status_code: Some(status.as_u16()),
                        outcome: "success",
                        retryable: false,
                        duration_ms,
                        error: None,
                    })
                    .await;
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
                    self.router.mark_success(&route.id, adaptive_latency_ms).await;
                    if is_stream {
                        if let Err(error) = self
                            .execution_traces
                            .start_stream(&request_id, &route.id)
                            .await
                        {
                            warn!(%error, request_id, "failed to mark execution stream as started");
                        }
                    } else {
                        self.complete_execution(&request_id, "success", Some(&route.id), None)
                            .await;
                    }
                    return Ok(RoutedResponse {
                        response,
                        route,
                        usage_event_id,
                        request_id,
                        started_at,
                    });
                }
                Ok(response) => {
                    let status = response.status();
                    let duration_ms = attempt_started.elapsed().as_millis();
                    let adaptive_latency_ms = duration_ms.min(u64::MAX as u128) as u64;
                    let retry_after = QuotaUsageStore::retry_after_seconds(response.headers());
                    if let Some(usage) = quota_usage_runtime::get() {
                        if let Err(error) = usage.observe_headers(&account.id, response.headers()).await {
                            warn!(%error, account = %account.id, "failed to observe quota headers");
                        }
                    }
                    let body_text = response.text().await.unwrap_or_default();
                    let retryable = is_retryable_status(status);
                    let cooldown = cooldown_for(status);
                    let adaptive_failure =
                        status == StatusCode::REQUEST_TIMEOUT || status.is_server_error();
                    self.router
                        .mark_failure(
                            &route.id,
                            format!("HTTP {status}: {body_text}"),
                            cooldown,
                            adaptive_latency_ms,
                            adaptive_failure,
                        )
                        .await;

                    let outcome = if status == StatusCode::TOO_MANY_REQUESTS {
                        "rate_limited"
                    } else if matches!(status.as_u16(), 401 | 403) {
                        "authentication_error"
                    } else {
                        "upstream_error"
                    };
                    self.record_execution_attempt(AttemptRecord {
                        request_id: &request_id,
                        attempt_index,
                        route_id: &route.id,
                        account_id: &account.id,
                        model: &route.model,
                        status_code: Some(status.as_u16()),
                        outcome,
                        retryable,
                        duration_ms,
                        error: Some(&body_text),
                    })
                    .await;
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
                    if matches!(status.as_u16(), 401 | 403)
                        && BrowserProviderRegistry::is_browser_kind(&provider.kind)
                    {
                        if let Some(registry) = browser_provider_runtime::get() {
                            if let Err(error) = registry
                                .require_attention(&account.id, &format!("HTTP {status}: {body_text}"))
                                .await
                            {
                                warn!(%error, account = %account.id, "failed to mark browser session as requiring attention");
                            }
                        }
                    }

                    let error = GatewayError::Upstream {
                        status,
                        body: body_text,
                    };
                    if !retryable {
                        return Err(self.finish_execution_error(&request_id, error).await);
                    }
                    last_error = Some(error);
                }
                Err(error) => {
                    let duration_ms = attempt_started.elapsed().as_millis();
                    let adaptive_latency_ms = duration_ms.min(u64::MAX as u128) as u64;
                    let error_text = error.to_string();
                    let adaptive_failure = matches!(
                        &error,
                        GatewayError::Transport(_) | GatewayError::BrowserTransport(_)
                    );
                    self.router
                        .mark_failure(
                            &route.id,
                            error_text.clone(),
                            10,
                            adaptive_latency_ms,
                            adaptive_failure,
                        )
                        .await;
                    let outcome = match &error {
                        GatewayError::BrowserSessionUnavailable(_) => "browser_session_unavailable",
                        GatewayError::BrowserTransport(_) => "browser_transport_error",
                        GatewayError::BrowserAdapterIncompatible(_) => "browser_adapter_incompatible",
                        GatewayError::BrowserModelUnavailable(_) => "browser_model_unavailable",
                        _ => "transport_error",
                    };
                    self.record_execution_attempt(AttemptRecord {
                        request_id: &request_id,
                        attempt_index,
                        route_id: &route.id,
                        account_id: &account.id,
                        model: &route.model,
                        status_code: None,
                        outcome,
                        retryable: true,
                        duration_ms,
                        error: Some(&error_text),
                    })
                    .await;
                    self.record_usage(
                        account,
                        &route,
                        None,
                        outcome,
                        estimated_input_tokens,
                        Some(&error_text),
                    )
                    .await;
                    last_error = Some(error);
                }
            }
        }

        let error = last_error.unwrap_or_else(|| GatewayError::NoRoute(requested_model.to_string()));
        Err(self.finish_execution_error(&request_id, error).await)
    }

    pub fn trace_stream_response(
        &self,
        response: reqwest::Response,
        request_id: String,
        route_id: String,
        started_at: Instant,
    ) -> reqwest::Response {
        let status = response.status();
        let version = response.version();
        let headers = response.headers().clone();
        let mut upstream = response.bytes_stream();
        let store = self.execution_traces.clone();

        let stream = async_stream::stream! {
            let mut guard = ExecutionStreamGuard {
                store,
                request_id,
                route_id,
                started_at,
                first_byte_ms: None,
                chunk_count: 0,
                byte_count: 0,
                finished: false,
            };

            let mut sse_tail = Vec::<u8>::new();
            while let Some(item) = upstream.next().await {
                match item {
                    Ok(bytes) => {
                        guard.observe(bytes.len());
                        let mut scan = Vec::with_capacity(sse_tail.len() + bytes.len());
                        scan.extend_from_slice(&sse_tail);
                        scan.extend_from_slice(&bytes);
                        if scan.windows(b"data: [DONE]".len()).any(|window| window == b"data: [DONE]") {
                            guard.finish("completed", None).await;
                        }
                        let keep = scan.len().min(32);
                        sse_tail.clear();
                        sse_tail.extend_from_slice(&scan[scan.len().saturating_sub(keep)..]);
                        yield Ok::<_, std::io::Error>(bytes);
                    }
                    Err(error) => {
                        let message = error.to_string();
                        guard.finish("failed", Some(&message)).await;
                        yield Err(std::io::Error::other(message));
                        return;
                    }
                }
            }

            guard.finish("completed", None).await;
        };

        let mut response = HttpResponse::builder()
            .status(status)
            .version(version)
            .body(reqwest::Body::wrap_stream(stream))
            .expect("stream response builder uses validated upstream metadata");
        *response.headers_mut() = headers;
        reqwest::Response::from(response)
    }

    async fn record_execution_attempt(&self, record: AttemptRecord<'_>) {
        if let Err(error) = self.execution_traces.record_attempt(record).await {
            warn!(%error, "failed to record execution attempt");
        }
    }

    async fn complete_execution(
        &self,
        request_id: &str,
        status: &str,
        selected_route: Option<&str>,
        final_error: Option<&str>,
    ) {
        if let Err(error) = self
            .execution_traces
            .complete(request_id, status, selected_route, final_error)
            .await
        {
            warn!(%error, request_id, "failed to complete execution trace");
        }
    }

    async fn finish_execution_error(&self, request_id: &str, error: GatewayError) -> GatewayError {
        let error_text = error.to_string();
        self.complete_execution(request_id, "failed", None, Some(&error_text))
            .await;
        GatewayError::Execution {
            request_id: request_id.to_string(),
            source: Box::new(error),
        }
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

    async fn send_route_chat(
        &self,
        provider: &ProviderConfig,
        account: &AccountConfig,
        route: &RouteConfig,
        body: &Value,
    ) -> Result<reqwest::Response, GatewayError> {
        match provider.kind.as_str() {
            "openai-compatible" => self.send_openai_chat(provider, account, route, body).await,
            kind if BrowserProviderRegistry::is_browser_kind(kind) => {
                let registry = browser_provider_runtime::get().ok_or_else(|| {
                    GatewayError::InvalidConfig(
                        "browser provider runtime is not initialized".into(),
                    )
                })?;
                registry
                    .execute_chat(provider, account, route, body)
                    .await
                    .map_err(map_browser_provider_error)
            }
            other => Err(GatewayError::InvalidConfig(format!(
                "provider '{}' uses unsupported kind '{}'",
                provider.id, other
            ))),
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

fn sanitized_upstream_body(body: &Value) -> Value {
    let mut sanitized = body.clone();
    if let Some(object) = sanitized.as_object_mut() {
        object.remove("llmgateway_task");
    }
    sanitized
}

fn map_browser_provider_error(error: BrowserProviderError) -> GatewayError {
    match error {
        BrowserProviderError::InvalidConfig(_)
        | BrowserProviderError::UnsupportedAdapter(_)
        | BrowserProviderError::MissingBinding(_)
        | BrowserProviderError::Io(_)
        | BrowserProviderError::Toml(_) => GatewayError::InvalidConfig(error.to_string()),
        BrowserProviderError::SessionUnavailable { .. } => {
            GatewayError::BrowserSessionUnavailable(error.to_string())
        }
        BrowserProviderError::AdapterIncompatible { .. } => {
            GatewayError::BrowserAdapterIncompatible(error.to_string())
        }
        BrowserProviderError::ModelUnavailable { .. } => {
            GatewayError::BrowserModelUnavailable(error.to_string())
        }
        BrowserProviderError::Transport(_) => GatewayError::BrowserTransport(error.to_string())
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
