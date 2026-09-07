use crate::{
    account_runtime::{
        AccountAdmissionPermit, AccountRuntimeRegistry, AccountRuntimeSnapshot,
        ProviderRuntimePolicy,
    },
    browser_provider::{BrowserProviderError, BrowserProviderRegistry},
    browser_provider_runtime,
    catalog::ModelCatalog,
    config::{AccountConfig, AppConfig, ClientPolicyConfig, ProviderConfig, RouteConfig},
    execution::{
        ExecutionBudget, ExecutionBudgetTracker, ExecutionFailure, ExecutionPhase, FailureClass,
        FailureScope, ReplaySafety, StreamCommitBarrier,
    },
    execution_trace::{AttemptRecord, ExecutionTraceError, ExecutionTraceStore},
    live_config::LiveConfig,
    quota_usage::{QuotaUsageStore, UsageEvent},
    quota_usage_runtime,
    routing::{RouteDecisionTrace, Router},
    runtime_health::RuntimeHealthGraph,
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
    account_runtimes: Arc<AccountRuntimeRegistry>,
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
    barrier: StreamCommitBarrier,
}

impl ExecutionStreamGuard {
    fn observe(&mut self, bytes: usize) {
        if bytes > 0 {
            self.barrier.commit_client_visible();
        }
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
        let committed = self.barrier.committed();
        let replay_safety = self.barrier.replay_safety();
        self.barrier.finish();
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
                self.barrier.phase().as_str(),
                replay_safety.as_str(),
                committed,
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
        let committed = self.barrier.committed();
        let replay_safety = self.barrier.replay_safety();
        self.barrier.cancel();
        let execution_phase = self.barrier.phase();
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
                        execution_phase.as_str(),
                        replay_safety.as_str(),
                        committed,
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

fn hold_account_admission(
    response: reqwest::Response,
    permit: AccountAdmissionPermit,
) -> reqwest::Response {
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let mut upstream = response.bytes_stream();
    let stream = async_stream::stream! {
        let _permit = permit;
        while let Some(item) = upstream.next().await {
            yield item;
        }
    };
    let mut response = HttpResponse::builder()
        .status(status)
        .version(version)
        .body(reqwest::Body::wrap_stream(stream))
        .expect("admission response builder uses validated upstream metadata");
    *response.headers_mut() = headers;
    reqwest::Response::from(response)
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
    #[error("{0}")]
    ModelBindingConflict(String),
    #[error("browser model unavailable: {0}")]
    BrowserModelUnavailable(String),
    #[error("model_recipe_stale: {0}")]
    BrowserModelRecipeStale(String),
    #[error("upstream rejected request with {status}: {body}")]
    Upstream { status: StatusCode, body: String },
    #[error("{source}")]
    Classified {
        failure: Box<ExecutionFailure>,
        #[source]
        source: Box<GatewayError>,
    },
    #[error("{source}")]
    Execution {
        request_id: String,
        #[source]
        source: Box<GatewayError>,
    },
}

impl Gateway {
    #[allow(dead_code)]
    pub fn new(
        config: Arc<AppConfig>,
        live_config: LiveConfig,
        catalog: Arc<ModelCatalog>,
        execution_traces: Arc<ExecutionTraceStore>,
    ) -> Result<Self, GatewayError> {
        Self::with_runtime_health(
            config,
            live_config,
            catalog,
            execution_traces,
            Arc::new(RuntimeHealthGraph::default()),
        )
    }

    pub fn with_runtime_health(
        config: Arc<AppConfig>,
        live_config: LiveConfig,
        catalog: Arc<ModelCatalog>,
        execution_traces: Arc<ExecutionTraceStore>,
        runtime_health: Arc<RuntimeHealthGraph>,
    ) -> Result<Self, GatewayError> {
        Self::with_runtime_fabric(
            config,
            live_config,
            catalog,
            execution_traces,
            runtime_health,
            Arc::new(AccountRuntimeRegistry::default()),
        )
    }

    pub fn with_runtime_fabric(
        config: Arc<AppConfig>,
        live_config: LiveConfig,
        catalog: Arc<ModelCatalog>,
        execution_traces: Arc<ExecutionTraceStore>,
        runtime_health: Arc<RuntimeHealthGraph>,
        account_runtimes: Arc<AccountRuntimeRegistry>,
    ) -> Result<Self, GatewayError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|error| GatewayError::Transport(error.to_string()))?;
        let router = Router::with_runtime_health(
            config.clone(),
            live_config.clone(),
            catalog,
            runtime_health,
        );
        Ok(Self {
            config,
            live_config,
            router,
            execution_traces,
            account_runtimes,
            client,
        })
    }

