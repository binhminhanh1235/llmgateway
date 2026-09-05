use crate::{
    browser_provider::{BrowserProviderError, BrowserProviderRegistry},
    browser_provider_runtime,
    catalog::ModelCatalog,
    config::{AccountConfig, AppConfig, ClientPolicyConfig, ProviderConfig, RouteConfig},
    execution_trace::{AttemptRecord, ExecutionTraceError, ExecutionTraceStore},
    live_config::LiveConfig,
    quota_usage::{QuotaUsageStore, UsageEvent},
    quota_usage_runtime,
    routing::{RouteDecisionTrace, Router},
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

fn observe_terminal_sse_completion(buffer: &mut Vec<u8>, chunk: &[u8]) -> bool {
    buffer.extend_from_slice(chunk);
    let mut saw_terminal = false;

    loop {
        let lf_end = buffer
            .windows(2)
            .position(|window| window == b"\n\n")
            .map(|pos| (pos, 2));
        let crlf_end = buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|pos| (pos, 4));
        let Some((frame_end, delimiter_len)) = (match (lf_end, crlf_end) {
            (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        }) else {
            break;
        };

        let frame = String::from_utf8_lossy(&buffer[..frame_end]);
        let is_terminal = frame.lines().any(|line| {
            let Some(payload) = line.strip_prefix("data:") else {
                return false;
            };
            let payload = payload.trim();
            if payload == "[DONE]" {
                return true;
            }
            let Ok(value) = serde_json::from_str::<Value>(payload) else {
                return false;
            };
            value
                .get("choices")
                .and_then(Value::as_array)
                .is_some_and(|choices| {
                    choices.iter().any(|choice| {
                        choice
                            .get("finish_reason")
                            .is_some_and(|reason| !reason.is_null())
                    })
                })
        });
        buffer.drain(..frame_end + delimiter_len);
        saw_terminal |= is_terminal;
    }

    saw_terminal
}

fn stream_error_message(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(current) = source {
        let candidate = current.to_string();
        if !candidate.trim().is_empty() {
            message = candidate;
        }
        source = current.source();
    }
    message
}

fn upstream_stream_error_sse(message: &str) -> bytes::Bytes {
    let payload = serde_json::json!({
        "error": {
            "message": message,
            "type": "upstream_stream_error",
            "code": "upstream_stream_error"
        }
    });
    bytes::Bytes::from(format!("data: {payload}\n\n"))
}

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("{0}")]
    NoRoute(String),
    #[error("missing credential environment variable '{0}'")]
    MissingCredential(String),
    #[error("invalid upstream configuration: {0}")]
    InvalidConfig(String),
    #[error("client policy denied request: {0}")]
    ClientPolicyDenied(String),
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
    #[error("model_recipe_stale: {0}")]
    BrowserModelRecipeStale(String),
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

    pub fn effective_client_config(
        &self,
        config: Arc<AppConfig>,
        client_policy: Option<&ClientPolicyConfig>,
    ) -> Arc<AppConfig> {
        let Some(policy) = client_policy else {
            return config;
        };
        if policy.execution_preference.is_none() && policy.api_fallback.is_none() {
            return config;
        }

        let mut effective = (*config).clone();
        if let Some(preference) = &policy.execution_preference {
            effective.routing.execution_preference = preference.clone();
        }
        if let Some(api_fallback) = policy.api_fallback {
            effective.routing.api_fallback = api_fallback;
        }
        Arc::new(effective)
    }

    pub(crate) fn effective_request_config(
        &self,
        base_config: Arc<AppConfig>,
        client_policy: Option<&ClientPolicyConfig>,
        body: &Value,
    ) -> Result<Arc<AppConfig>, GatewayError> {
        let client_config = self.effective_client_config(base_config, client_policy);
        let request_preference = body
            .get("llmgateway_execution_preference")
            .and_then(Value::as_str);
        let request_api_fallback = body
            .get("llmgateway_api_fallback")
            .and_then(Value::as_bool);

        if request_preference.is_none() && request_api_fallback.is_none() {
            return Ok(client_config);
        }

        if let Some(preference) = request_preference {
            if !is_execution_preference(preference) {
                return Err(GatewayError::ClientPolicyDenied(format!(
                    "unsupported request execution preference '{preference}'"
                )));
            }
        }

        let client_policy_name = client_config.routing.execution_policy();
        let client_api_fallback = client_config.routing.api_fallback;
        let requested_policy_name = request_preference
            .map(normalize_execution_policy)
            .unwrap_or(client_policy_name);
        let requested_api_fallback = request_api_fallback.unwrap_or(client_api_fallback);

        if client_policy.is_some()
            && !transport_permissions_subset(
                requested_policy_name,
                requested_api_fallback,
                client_policy_name,
                client_api_fallback,
            )
        {
            return Err(GatewayError::ClientPolicyDenied(
                "request routing override exceeds the configured client transport permissions"
                    .into(),
            ));
        }

        let mut effective = (*client_config).clone();
        if let Some(preference) = request_preference {
            effective.routing.execution_preference = preference.to_string();
        }
        if let Some(api_fallback) = request_api_fallback {
            effective.routing.api_fallback = api_fallback;
        }
        Ok(Arc::new(effective))
    }

    pub async fn restore_adaptive_from_traces(&self) -> Result<usize, ExecutionTraceError> {
        let samples = self
            .execution_traces
            .adaptive_samples(self.config.routing.adaptive_history_samples)
            .await?;
        Ok(self
            .router
            .restore_adaptive_samples(samples.into_iter().map(|sample| {
                (
                    sample.route_id,
                    sample.success,
                    sample.duration_ms,
                    sample.observed_at_ms,
                )
            }))
            .await)
    }

    pub async fn execute_openai_chat_for_client(
        &self,
        requested_model: &str,
        body: &Value,
        client_policy: Option<&ClientPolicyConfig>,
    ) -> Result<RoutedResponse, GatewayError> {
        self.execute_openai_chat_with_affinity_for_client(
            requested_model,
            body,
            None,
            client_policy,
        )
        .await
    }

    pub async fn execute_openai_chat_with_affinity(
        &self,
        requested_model: &str,
        body: &Value,
        preferred_route: Option<&str>,
    ) -> Result<RoutedResponse, GatewayError> {
        self.execute_openai_chat_with_affinity_for_client(
            requested_model,
            body,
            preferred_route,
            None,
        )
        .await
    }

    pub async fn execute_openai_chat_with_affinity_for_client(
        &self,
        requested_model: &str,
        body: &Value,
        preferred_route: Option<&str>,
        client_policy: Option<&ClientPolicyConfig>,
    ) -> Result<RoutedResponse, GatewayError> {
        self.execute_openai_chat_with_context(
            requested_model,
            body,
            preferred_route,
            None,
            client_policy,
        )
        .await
    }

    pub async fn execute_openai_chat_with_thread_affinity(
        &self,
        requested_model: &str,
        body: &Value,
        preferred_route: Option<&str>,
        thread_id: &str,
    ) -> Result<RoutedResponse, GatewayError> {
        self.execute_openai_chat_with_context(
            requested_model,
            body,
            preferred_route,
            Some(thread_id),
            None,
        )
        .await
    }

    async fn execute_openai_chat_with_context(
        &self,
        requested_model: &str,
        body: &Value,
        preferred_route: Option<&str>,
        thread_id: Option<&str>,
        client_policy: Option<&ClientPolicyConfig>,
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

        let base_config = self.live_config.snapshot();
        if let Some(policy) = client_policy {
            let resolved = base_config.resolve_model_alias(requested_model);
            if !policy.model_allowed(requested_model, resolved) {
                let error = GatewayError::ClientPolicyDenied(format!(
                    "model '{requested_model}' is not allowed for this client"
                ));
                return Err(self.finish_execution_error(&request_id, error).await);
            }
        }
        let config = match self.effective_request_config(base_config, client_policy, body) {
            Ok(config) => config,
            Err(error) => return Err(self.finish_execution_error(&request_id, error).await),
        };
        let mut routes = self
            .router
            .plan_for_body_with_config(config.clone(), requested_model, Some(body))
            .await;
        if let Some(policy) = client_policy {
            routes.retain(|route| policy.route_allowed(&route.id));
        }
        if let Some(preferred_route) = preferred_route {
            if let Some(index) = routes.iter().position(|route| route.id == preferred_route) {
                let keep_sticky = index == 0
                    || self.router.sticky_route_matches_best_task_fit_with_config(
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
            let trace = self
                .router
                .explain_for_body_with_config(config.clone(), requested_model, Some(body))
                .await;
            let error = GatewayError::NoRoute(no_route_message(&trace));
            return Err(self.finish_execution_error(&request_id, error).await);
        }

        let upstream_body = sanitized_upstream_body(body);
        let estimated_input_tokens = QuotaUsageStore::estimate_input_tokens(&upstream_body);
        let mut last_error: Option<GatewayError> = None;

        for (attempt_index, route) in routes.into_iter().enumerate() {
            let account = match config.account(&route.account) {
                Some(account) => account,
                None => {
                    let error =
                        GatewayError::InvalidConfig(format!("unknown account '{}'", route.account));
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
                .send_route_chat(provider, account, &route, &upstream_body, thread_id)
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
                        if let Err(error) =
                            usage.observe_headers(&account.id, response.headers()).await
                        {
                            warn!(%error, account = %account.id, "failed to observe quota headers");
                        }
                        if let Err(error) = usage.mark_success(&account.id).await {
                            warn!(%error, account = %account.id, "failed to clear quota cooldown");
                        }
                    }
                    self.router
                        .mark_success(&route.id, adaptive_latency_ms)
                        .await;
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
                        if let Err(error) =
                            usage.observe_headers(&account.id, response.headers()).await
                        {
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
                                .require_attention(
                                    &account.id,
                                    &format!("HTTP {status}: {body_text}"),
                                )
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
                    let non_retryable_post_submit = !is_retryable_attempt_error(&error);
                    if let Some((route_cooldown_secs, adaptive_failure)) =
                        route_failure_policy(&error)
                    {
                        self.router
                            .mark_failure(
                                &route.id,
                                error_text.clone(),
                                route_cooldown_secs,
                                adaptive_latency_ms,
                                adaptive_failure,
                            )
                            .await;
                    }
                    let outcome = match &error {
                        GatewayError::BrowserSessionUnavailable(_) => "browser_session_unavailable",
                        GatewayError::BrowserTransport(_) => "browser_transport_error",
                        GatewayError::BrowserAdapterIncompatible(_) => {
                            "browser_adapter_incompatible"
                        }
                        GatewayError::BrowserModelUnavailable(_) => "browser_model_unavailable",
                        GatewayError::BrowserModelRecipeStale(_) => "model_recipe_stale",
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
                        retryable: !non_retryable_post_submit,
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
                    if non_retryable_post_submit {
                        return Err(self.finish_execution_error(&request_id, error).await);
                    }
                    last_error = Some(error);
                }
            }
        }

        let error = last_error.unwrap_or_else(|| {
            GatewayError::NoRoute(format!(
                "model '{}' had eligible routes, but none could be attempted",
                requested_model
            ))
        });
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

            let mut sse_buffer = Vec::<u8>::new();
            let mut saw_terminal = false;
            while let Some(item) = upstream.next().await {
                match item {
                    Ok(bytes) => {
                        guard.observe(bytes.len());
                        if observe_terminal_sse_completion(&mut sse_buffer, &bytes) {
                            saw_terminal = true;
                            guard.finish("completed", None).await;
                        }
                        yield Ok::<_, std::io::Error>(bytes);
                    }
                    Err(error) => {
                        let message = stream_error_message(&error);
                        guard.finish("failed", Some(&message)).await;
                        // Preserve a valid SSE response for downstream clients. Propagating a
                        // transport-level body error makes reqwest surface only the opaque
                        // "error decoding response body" message and hides the actual cause.
                        // Do not emit [DONE]: the stream is failed, not completed.
                        yield Ok::<_, std::io::Error>(upstream_stream_error_sse(&message));
                        return;
                    }
                }
            }

            if !saw_terminal {
                guard
                    .finish(
                        "failed",
                        Some("upstream stream ended before terminal completion frame"),
                    )
                    .await;
            }
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
        thread_id: Option<&str>,
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
                    .execute_chat(provider, account, route, body, thread_id)
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

fn is_execution_preference(value: &str) -> bool {
    matches!(
        value,
        "browser-first"
            | "prefer-browser"
            | "browser-only"
            | "balanced"
            | "api-first"
            | "prefer-api"
            | "api-only"
    )
}

fn normalize_execution_policy(value: &str) -> &str {
    match value {
        "browser-first" | "prefer-browser" => "prefer-browser",
        "browser-only" => "browser-only",
        "api-first" | "prefer-api" => "prefer-api",
        "api-only" => "api-only",
        _ => "balanced",
    }
}

fn transport_permissions(policy: &str, api_fallback: bool) -> (bool, bool) {
    match policy {
        "browser-only" => (true, false),
        "api-only" => (false, true),
        "prefer-browser" if !api_fallback => (true, false),
        _ => (true, true),
    }
}

fn transport_permissions_subset(
    requested_policy: &str,
    requested_api_fallback: bool,
    client_policy: &str,
    client_api_fallback: bool,
) -> bool {
    let requested = transport_permissions(requested_policy, requested_api_fallback);
    let allowed = transport_permissions(client_policy, client_api_fallback);
    (!requested.0 || allowed.0) && (!requested.1 || allowed.1)
}

fn sanitized_upstream_body(body: &Value) -> Value {
    let mut sanitized = body.clone();
    if let Some(object) = sanitized.as_object_mut() {
        object.remove("llmgateway_task");
        object.remove("llmgateway_execution_preference");
        object.remove("llmgateway_api_fallback");
    }
    sanitized
}

fn map_browser_provider_error(error: BrowserProviderError) -> GatewayError {
    match error {
        BrowserProviderError::InvalidConfig(_)
        | BrowserProviderError::UnsupportedAdapter(_)
        | BrowserProviderError::UnsupportedBrowserless(_)
        | BrowserProviderError::InvalidTransportPolicy(_)
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
        BrowserProviderError::ModelRecipeStale { .. } => {
            GatewayError::BrowserModelRecipeStale(error.to_string())
        }
        BrowserProviderError::Transport(_) => GatewayError::BrowserTransport(error.to_string()),
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

fn no_route_message(trace: &RouteDecisionTrace) -> String {
    if trace.candidates.is_empty() {
        return format!(
            "model '{}' has no configured or discovered routes",
            trace.requested_model
        );
    }

    let details = trace
        .candidates
        .iter()
        .take(6)
        .map(|candidate| {
            let mut reasons = candidate.exclusion_reasons.clone();
            if let Some(adapter_message) = candidate
                .readiness
                .browser_adapter_message
                .as_deref()
                .filter(|message| !message.trim().is_empty())
            {
                let adapter_detail = format!("adapter: {adapter_message}");
                if !reasons.iter().any(|reason| reason == &adapter_detail) {
                    reasons.push(adapter_detail);
                }
            }
            if reasons.is_empty() {
                reasons.push("route_not_eligible".into());
            }
            format!("{} [{}]", candidate.route_id, reasons.join(", "))
        })
        .collect::<Vec<_>>()
        .join("; ");

    format!(
        "model '{}' has no eligible routes: {details}",
        trace.requested_model
    )
}

fn is_model_binding_conflict(error: &GatewayError) -> bool {
    matches!(
        error,
        GatewayError::BrowserTransport(msg)
            if msg.contains("native conversation is already bound to model")
    )
}

fn route_failure_policy(error: &GatewayError) -> Option<(i64, bool)> {
    if is_model_binding_conflict(error) {
        return None;
    }

    let adaptive_failure = matches!(
        error,
        GatewayError::Transport(_) | GatewayError::BrowserTransport(_)
    );
    let route_cooldown_secs = match error {
        GatewayError::BrowserAdapterIncompatible(_)
        | GatewayError::BrowserModelUnavailable(_)
        | GatewayError::BrowserModelRecipeStale(_) => 0,
        GatewayError::BrowserSessionUnavailable(_) => 2,
        _ => 10,
    };
    Some((route_cooldown_secs, adaptive_failure))
}

fn is_retryable_attempt_error(error: &GatewayError) -> bool {
    !matches!(error, GatewayError::BrowserModelRecipeStale(_))
        && !is_model_binding_conflict(error)
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

#[cfg(test)]
mod client_policy_tests {
    use super::{normalize_execution_policy, transport_permissions_subset};

    #[test]
    fn request_transport_permissions_can_narrow_but_not_broaden_client_policy() {
        assert!(transport_permissions_subset(
            "browser-only",
            false,
            "prefer-browser",
            true,
        ));
        assert!(transport_permissions_subset(
            "prefer-browser",
            false,
            "browser-only",
            false,
        ));
        assert!(!transport_permissions_subset(
            "api-only",
            true,
            "browser-only",
            false,
        ));
        assert!(!transport_permissions_subset(
            "balanced",
            false,
            "prefer-browser",
            false,
        ));
        assert!(!transport_permissions_subset(
            "browser-only",
            false,
            "api-only",
            true,
        ));
    }

    #[test]
    fn legacy_execution_preference_aliases_normalize_before_permission_checks() {
        assert_eq!(normalize_execution_policy("browser-first"), "prefer-browser");
        assert_eq!(normalize_execution_policy("api-first"), "prefer-api");
        assert_eq!(normalize_execution_policy("balanced"), "balanced");
    }
}

#[cfg(test)]
mod stream_trace_tests {
    use super::{
        is_retryable_attempt_error, observe_terminal_sse_completion, route_failure_policy,
        stream_error_message, upstream_stream_error_sse, GatewayError,
    };

    #[test]
    fn stale_model_recipe_is_not_retryable_after_submit() {
        assert!(!is_retryable_attempt_error(
            &GatewayError::BrowserModelRecipeStale("stale".into())
        ));
        assert!(!is_retryable_attempt_error(
            &GatewayError::BrowserTransport(
                "Gemini native conversation is already bound to model 'pro'".into()
            )
        ));
        assert!(is_retryable_attempt_error(&GatewayError::BrowserTransport(
            "network".into()
        )));
    }

    #[test]
    fn model_binding_conflict_does_not_mutate_route_health() {
        for provider in ["Gemini", "Qwen"] {
            let conflict = GatewayError::BrowserTransport(format!(
                "{provider} native conversation is already bound to model 'pro'"
            ));
            assert!(!is_retryable_attempt_error(&conflict));
            assert_eq!(route_failure_policy(&conflict), None);
        }

        assert_eq!(
            route_failure_policy(&GatewayError::BrowserTransport("network".into())),
            Some((10, true))
        );
        assert_eq!(
            route_failure_policy(&GatewayError::BrowserSessionUnavailable(
                "browser unavailable".into()
            )),
            Some((2, false))
        );
    }

    #[test]
    fn terminal_detector_requires_a_complete_sse_data_frame() {
        let mut buffer = Vec::new();
        assert!(!observe_terminal_sse_completion(
            &mut buffer,
            b"data: {\"message\":\"please wait for data: [DONE]\"}\n\n",
        ));
        assert!(!observe_terminal_sse_completion(&mut buffer, b"data: [DO"));
        assert!(observe_terminal_sse_completion(&mut buffer, b"NE]\n\n"));
    }

    #[test]
    fn terminal_detector_accepts_finish_reason_without_done_marker() {
        let mut buffer = Vec::new();
        assert!(!observe_terminal_sse_completion(
            &mut buffer,
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
        ));
        assert!(observe_terminal_sse_completion(
            &mut buffer,
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ));
    }

    #[derive(Debug)]
    struct NestedStreamError;

    impl std::fmt::Display for NestedStreamError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("forced browser stream poll failure")
        }
    }

    impl std::error::Error for NestedStreamError {}

    #[test]
    fn stream_error_message_prefers_the_deepest_source() {
        let error = std::io::Error::new(std::io::ErrorKind::Other, NestedStreamError);
        assert_eq!(
            stream_error_message(&error),
            "forced browser stream poll failure"
        );
    }

    #[test]
    fn upstream_stream_errors_are_emitted_as_valid_non_terminal_sse() {
        let frame = String::from_utf8(upstream_stream_error_sse("browser poll failed").to_vec())
            .expect("SSE error frame should be UTF-8");
        assert!(frame.starts_with("data: "));
        assert!(frame.ends_with("\n\n"));
        assert!(!frame.contains("[DONE]"));

        let payload = frame
            .strip_prefix("data: ")
            .expect("SSE data prefix")
            .trim();
        let value: serde_json::Value =
            serde_json::from_str(payload).expect("valid JSON SSE payload");
        assert_eq!(value["error"]["code"], "upstream_stream_error");
        assert_eq!(value["error"]["message"], "browser poll failed");
    }

    #[test]
    fn terminal_done_detector_accepts_crlf_frames() {
        let mut buffer = Vec::new();
        assert!(observe_terminal_sse_completion(
            &mut buffer,
            b"event: message\r\ndata: [DONE]\r\n\r\n",
        ));
    }
}