    pub fn config_snapshot(&self) -> Arc<AppConfig> {
        self.live_config.snapshot()
    }

    pub fn set_account_runtime_enabled(&self, account_id: &str, enabled: bool) {
        if enabled {
            self.account_runtimes.activate_account(account_id);
        } else {
            self.account_runtimes.stop_account(account_id);
        }
    }

    pub fn account_runtime_snapshot(
        &self,
        provider: &ProviderConfig,
        account: &AccountConfig,
    ) -> AccountRuntimeSnapshot {
        let policy = self.account_runtime_policy(provider, account);
        let snapshot = self
            .account_runtimes
            .snapshot_or_create(&provider.id, &account.id, policy);
        if account.enabled {
            snapshot
        } else {
            self.account_runtimes.stop_account(&account.id);
            self.account_runtimes
                .snapshot_or_create(&provider.id, &account.id, policy)
        }
    }

    fn account_runtime_policy(
        &self,
        provider: &ProviderConfig,
        account: &AccountConfig,
    ) -> ProviderRuntimePolicy {
        if BrowserProviderRegistry::is_browser_kind(&provider.kind) {
            browser_provider_runtime::get()
                .map(|registry| registry.account_runtime_policy(&provider.kind, &account.id))
                .unwrap_or_else(ProviderRuntimePolicy::serialized_browser)
        } else {
            ProviderRuntimePolicy::api_default()
        }
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
        let request_api_fallback = body.get("llmgateway_api_fallback").and_then(Value::as_bool);

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
        let mut recovery_reason: Option<String> = None;
        let mut budget_tracker =
            ExecutionBudgetTracker::new(ExecutionBudget::for_candidate_count(routes.len()));

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
            let runtime_health_permit = match self
                .router
                .acquire_runtime_health_with_config(config.as_ref(), &route)
                .await
            {
                Ok(permit) => permit,
                Err(blocked) => {
                    recovery_reason = blocked
                        .snapshot
                        .last_failure_class
                        .clone()
                        .or_else(|| Some("runtime_breaker_open".into()));
                    continue;
                }
            };
            if !budget_tracker.try_begin_attempt() {
                self.router
                    .release_runtime_health(runtime_health_permit)
                    .await;
                last_error = Some(GatewayError::Transport(
                    "execution recovery budget exhausted".into(),
                ));
                break;
            }

            let selected_transport = selected_transport_label(provider);
            let logical_candidate = format!("{}/{}/{}", account.provider, account.id, route.model);
            let attempt_started = Instant::now();
            let admission_policy = self.account_runtime_policy(provider, account);
            let account_admission = match self
                .account_runtimes
                .acquire(
                    &provider.id,
                    &account.id,
                    admission_policy,
                    budget_tracker.max_queue_wait(),
                    budget_tracker.remaining(),
                )
                .await
            {
                Ok(permit) => permit,
                Err(rejection) => {
                    self.router
                        .release_runtime_health(runtime_health_permit)
                        .await;
                    let failure = rejection.execution_failure(
                        &provider.id,
                        &account.id,
                        &route.model,
                        selected_transport,
                    );
                    let error_text = rejection.to_string();
                    self.record_execution_attempt(AttemptRecord {
                        request_id: &request_id,
                        attempt_index,
                        route_id: &route.id,
                        account_id: &account.id,
                        model: &route.model,
                        status_code: None,
                        outcome: "admission_rejected",
                        retryable: true,
                        duration_ms: attempt_started.elapsed().as_millis(),
                        error: Some(&error_text),
                        failure_class: Some(failure.class.as_str()),
                        execution_phase: failure.phase.as_str(),
                        replay_safety: failure.replay_safety.as_str(),
                        committed: false,
                        selected_transport: Some(selected_transport),
                        recovery_reason: recovery_reason.as_deref(),
                        logical_candidate: Some(&logical_candidate),
                    })
                    .await;
                    recovery_reason = Some(failure.class.as_str().to_string());
                    last_error = Some(GatewayError::Classified {
                        failure: Box::new(failure),
                        source: Box::new(GatewayError::Transport(error_text)),
                    });
                    continue;
                }
            };
            match self
                .send_route_chat(provider, account, &route, &upstream_body, thread_id)
                .await
            {
                Ok(response) if response.status().is_success() => {
                    account_admission.observe_success();
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
                        failure_class: None,
                        execution_phase: if is_stream {
                            ExecutionPhase::Submitted.as_str()
                        } else {
                            ExecutionPhase::Terminal.as_str()
                        },
                        replay_safety: if is_stream {
                            ReplaySafety::ProbablySafe.as_str()
                        } else {
                            ReplaySafety::Unsafe.as_str()
                        },
                        committed: !is_stream,
                        selected_transport: Some(selected_transport),
                        recovery_reason: recovery_reason.as_deref(),
                        logical_candidate: Some(&logical_candidate),
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
                    self.router
                        .mark_runtime_success(runtime_health_permit)
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
                    let response = hold_account_admission(response, account_admission);
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
                    let failure = normalized_status_failure(
                        status,
                        &body_text,
                        &account.provider,
                        &account.id,
                        &route.model,
                        selected_transport,
                        BrowserProviderRegistry::is_browser_kind(&provider.kind),
                    );
                    account_admission.observe_failure(failure.class);
                    let retryable = failure.allows_silent_fallback(false);
                    let cooldown = failure
                        .suggested_cooldown_secs
                        .unwrap_or_else(|| cooldown_for(status));
                    let adaptive_failure = failure_is_adaptive(&failure);
                    let route_cooldown = route_cooldown_for_failure(&failure, cooldown);
                    self.router
                        .mark_failure(
                            &route.id,
                            format!("HTTP {status}: {body_text}"),
                            route_cooldown,
                            adaptive_latency_ms,
                            adaptive_failure,
                        )
                        .await;
                    self.router
                        .mark_runtime_failure(
                            runtime_health_permit,
                            &failure,
                            &format!("HTTP {status}: {body_text}"),
                            cooldown,
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
                        failure_class: Some(failure.class.as_str()),
                        execution_phase: failure.phase.as_str(),
                        replay_safety: failure.replay_safety.as_str(),
                        committed: false,
                        selected_transport: Some(selected_transport),
                        recovery_reason: recovery_reason.as_deref(),
                        logical_candidate: Some(&logical_candidate),
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
                    recovery_reason = Some(failure.class.as_str().to_string());
                    last_error = Some(error);
                }
                Err(error) => {
                    let duration_ms = attempt_started.elapsed().as_millis();
                    let adaptive_latency_ms = duration_ms.min(u64::MAX as u128) as u64;
                    let error_text = error.to_string();
                    let failure = normalized_gateway_failure(
                        &error,
                        &account.provider,
                        &account.id,
                        &route.model,
                        selected_transport,
                    );
                    account_admission.observe_failure(failure.class);
                    let retryable = failure.allows_silent_fallback(false);
                    if let Some((route_cooldown_secs, adaptive_failure)) =
                        route_failure_policy_for_failure(&failure)
                    {
                        let route_cooldown =
                            route_cooldown_for_failure(&failure, route_cooldown_secs);
                        self.router
                            .mark_failure(
                                &route.id,
                                error_text.clone(),
                                route_cooldown,
                                adaptive_latency_ms,
                                adaptive_failure,
                            )
                            .await;
                        self.router
                            .mark_runtime_failure(
                                runtime_health_permit,
                                &failure,
                                &error_text,
                                route_cooldown_secs,
                            )
                            .await;
                    } else {
                        self.router
                            .mark_runtime_failure(runtime_health_permit, &failure, &error_text, 0)
                            .await;
                    }
                    let outcome = failure_outcome(&error, &failure);
                    self.record_execution_attempt(AttemptRecord {
                        request_id: &request_id,
                        attempt_index,
                        route_id: &route.id,
                        account_id: &account.id,
                        model: &route.model,
                        status_code: None,
                        outcome,
                        retryable,
                        duration_ms,
                        error: Some(&error_text),
                        failure_class: Some(failure.class.as_str()),
                        execution_phase: failure.phase.as_str(),
                        replay_safety: failure.replay_safety.as_str(),
                        committed: false,
                        selected_transport: Some(selected_transport),
                        recovery_reason: recovery_reason.as_deref(),
                        logical_candidate: Some(&logical_candidate),
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
                    if !retryable {
                        return Err(self.finish_execution_error(&request_id, error).await);
                    }
                    recovery_reason = Some(failure.class.as_str().to_string());
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
                barrier: StreamCommitBarrier::new(),
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
    let failure = error.execution_failure();
    let mut legacy = error;
    while let BrowserProviderError::Classified { source, .. } = legacy {
        legacy = *source;
    }
    let legacy_text = legacy.to_string();
    let source = match legacy {
        BrowserProviderError::InvalidConfig(_)
        | BrowserProviderError::UnsupportedAdapter(_)
        | BrowserProviderError::UnsupportedBrowserless(_)
        | BrowserProviderError::InvalidTransportPolicy(_)
        | BrowserProviderError::MissingBinding(_)
        | BrowserProviderError::Io(_)
        | BrowserProviderError::Toml(_) => GatewayError::InvalidConfig(legacy_text.clone()),
        BrowserProviderError::SessionUnavailable { .. } => {
            GatewayError::BrowserSessionUnavailable(legacy_text.clone())
        }
        BrowserProviderError::AdapterIncompatible {
            account_id,
            code,
            message,
        } => {
            let rendered = format!(
                "browser adapter incompatible for account '{account_id}' ({code}): {message}"
            );
            if failure.class == FailureClass::SessionStateDesync {
                GatewayError::ModelBindingConflict(rendered)
            } else {
                GatewayError::BrowserAdapterIncompatible(rendered)
            }
        }
        BrowserProviderError::ModelUnavailable { .. } => {
            GatewayError::BrowserModelUnavailable(legacy_text.clone())
        }
        BrowserProviderError::ModelRecipeStale { .. } => {
            GatewayError::BrowserModelRecipeStale(legacy_text.clone())
        }
        BrowserProviderError::Transport(_) | BrowserProviderError::TransportUnavailable { .. } => {
            GatewayError::BrowserTransport(legacy_text)
        }
        BrowserProviderError::Classified { .. } => {
            unreachable!("classified browser provider errors are unwrapped above")
        }
    };
    GatewayError::Classified {
        failure: Box::new(failure),
        source: Box::new(source),
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

fn legacy_gateway_error(mut error: &GatewayError) -> &GatewayError {
    loop {
        match error {
            GatewayError::Classified { source, .. } | GatewayError::Execution { source, .. } => {
                error = source.as_ref();
            }
            _ => return error,
        }
    }
}

fn selected_transport_label(provider: &ProviderConfig) -> &'static str {
    if BrowserProviderRegistry::is_browser_kind(&provider.kind) {
        "browser_runtime"
    } else {
        "direct_http"
    }
}

fn normalized_status_failure(
    status: StatusCode,
    _body: &str,
    provider: &str,
    account_id: &str,
    model: &str,
    transport: &str,
    browser_provider: bool,
) -> ExecutionFailure {
    let mut failure = match status.as_u16() {
        401 | 403 => ExecutionFailure::new(
            FailureClass::AuthExpired,
            true,
            ReplaySafety::Safe,
            ExecutionPhase::Submitted,
            FailureScope::Account,
            "provider authentication is not ready",
        ),
        408 => ExecutionFailure::new(
            FailureClass::NetworkTransient,
            true,
            ReplaySafety::ProbablySafe,
            ExecutionPhase::Submitted,
            FailureScope::Request,
            "upstream request timed out",
        ),
        409 => ExecutionFailure::new(
            FailureClass::SessionStateDesync,
            true,
            ReplaySafety::ProbablySafe,
            ExecutionPhase::Submitted,
            FailureScope::Conversation,
            "provider session state rejected the request",
        ),
        429 => ExecutionFailure::new(
            FailureClass::RateLimited,
            true,
            ReplaySafety::Safe,
            ExecutionPhase::Submitted,
            FailureScope::Account,
            "provider rate limit reached",
        ),
        503 => ExecutionFailure::new(
            FailureClass::UpstreamOverloaded,
            true,
            ReplaySafety::ProbablySafe,
            ExecutionPhase::Submitted,
            FailureScope::Provider,
            "provider is temporarily overloaded",
        ),
        500..=599 => ExecutionFailure::new(
            FailureClass::Upstream5xx,
            matches!(status.as_u16(), 500 | 502 | 503 | 504),
            ReplaySafety::ProbablySafe,
            ExecutionPhase::Submitted,
            FailureScope::Provider,
            "provider returned a transient server error",
        ),
        _ => ExecutionFailure::new(
            FailureClass::Unknown,
            false,
            ReplaySafety::ProbablySafe,
            ExecutionPhase::Submitted,
            FailureScope::Request,
            "upstream rejected the request",
        ),
    };

    failure = failure
        .with_context(provider, account_id, model, transport)
        .with_cooldown(cooldown_for(status));
    if browser_provider && matches!(status.as_u16(), 401 | 403) {
        failure = failure.requiring_human_action();
    }
    failure
}

fn normalized_gateway_failure(
    error: &GatewayError,
    provider: &str,
    account_id: &str,
    model: &str,
    transport: &str,
) -> ExecutionFailure {
    let failure = match error {
        GatewayError::NoRoute(_) => ExecutionFailure::new(
            FailureClass::ModelUnavailable,
            false,
            ReplaySafety::Safe,
            ExecutionPhase::PreSubmit,
            FailureScope::Model,
            "no eligible logical route is available",
        ),
        GatewayError::MissingCredential(_) => ExecutionFailure::new(
            FailureClass::AuthIncomplete,
            false,
            ReplaySafety::Safe,
            ExecutionPhase::PreSubmit,
            FailureScope::Account,
            "required credential is missing",
        ),
        GatewayError::InvalidConfig(_) | GatewayError::ClientPolicyDenied(_) => {
            ExecutionFailure::new(
                FailureClass::Unknown,
                false,
                ReplaySafety::Safe,
                ExecutionPhase::PreSubmit,
                FailureScope::Request,
                "request cannot be executed with the current configuration",
            )
        }
        GatewayError::Transport(_) => ExecutionFailure::new(
            FailureClass::NetworkTransient,
            true,
            ReplaySafety::ProbablySafe,
            ExecutionPhase::Submitted,
            FailureScope::Transport,
            "transport failed before client-visible commit",
        ),
        GatewayError::BrowserSessionUnavailable(_) => ExecutionFailure::new(
            FailureClass::SessionBusy,
            true,
            ReplaySafety::Safe,
            ExecutionPhase::PreSubmit,
            FailureScope::Session,
            "browser session is not ready",
        )
        .with_cooldown(2),
        GatewayError::BrowserTransport(_) => ExecutionFailure::new(
            FailureClass::NetworkTransient,
            true,
            ReplaySafety::ProbablySafe,
            ExecutionPhase::Submitted,
            FailureScope::Transport,
            "browser-backed transport failed before client-visible commit",
        ),
        GatewayError::BrowserAdapterIncompatible(_) => ExecutionFailure::new(
            FailureClass::ModelUnavailable,
            true,
            ReplaySafety::Safe,
            ExecutionPhase::PreSubmit,
            FailureScope::Model,
            "browser adapter is not compatible with the selected model/page",
        )
        .with_cooldown(0),
        GatewayError::ModelBindingConflict(_) => ExecutionFailure::new(
            FailureClass::SessionStateDesync,
            false,
            ReplaySafety::Unsafe,
            ExecutionPhase::Submitted,
            FailureScope::Conversation,
            "provider-native conversation is bound to incompatible model state",
        )
        .with_cooldown(0),
        GatewayError::BrowserModelUnavailable(_) => ExecutionFailure::new(
            FailureClass::ModelUnavailable,
            true,
            ReplaySafety::Safe,
            ExecutionPhase::PreSubmit,
            FailureScope::Model,
            "selected browser model is unavailable",
        )
        .with_cooldown(0),
        GatewayError::BrowserModelRecipeStale(_) => ExecutionFailure::new(
            FailureClass::ModelRecipeStale,
            false,
            ReplaySafety::Safe,
            ExecutionPhase::PreSubmit,
            FailureScope::Model,
            "selected browser model recipe is stale",
        )
        .with_cooldown(0),
        GatewayError::Upstream { status, body } => {
            return normalized_status_failure(
                *status, body, provider, account_id, model, transport, false,
            )
        }
        GatewayError::Classified { failure, .. } => {
            return failure
                .as_ref()
                .clone()
                .with_context(provider, account_id, model, transport)
        }
        GatewayError::Execution { source, .. } => {
            return normalized_gateway_failure(source, provider, account_id, model, transport)
        }
    };
    failure.with_context(provider, account_id, model, transport)
}

fn failure_is_adaptive(failure: &ExecutionFailure) -> bool {
    matches!(
        failure.class,
        FailureClass::NetworkTransient
            | FailureClass::CdpDisconnected
            | FailureClass::PageTargetLost
            | FailureClass::BrowserCrashed
            | FailureClass::StreamDropped
            | FailureClass::StreamEmpty
            | FailureClass::UpstreamOverloaded
            | FailureClass::Upstream5xx
    )
}

fn route_cooldown_for_failure(failure: &ExecutionFailure, cooldown_secs: i64) -> i64 {
    if matches!(
        failure.scope,
        FailureScope::Transport | FailureScope::Session | FailureScope::Provider
    ) {
        0
    } else {
        cooldown_secs
    }
}

fn route_failure_policy_for_failure(failure: &ExecutionFailure) -> Option<(i64, bool)> {
    if failure.class == FailureClass::SessionStateDesync && !failure.retryable {
        return None;
    }
    let route_cooldown_secs = failure
        .suggested_cooldown_secs
        .unwrap_or(match failure.class {
            FailureClass::ModelUnavailable | FailureClass::ModelRecipeStale => 0,
            FailureClass::SessionBusy => 2,
            _ => 10,
        });
    Some((route_cooldown_secs, failure_is_adaptive(failure)))
}

fn failure_outcome(error: &GatewayError, failure: &ExecutionFailure) -> &'static str {
    let error = legacy_gateway_error(error);
    match failure.class {
        FailureClass::RateLimited => "rate_limited",
        FailureClass::AuthExpired
        | FailureClass::AuthIncomplete
        | FailureClass::HumanActionRequired => "authentication_error",
        FailureClass::SessionBusy => "browser_session_unavailable",
        FailureClass::ModelRecipeStale => "model_recipe_stale",
        FailureClass::SessionStateDesync
            if matches!(error, GatewayError::ModelBindingConflict(_)) =>
        {
            "model_binding_conflict"
        }
        FailureClass::ModelUnavailable => match error {
            GatewayError::BrowserAdapterIncompatible(_) => "browser_adapter_incompatible",
            GatewayError::BrowserModelUnavailable(_) => "browser_model_unavailable",
            _ => "transport_error",
        },
        _ => match error {
            GatewayError::BrowserTransport(_) => "browser_transport_error",
            _ => "transport_error",
        },
    }
}

#[cfg(test)]
fn is_retryable_attempt_error(error: &GatewayError) -> bool {
    normalized_gateway_failure(error, "provider", "account", "model", "transport")
        .allows_silent_fallback(false)
}

#[cfg(test)]
fn route_failure_policy(error: &GatewayError) -> Option<(i64, bool)> {
    let failure = normalized_gateway_failure(error, "provider", "account", "model", "transport");
    route_failure_policy_for_failure(&failure)
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
        assert_eq!(
            normalize_execution_policy("browser-first"),
            "prefer-browser"
        );
        assert_eq!(normalize_execution_policy("api-first"), "prefer-api");
        assert_eq!(normalize_execution_policy("balanced"), "balanced");
    }
}

#[cfg(test)]
mod stream_trace_tests {
    use super::{
        is_retryable_attempt_error, legacy_gateway_error, map_browser_provider_error,
        normalized_gateway_failure, normalized_status_failure, observe_terminal_sse_completion,
        route_cooldown_for_failure, route_failure_policy, stream_error_message,
        upstream_stream_error_sse, BrowserProviderError, GatewayError,
    };
    use crate::execution::{
        ExecutionFailure, ExecutionPhase, FailureClass, FailureScope, ReplaySafety,
    };
    use reqwest::StatusCode;

    #[test]
    fn typed_status_failures_preserve_existing_retry_semantics() {
        let rate_limited = normalized_status_failure(
            StatusCode::TOO_MANY_REQUESTS,
            "slow down",
            "gemini-web",
            "account-a",
            "gemini-pro",
            "direct-http",
            true,
        );
        assert_eq!(rate_limited.class, FailureClass::RateLimited);
        assert_eq!(rate_limited.phase, ExecutionPhase::Submitted);
        assert_eq!(rate_limited.replay_safety, ReplaySafety::Safe);
        assert!(rate_limited.retryable);

        let overloaded = normalized_status_failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "temporarily unavailable",
            "gemini-web",
            "account-a",
            "gemini-pro",
            "direct-http",
            false,
        );
        assert_eq!(overloaded.class, FailureClass::UpstreamOverloaded);
        assert_eq!(overloaded.replay_safety, ReplaySafety::ProbablySafe);
        assert!(overloaded.retryable);

        let not_implemented = normalized_status_failure(
            StatusCode::NOT_IMPLEMENTED,
            "unsupported",
            "api-provider",
            "account-a",
            "model-a",
            "direct-http",
            false,
        );
        assert_eq!(not_implemented.class, FailureClass::Upstream5xx);
        assert!(!not_implemented.retryable);
    }

    #[test]
    fn browser_provider_errors_are_normalized_at_the_provider_boundary() {
        let waf_error = map_browser_provider_error(BrowserProviderError::AdapterIncompatible {
            account_id: "account-a".into(),
            code: "upstream_waf_rejected".into(),
            message: "classification=waf body=aliyun_waf_aa".into(),
        });
        assert!(matches!(
            legacy_gateway_error(&waf_error),
            GatewayError::BrowserAdapterIncompatible(_)
        ));
        let waf = normalized_gateway_failure(
            &waf_error,
            "qwen-web",
            "account-a",
            "qwen-max",
            "browser_runtime",
        );
        assert_eq!(waf.class, FailureClass::WafRejected);
        assert_eq!(waf.scope, FailureScope::Transport);
        assert_eq!(waf.replay_safety, ReplaySafety::Safe);
        assert!(waf.allows_silent_fallback(false));

        let cdp_error = map_browser_provider_error(BrowserProviderError::Transport(
            "WebSocket protocol error: Connection reset without closing handshake".into(),
        ));
        let cdp = normalized_gateway_failure(
            &cdp_error,
            "qwen-web",
            "account-a",
            "qwen-max",
            "browser_runtime",
        );
        assert_eq!(cdp.class, FailureClass::CdpDisconnected);
        assert_eq!(cdp.scope, FailureScope::Session);

        let empty_error = map_browser_provider_error(BrowserProviderError::Transport(
            "browser stream completed without assistant output".into(),
        ));
        let empty_stream = normalized_gateway_failure(
            &empty_error,
            "deepseek-web",
            "account-a",
            "deepseek-chat",
            "browser_runtime",
        );
        assert_eq!(empty_stream.class, FailureClass::StreamEmpty);
        assert_eq!(empty_stream.scope, FailureScope::Conversation);

        let raw_legacy = normalized_gateway_failure(
            &GatewayError::BrowserTransport(
                "upstream_waf_rejected: WebSocket protocol error: empty stream".into(),
            ),
            "legacy-browser",
            "account-a",
            "model-a",
            "browser_runtime",
        );
        assert_eq!(raw_legacy.class, FailureClass::NetworkTransient);
    }

    #[test]
    fn browser_auth_failure_records_human_action_without_leaking_credentials() {
        let failure = normalized_status_failure(
            StatusCode::UNAUTHORIZED,
            "authentication required",
            "gemini-web",
            "account-a",
            "gemini-pro",
            "browser_runtime",
            true,
        );
        assert_eq!(failure.class, FailureClass::AuthExpired);
        assert!(failure.human_action_required);
        assert!(!failure.diagnostic.contains("cookie"));
        assert!(!failure.diagnostic.contains("token"));
    }

    #[test]
    fn stale_model_recipe_and_binding_conflict_are_not_retryable() {
        let stale = map_browser_provider_error(BrowserProviderError::ModelRecipeStale {
            account_id: "account-a".into(),
            model: "gemini-pro".into(),
        });
        assert!(!is_retryable_attempt_error(&stale));

        let conflict = map_browser_provider_error(BrowserProviderError::AdapterIncompatible {
            account_id: "account-a".into(),
            code: "model_binding_conflict".into(),
            message: "native conversation is already bound to model 'pro'".into(),
        });
        assert!(!is_retryable_attempt_error(&conflict));

        assert!(is_retryable_attempt_error(&GatewayError::BrowserTransport(
            "network".into()
        )));
    }

    #[test]
    fn runtime_scope_preserves_account_cooldown_but_isolates_transport_cooldown() {
        let account_failure = ExecutionFailure::new(
            FailureClass::RateLimited,
            true,
            ReplaySafety::Safe,
            ExecutionPhase::Submitted,
            FailureScope::Account,
            "rate limited",
        );
        assert_eq!(route_cooldown_for_failure(&account_failure, 60), 60);

        let transport_failure = ExecutionFailure::new(
            FailureClass::WafRejected,
            true,
            ReplaySafety::Safe,
            ExecutionPhase::Submitted,
            FailureScope::Transport,
            "transport rejected",
        );
        assert_eq!(route_cooldown_for_failure(&transport_failure, 60), 0);
    }

    #[test]
    fn model_binding_conflict_does_not_mutate_route_health() {
        for provider in ["Gemini", "Qwen"] {
            let conflict = map_browser_provider_error(BrowserProviderError::AdapterIncompatible {
                account_id: "account-a".into(),
                code: "model_binding_conflict".into(),
                message: format!("{provider} native conversation is already bound to model 'pro'"),
            });
            assert!(!is_retryable_attempt_error(&conflict));
            assert_eq!(route_failure_policy(&conflict), None);
        }

        let stable_conflict = GatewayError::ModelBindingConflict(
            "model_binding_conflict: MiMo conversation is bound to mimo-v2.5-pro".into(),
        );
        assert!(!is_retryable_attempt_error(&stable_conflict));
        assert_eq!(route_failure_policy(&stable_conflict), None);

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
