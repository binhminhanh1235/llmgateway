use crate::{
    catalog::{CatalogError, ModelCatalog},
    client_policy::{ClientAccess, ClientPolicyError, ClientPolicyStore},
    compat::{anthropic, responses},
    conversation::{ConversationError, ConversationStore},
    gateway::{Gateway, GatewayError},
    quota_usage_runtime,
    response_state::{response_to_openai_assistant, responses_stream_with_capture},
};
use axum::{
    body::{to_bytes, Body},
    extract::{rejection::JsonRejection, Path, Request, State},
    http::{
        header::{AUTHORIZATION, CONTENT_TYPE},
        HeaderMap, HeaderValue, Response, StatusCode,
    },
    middleware::Next,
    response::IntoResponse,
    Json,
};
use futures_util::TryStreamExt;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::oneshot;

#[derive(Clone)]
pub struct AppState {
    pub gateway: Arc<Gateway>,
    pub catalog: Arc<ModelCatalog>,
    pub conversations: Arc<ConversationStore>,
    pub gateway_api_key: Arc<String>,
    pub client_policies: Arc<ClientPolicyStore>,
}

pub async fn openai_chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };
    let config = state.gateway.config_snapshot();
    let requested_model = if config.api.strict_openai_compatibility {
        match body
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
        {
            Some(model) => model.to_string(),
            None => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "'model' is required when api.strict_openai_compatibility is enabled",
                )
            }
        }
    } else {
        body.get("model")
            .and_then(Value::as_str)
            .unwrap_or(&config.api.default_model)
            .to_string()
    };
    let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let claude = ClaudeCodeRequestContext::from_headers(&headers);
    let claude_agentic_tool_loop = claude.session_id.is_some()
        && openai_body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty())
        && openai_body.get("tool_choice").and_then(Value::as_str) == Some("auto");
    if claude_agentic_tool_loop {
        if let Some(object) = openai_body.as_object_mut() {
            object.insert("llmgateway_agentic_tool_loop".into(), Value::Bool(true));
        }
    }
    if let Err(error) = state
        .client_policies
        .enforce_model(&access, &requested_model)
    {
        return client_policy_error(error);
    }
    let reservation = match state
        .client_policies
        .reserve_request(&access, &mut body)
        .await
    {
        Ok(reservation) => reservation,
        Err(error) => return client_policy_error(error),
    };

    match state
        .gateway
        .execute_openai_chat_for_client(&requested_model, &body, access.policy())
        .await
    {
        Ok(routed) => {
            let route_id = routed.route.id.clone();
            let request_id = routed.request_id.clone();
            if is_stream {
                let response = state.gateway.trace_stream_response(
                    routed.response,
                    request_id.clone(),
                    route_id.clone(),
                    routed.started_at,
                );
                let stream = response.bytes_stream().map_err(std::io::Error::other);
                response_with_route_and_request(
                    StatusCode::OK,
                    "text/event-stream",
                    Body::from_stream(stream),
                    &route_id,
                    &request_id,
                )
            } else {
                match routed.response.bytes().await {
                    Ok(bytes) => {
                        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                            if let Err(error) = state
                                .client_policies
                                .reconcile_usage(reservation.as_ref(), &value)
                                .await
                            {
                                return client_policy_error(error);
                            }
                        }
                        response_with_route_and_request(
                            StatusCode::OK,
                            "application/json",
                            Body::from(bytes),
                            &route_id,
                            &request_id,
                        )
                    }
                    Err(error) => gateway_error(GatewayError::Execution {
                        request_id,
                        source: Box::new(GatewayError::Transport(error.to_string())),
                    }),
                }
            }
        }
        Err(error) => gateway_error(error),
    }
}

pub async fn openai_responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };

    let previous_response_id = body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut normalized_body = body.clone();
    if let Some(object) = normalized_body.as_object_mut() {
        object.remove("previous_response_id");
    }

    let (requested_model, mut openai_body) = match responses::to_openai_request(&normalized_body) {
        Ok(value) => value,
        Err(message) => {
            return json_error(StatusCode::BAD_REQUEST, "invalid_request_error", &message)
        }
    };
    let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if let Err(error) = state
        .client_policies
        .enforce_model(&access, &requested_model)
    {
        return client_policy_error(error);
    }
    let response_owner = access.client_id().map(str::to_string);
    let mut preferred_route = None;

    if let Some(previous_response_id) = previous_response_id {
        let previous = match state
            .conversations
            .response_context(&previous_response_id)
            .await
        {
            Ok(previous) => previous,
            Err(ConversationError::ResponseNotFound(_)) => {
                return json_error(
                    StatusCode::NOT_FOUND,
                    "invalid_request_error",
                    &format!("previous_response_id '{previous_response_id}' was not found"),
                )
            }
            Err(error) => return conversation_state_error(error),
        };
        if response_owner.is_some() && previous.client_id.as_deref() != response_owner.as_deref() {
            return json_error(
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                &format!("previous_response_id '{previous_response_id}' was not found"),
            );
        }
        preferred_route = previous.route_id.clone();
        let current = request_messages(&openai_body);
        let mut merged = previous.messages;
        merged.extend(current);
        set_request_messages(&mut openai_body, merged);
    }

    let history_before_response = request_messages(&openai_body);
    let reservation = match state
        .client_policies
        .reserve_request(&access, &mut openai_body)
        .await
    {
        Ok(reservation) => reservation,
        Err(error) => return client_policy_error(error),
    };
    match state
        .gateway
        .execute_openai_chat_with_affinity_for_client(
            &requested_model,
            &openai_body,
            preferred_route.as_deref(),
            access.policy(),
        )
        .await
    {
        Ok(routed) => {
            let route_id = routed.route.id.clone();
            let request_id = routed.request_id.clone();
            if is_stream {
                let response = state.gateway.trace_stream_response(
                    routed.response,
                    request_id.clone(),
                    route_id.clone(),
                    routed.started_at,
                );
                let stream =
                    responses::openai_stream_to_responses(response, requested_model.clone());
                let (tx, rx) = oneshot::channel();
                let stream = responses_stream_with_capture(stream, tx);
                let conversations = state.conversations.clone();
                let client_policies = state.client_policies.clone();
                let model_for_task = requested_model.clone();
                let route_for_task = route_id.clone();
                let history_for_task = history_before_response.clone();
                let owner_for_task = response_owner.clone();
                let reservation_for_task = reservation.clone();
                tokio::spawn(async move {
                    if let Ok(response) = rx.await {
                        let _ = client_policies
                            .reconcile_usage(reservation_for_task.as_ref(), &response)
                            .await;
                        let mut history = history_for_task;
                        if let Some(assistant) = response_to_openai_assistant(&response) {
                            history.push(assistant);
                        }
                        if let Some(response_id) = response.get("id").and_then(Value::as_str) {
                            let _ = conversations
                                .save_response_context(
                                    response_id,
                                    &model_for_task,
                                    &history,
                                    Some(&route_for_task),
                                    owner_for_task.as_deref(),
                                )
                                .await;
                        }
                    }
                });
                response_with_route_and_request(
                    StatusCode::OK,
                    "text/event-stream",
                    Body::from_stream(stream),
                    &route_id,
                    &request_id,
                )
            } else {
                let usage_event_id = routed.usage_event_id.clone();
                match routed.response.json::<Value>().await {
                    Ok(openai) => {
                        update_provider_usage(usage_event_id.as_deref(), &openai).await;
                        if let Err(error) = state
                            .client_policies
                            .reconcile_usage(reservation.as_ref(), &openai)
                            .await
                        {
                            return client_policy_error(error);
                        }
                        let response = responses::from_openai_response(&openai, &requested_model);
                        let mut history = history_before_response;
                        if let Some(assistant) = openai_assistant_message(&openai) {
                            history.push(assistant);
                        }
                        let Some(response_id) = response.get("id").and_then(Value::as_str) else {
                            return json_error(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "response_state_error",
                                "generated Responses object is missing id",
                            );
                        };
                        if let Err(error) = state
                            .conversations
                            .save_response_context(
                                response_id,
                                &requested_model,
                                &history,
                                Some(&route_id),
                                response_owner.as_deref(),
                            )
                            .await
                        {
                            return conversation_state_error(error);
                        }
                        json_response_with_request(
                            StatusCode::OK,
                            response,
                            Some(&route_id),
                            &request_id,
                        )
                    }
                    Err(error) => gateway_error(GatewayError::Execution {
                        request_id,
                        source: Box::new(GatewayError::Transport(error.to_string())),
                    }),
                }
            }
        }
        Err(error) => gateway_error(error),
    }
}

pub const ANTHROPIC_MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ClaudeCodeRequestContext {
    session_id: Option<String>,
    agent_id: Option<String>,
    parent_agent_id: Option<String>,
}

impl ClaudeCodeRequestContext {
    fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            session_id: trimmed_header(headers, "x-claude-code-session-id"),
            agent_id: trimmed_header(headers, "x-claude-code-agent-id"),
            parent_agent_id: trimmed_header(headers, "x-claude-code-parent-agent-id"),
        }
    }

    fn thread_id(&self) -> Option<String> {
        self.session_id
            .as_deref()
            .map(|session_id| format!("claude-code:{session_id}"))
    }
}

pub async fn anthropic_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response<Body> {
    let boundary_request_id = fresh_request_id();
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return anthropic_json_rejection(rejection, &boundary_request_id),
    };

    let access = match authorize_anthropic_client(&headers, &state, &boundary_request_id) {
        Ok(access) => access,
        Err(response) => return response,
    };
    let anthropic_version = headers
        .get("anthropic-version")
        .and_then(|value| value.to_str().ok());
    let anthropic_beta_values = headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let (requested_model, mut openai_body) = match anthropic::to_openai_request_with_protocol(
        &body,
        anthropic_version,
        &anthropic_beta_values,
    ) {
        Ok(value) => value,
        Err(message) => {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &message,
                &boundary_request_id,
            )
        }
    };
    let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if let Err(error) = state
        .client_policies
        .enforce_model(&access, &requested_model)
    {
        return anthropic_client_policy_error(error, &boundary_request_id);
    }
    let reservation = match state
        .client_policies
        .reserve_request(&access, &mut openai_body)
        .await
    {
        Ok(reservation) => reservation,
        Err(error) => return anthropic_client_policy_error(error, &boundary_request_id),
    };

    if claude.session_id.is_some() || claude.agent_id.is_some() || claude.parent_agent_id.is_some() {
        tracing::debug!(
            request_id = %boundary_request_id,
            claude_code_session_id = claude.session_id.as_deref().unwrap_or(""),
            claude_code_agent_id = claude.agent_id.as_deref().unwrap_or(""),
            claude_code_parent_agent_id = claude.parent_agent_id.as_deref().unwrap_or(""),
            "received Claude Code Anthropic request"
        );
    }

    let compatibility_thread = if let Some(thread_id) = claude.thread_id() {
        match state
            .conversations
            .ensure_compatibility_thread(&thread_id, "Claude Code session", &requested_model)
            .await
        {
            Ok(context) => Some((thread_id, context.sticky_route)),
            Err(error) => {
                return anthropic_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    &error.to_string(),
                    &boundary_request_id,
                )
            }
        }
    } else {
        None
    };

    let routed = if let Some((thread_id, preferred_route)) = compatibility_thread.as_ref() {
        state
            .gateway
            .execute_openai_chat_with_thread_affinity_for_client(
                &requested_model,
                &openai_body,
                preferred_route.as_deref(),
                thread_id,
                access.policy(),
            )
            .await
    } else {
        state
            .gateway
            .execute_openai_chat_for_client(&requested_model, &openai_body, access.policy())
            .await
    };

    match routed {
        Ok(routed) => {
            let route_id = routed.route.id.clone();
            let request_id = routed.request_id.clone();

            if let Some((thread_id, _)) = compatibility_thread.as_ref() {
                if let Err(error) = state
                    .conversations
                    .update_thread_route_and_model(thread_id, &route_id, &requested_model)
                    .await
                {
                    return anthropic_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "api_error",
                        &error.to_string(),
                        &request_id,
                    );
                }
            }

            if is_stream {
                let response = state.gateway.trace_stream_response(
                    routed.response,
                    request_id.clone(),
                    route_id.clone(),
                    routed.started_at,
                );
                let stream = anthropic::openai_stream_to_anthropic(
                    response,
                    requested_model,
                    request_id.clone(),
                );
                response_with_route_and_request(
                    StatusCode::OK,
                    "text/event-stream",
                    Body::from_stream(stream),
                    &route_id,
                    &request_id,
                )
            } else {
                let usage_event_id = routed.usage_event_id.clone();
                match routed.response.json::<Value>().await {
                    Ok(openai) => {
                        update_provider_usage(usage_event_id.as_deref(), &openai).await;
                        if let Err(error) = state
                            .client_policies
                            .reconcile_usage(reservation.as_ref(), &openai)
                            .await
                        {
                            return anthropic_client_policy_error(error, &request_id);
                        }
                        let anthropic = anthropic::from_openai_response(&openai, &requested_model);
                        json_response_with_request(
                            StatusCode::OK,
                            anthropic,
                            Some(&route_id),
                            &request_id,
                        )
                    }
                    Err(error) => anthropic_gateway_error(GatewayError::Execution {
                        request_id,
                        source: Box::new(GatewayError::Transport(error.to_string())),
                    }),
                }
            }
        }
        Err(error) => anthropic_gateway_error(error),
    }
}


pub async fn models(State(state): State<AppState>, headers: HeaderMap) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };

    let config = state.gateway.config_snapshot();
    let physical = match state.catalog.selectable_models().await {
        Ok(models) => models,
        Err(error) => return catalog_error(error),
    };
    let mut data: BTreeMap<String, Value> = BTreeMap::new();

    let route_is_valid = |route: &crate::config::RouteConfig| -> bool {
        if !route.enabled {
            return false;
        }
        let Some(account) = config.account(&route.account) else {
            return false;
        };
        if !account.enabled {
            return false;
        }
        physical.iter().any(|model| {
            model.provider == account.provider
                && (model.external_id == route.model || model.id == route.model)
                && model.accounts.iter().any(|binding| {
                    binding.account_id == route.account
                        && binding.enabled
                        && matches!(binding.availability.as_str(), "available" | "unknown")
                })
        })
    };

    for (id, group) in &config.virtual_models {
        if !group.enabled {
            continue;
        }
        if access
            .policy()
            .is_some_and(|policy| !policy.model_allowed(id, id))
        {
            continue;
        }
        data.insert(
            id.clone(),
            json!({
                "id":id,
                "object":"model",
                "name":id,
                "owned_by":"llmgateway",
                "llmgateway":{"kind":"virtual"}
            }),
        );
    }

    for model in &physical {
        if access
            .policy()
            .is_some_and(|policy| !policy.model_allowed(&model.id, &model.id))
        {
            continue;
        }
        let available_accounts = model
            .accounts
            .iter()
            .filter(|account| {
                account.enabled && matches!(account.availability.as_str(), "available" | "unknown")
            })
            .count();
        data.insert(
            model.id.clone(),
            json!({
                "id":model.id,
                "object":"model",
                "name":model.display_name,
                "owned_by":model.owned_by,
                "llmgateway":{
                    "kind":"physical",
                    "provider":model.provider,
                    "display_name":model.display_name,
                    "context_window":model.context_window,
                    "capabilities":model.capabilities,
                    "available_accounts":available_accounts
                }
            }),
        );
    }

    // Preserve v0.1 route IDs as selectable aliases for existing clients only if the route is valid and active.
    for route in config.routes.iter().filter(|route| route_is_valid(route)) {
        if access.policy().is_some_and(|policy| {
            !policy.route_allowed(&route.id) || !policy.model_allowed(&route.id, &route.model)
        }) {
            continue;
        }
        data.entry(route.id.clone()).or_insert_with(|| {
            json!({
                "id":route.id,
                "object":"model",
                "name":route.id,
                "owned_by":"llmgateway-route",
                "llmgateway":{"kind":"route","upstream_model":route.model,"account":route.account}
            })
        });
    }

    json_response(
        StatusCode::OK,
        json!({"object":"list","data":data.into_values().collect::<Vec<_>>() }),
        None,
    )
}

pub async fn admin_models(State(state): State<AppState>, headers: HeaderMap) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    match state.catalog.models().await {
        Ok(models) => json_response(StatusCode::OK, json!({"data":models}), None),
        Err(error) => catalog_error(error),
    }
}

pub async fn admin_accounts(State(state): State<AppState>, headers: HeaderMap) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    match state.catalog.accounts().await {
        Ok(accounts) => json_response(StatusCode::OK, json!({"data":accounts}), None),
        Err(error) => catalog_error(error),
    }
}

pub async fn admin_account_models(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    match state.catalog.account_models(&account_id).await {
        Ok(models) => json_response(
            StatusCode::OK,
            json!({"account_id":account_id,"data":models}),
            None,
        ),
        Err(error) => catalog_error(error),
    }
}

pub async fn admin_refresh_account_models(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    match state.catalog.refresh_account(&account_id).await {
        Ok(result) => json_response(StatusCode::OK, json!(result), None),
        Err(error) => catalog_error(error),
    }
}

pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.gateway.config_snapshot();
    let routes = state.gateway.router.snapshot().await;
    let catalog_models = state
        .catalog
        .models()
        .await
        .map(|models| models.len())
        .unwrap_or(0);
    let threads = state
        .conversations
        .list_threads()
        .await
        .map(|threads| threads.len())
        .unwrap_or(0);
    Json(json!({
        "status":"ok",
        "service":"llmgateway",
        "default_model":config.api.default_model,
        "catalog_models":catalog_models,
        "threads":threads,
        "routes":routes
    }))
}

async fn update_provider_usage(event_id: Option<&str>, response: &Value) {
    let (Some(event_id), Some(usage)) = (event_id, quota_usage_runtime::get()) else {
        return;
    };
    if let Err(error) = usage.update_provider_usage(event_id, response).await {
        tracing::warn!(%error, event_id, "failed to update provider usage");
    }
}

pub(crate) async fn normalize_json_rejections(request: Request, next: Next) -> Response<Body> {
    let request_is_json = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("application/json"));

    let response = next.run(request).await;
    if !request_is_json
        || !matches!(
            response.status(),
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
        )
    {
        return response;
    }

    let is_plain_text = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/plain"));
    if !is_plain_text {
        return response;
    }

    let status = response.status();
    let (_, body) = response.into_parts();
    let raw = match to_bytes(body, 1024 * 1024).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).trim().to_string(),
        Err(error) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "request_body_error",
                &format!("failed to read rejected request body: {error}"),
            )
        }
    };
    let message = normalize_json_rejection_message(&raw);
    json_error(status, "invalid_request_error", &message)
}

fn normalize_json_rejection_message(raw: &str) -> String {
    if let Some(detail) = raw.strip_prefix("Failed to parse the request body as JSON: ") {
        return format!("Failed to parse request JSON: {detail}");
    }
    if let Some(detail) =
        raw.strip_prefix("Failed to deserialize the JSON body into the target type: ")
    {
        return format!("Invalid request JSON: {detail}");
    }
    format!("Invalid request JSON: {raw}")
}

fn trimmed_header(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn fresh_request_id() -> String {
    format!("req_{}", Uuid::new_v4().simple())
}

fn authorize_anthropic_client(
    headers: &HeaderMap,
    state: &AppState,
    request_id: &str,
) -> Result<ClientAccess, Response<Body>> {
    let Some(presented) = presented_api_key(headers) else {
        return Err(anthropic_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing llmgateway API key",
            request_id,
        ));
    };
    state
        .client_policies
        .authenticate(presented)
        .map_err(|error| anthropic_client_policy_error(error, request_id))
}

fn anthropic_json_rejection(rejection: JsonRejection, request_id: &str) -> Response<Body> {
    let status = rejection.status();
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        return anthropic_error(
            status,
            "request_too_large",
            "request body exceeds the 32 MB Anthropic Messages limit",
            request_id,
        );
    }
    let message = normalize_json_rejection_message(&rejection.body_text());
    anthropic_error(
        status,
        "invalid_request_error",
        &message,
        request_id,
    )
}

fn anthropic_client_policy_error(
    error: ClientPolicyError,
    request_id: &str,
) -> Response<Body> {
    match error {
        ClientPolicyError::Unauthorized => anthropic_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid llmgateway API key",
            request_id,
        ),
        ClientPolicyError::Forbidden(message) => anthropic_error(
            StatusCode::FORBIDDEN,
            "permission_error",
            &message,
            request_id,
        ),
        ClientPolicyError::BudgetExceeded(message) => anthropic_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            &message,
            request_id,
        ),
        ClientPolicyError::MissingEnv(message) => anthropic_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            &format!(
                "configured client credential environment variable '{message}' is unavailable"
            ),
            request_id,
        ),
        ClientPolicyError::Database(error) => anthropic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            &error.to_string(),
            request_id,
        ),
        ClientPolicyError::Io(error) => anthropic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            &error.to_string(),
            request_id,
        ),
    }
}

fn anthropic_gateway_error(error: GatewayError) -> Response<Body> {
    let mut request_id = None;
    let mut current = error;
    loop {
        current = match current {
            GatewayError::Execution {
                request_id: id,
                source,
            } => {
                request_id = Some(id);
                *source
            }
            GatewayError::Classified { source, .. } => *source,
            GatewayError::NoRoute(message) => {
                return anthropic_error_with_optional_request(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &message,
                    request_id,
                )
            }
            GatewayError::MissingCredential(message) => {
                return anthropic_error_with_optional_request(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "api_error",
                    &message,
                    request_id,
                )
            }
            GatewayError::InvalidConfig(message) => {
                return anthropic_error_with_optional_request(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    &message,
                    request_id,
                )
            }
            GatewayError::ClientPolicyDenied(message) => {
                return anthropic_error_with_optional_request(
                    StatusCode::FORBIDDEN,
                    "permission_error",
                    &message,
                    request_id,
                )
            }
            GatewayError::Transport(message) => {
                return anthropic_error_with_optional_request(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    &message,
                    request_id,
                )
            }
            GatewayError::BrowserSessionUnavailable(message) => {
                return anthropic_error_with_optional_request(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "api_error",
                    &message,
                    request_id,
                )
            }
            GatewayError::BrowserTransport(message) => {
                return anthropic_error_with_optional_request(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    &message,
                    request_id,
                )
            }
            GatewayError::BrowserAdapterIncompatible(message) => {
                let message = capability_rejected_message(&message);
                return anthropic_error_with_optional_request(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    &message,
                    request_id,
                );
            }
            GatewayError::ModelBindingConflict(message) => {
                return anthropic_error_with_optional_request(
                    StatusCode::CONFLICT,
                    "invalid_request_error",
                    &message,
                    request_id,
                )
            }
            GatewayError::BrowserModelUnavailable(message) => {
                let message = capability_rejected_message(&message);
                return anthropic_error_with_optional_request(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    &message,
                    request_id,
                );
            }
            GatewayError::BrowserModelRecipeStale(message) => {
                return anthropic_error_with_optional_request(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    &message,
                    request_id,
                )
            }
            GatewayError::Upstream { status, body } => {
                return anthropic_error_with_optional_request(
                    status,
                    anthropic_error_type_for_status(status),
                    &body,
                    request_id,
                )
            }
        };
    }
}

fn anthropic_error_type_for_status(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 409 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

fn capability_rejected_message(message: &str) -> String {
    if message.contains("capability_rejected:") {
        message.to_string()
    } else {
        format!("capability_rejected: {message}")
    }
}

fn anthropic_error_with_optional_request(
    status: StatusCode,
    kind: &str,
    message: &str,
    request_id: Option<String>,
) -> Response<Body> {
    let request_id = request_id.unwrap_or_else(fresh_request_id);
    anthropic_error(status, kind, message, &request_id)
}

fn anthropic_error(
    status: StatusCode,
    kind: &str,
    message: &str,
    request_id: &str,
) -> Response<Body> {
    response_with_route_and_request(
        status,
        "application/json",
        Body::from(
            json!({
                "type":"error",
                "error":{"type":kind,"message":message},
                "request_id":request_id
            })
            .to_string(),
        ),
        "",
        request_id,
    )
}


pub(crate) fn authorize(headers: &HeaderMap, expected: &str) -> Result<(), Response<Body>> {
    if presented_api_key(headers) == Some(expected) {
        Ok(())
    } else {
        Err(json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid llmgateway API key",
        ))
    }
}

pub(crate) fn authorize_client(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<ClientAccess, Response<Body>> {
    let Some(presented) = presented_api_key(headers) else {
        return Err(json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing llmgateway API key",
        ));
    };
    state
        .client_policies
        .authenticate(presented)
        .map_err(client_policy_error)
}

fn presented_api_key(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
        })
}

pub(crate) fn client_policy_error(error: ClientPolicyError) -> Response<Body> {
    match error {
        ClientPolicyError::Unauthorized => json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid llmgateway API key",
        ),
        ClientPolicyError::Forbidden(message) => {
            json_error(StatusCode::FORBIDDEN, "client_policy_error", &message)
        }
        ClientPolicyError::BudgetExceeded(message) => json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "client_budget_exceeded",
            &message,
        ),
        ClientPolicyError::MissingEnv(message) => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "client_policy_configuration_error",
            &format!(
                "configured client credential environment variable '{message}' is unavailable"
            ),
        ),
        ClientPolicyError::Database(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "client_policy_database_error",
            &error.to_string(),
        ),
        ClientPolicyError::Io(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "client_policy_storage_error",
            &error.to_string(),
        ),
    }
}

pub(crate) fn gateway_error(error: GatewayError) -> Response<Body> {
    match error {
        GatewayError::NoRoute(message) => {
            json_error(StatusCode::BAD_REQUEST, "model_error", &message)
        }
        GatewayError::MissingCredential(message) => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "credential_error",
            &message,
        ),
        GatewayError::InvalidConfig(message) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "configuration_error",
            &message,
        ),
        GatewayError::ClientPolicyDenied(message) => {
            json_error(StatusCode::FORBIDDEN, "client_policy_error", &message)
        }
        GatewayError::Transport(message) => {
            json_error(StatusCode::BAD_GATEWAY, "upstream_error", &message)
        }
        GatewayError::BrowserSessionUnavailable(message) => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "browser_session_error",
            &message,
        ),
        GatewayError::BrowserTransport(message) => {
            json_error(StatusCode::BAD_GATEWAY, "browser_transport_error", &message)
        }
        GatewayError::BrowserAdapterIncompatible(message) => json_error(
            StatusCode::BAD_GATEWAY,
            "browser_adapter_incompatible",
            &message,
        ),
        GatewayError::ModelBindingConflict(message) => {
            json_error(StatusCode::CONFLICT, "model_binding_conflict", &message)
        }
        GatewayError::BrowserModelUnavailable(message) => json_error(
            StatusCode::BAD_GATEWAY,
            "browser_model_unavailable",
            &message,
        ),
        GatewayError::BrowserModelRecipeStale(message) => {
            json_error(StatusCode::BAD_GATEWAY, "model_recipe_stale", &message)
        }
        GatewayError::Upstream { status, body } => json_error(status, "upstream_error", &body),
        GatewayError::Classified { source, .. } => gateway_error(*source),
        GatewayError::Execution { request_id, source } => {
            let mut response = gateway_error(*source);
            insert_request_id(&mut response, &request_id);
            response
        }
    }
}

fn catalog_error(error: CatalogError) -> Response<Body> {
    match error {
        CatalogError::AccountNotFound(account_id) => json_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            &format!("unknown account '{account_id}'"),
        ),
        CatalogError::InvalidConfig(message) => {
            json_error(StatusCode::BAD_REQUEST, "catalog_error", &message)
        }
        CatalogError::MissingCredential(message) => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "credential_error",
            &message,
        ),
        CatalogError::Transport(message) => {
            json_error(StatusCode::BAD_GATEWAY, "discovery_error", &message)
        }
        CatalogError::Upstream { status, body } => json_error(status, "discovery_error", &body),
        CatalogError::InvalidResponse(message) => {
            json_error(StatusCode::BAD_GATEWAY, "discovery_error", &message)
        }
        CatalogError::Database(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_database_error",
            &error.to_string(),
        ),
        CatalogError::Io(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_storage_error",
            &error.to_string(),
        ),
    }
}

fn conversation_state_error(error: ConversationError) -> Response<Body> {
    match error {
        ConversationError::ThreadNotFound(message)
        | ConversationError::ResponseNotFound(message) => {
            json_error(StatusCode::NOT_FOUND, "response_state_error", &message)
        }
        ConversationError::InvalidJson(message) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "response_state_error",
            &message,
        ),
        ConversationError::Database(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "response_state_database_error",
            &error.to_string(),
        ),
        ConversationError::Io(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "response_state_storage_error",
            &error.to_string(),
        ),
    }
}

pub(crate) fn json_error(status: StatusCode, kind: &str, message: &str) -> Response<Body> {
    json_response(
        status,
        json!({"error":{"type":kind,"message":message}}),
        None,
    )
}

pub(crate) fn json_response(
    status: StatusCode,
    value: Value,
    route: Option<&str>,
) -> Response<Body> {
    response_with_route(
        status,
        "application/json",
        Body::from(value.to_string()),
        route.unwrap_or(""),
    )
}

fn json_response_with_request(
    status: StatusCode,
    value: Value,
    route: Option<&str>,
    request_id: &str,
) -> Response<Body> {
    response_with_route_and_request(
        status,
        "application/json",
        Body::from(value.to_string()),
        route.unwrap_or(""),
        request_id,
    )
}

pub(crate) fn response_with_route(
    status: StatusCode,
    content_type: &str,
    body: Body,
    route: &str,
) -> Response<Body> {
    response_with_route_and_request(status, content_type, body, route, "")
}

fn response_with_route_and_request(
    status: StatusCode,
    content_type: &str,
    body: Body,
    route: &str,
    request_id: &str,
) -> Response<Body> {
    let mut response = Response::builder()
        .status(status)
        .body(body)
        .expect("valid response");
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(content_type).expect("valid content type"),
    );
    if !route.is_empty() {
        if let Ok(value) = HeaderValue::from_str(route) {
            response.headers_mut().insert("x-llmgateway-route", value);
        }
    }
    insert_request_id(&mut response, request_id);
    response
}

fn insert_request_id(response: &mut Response<Body>, request_id: &str) {
    if !request_id.is_empty() {
        if let Ok(value) = HeaderValue::from_str(request_id) {
            response
                .headers_mut()
                .insert("x-llmgateway-request-id", value.clone());
            response.headers_mut().insert("request-id", value);
        }
    }
}

fn request_messages(body: &Value) -> Vec<Value> {
    body.get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn set_request_messages(body: &mut Value, messages: Vec<Value>) {
    if let Some(object) = body.as_object_mut() {
        object.insert("messages".into(), Value::Array(messages));
    }
}

fn openai_assistant_message(openai: &Value) -> Option<Value> {
    openai
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::AppConfig,
        execution::{ExecutionFailure, ExecutionPhase, FailureClass, FailureScope, ReplaySafety},
        execution_trace::ExecutionTraceStore,
        live_config::LiveConfig,
    };
    use axum::body::to_bytes;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::{collections::HashSet, fs};
    use uuid::Uuid;
    use tower::ServiceExt;

    #[test]
    fn classified_model_binding_conflict_preserves_http_409() {
        let failure = ExecutionFailure::new(
            FailureClass::SessionStateDesync,
            false,
            ReplaySafety::Unsafe,
            ExecutionPhase::Submitted,
            FailureScope::Conversation,
            "provider-native conversation is bound to incompatible model state",
        );
        let response = gateway_error(GatewayError::Classified {
            failure: Box::new(failure),
            source: Box::new(GatewayError::ModelBindingConflict(
                "model_binding_conflict".into(),
            )),
        });
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn models_exposes_only_enabled_models_active_routes_and_viable_groups() {
        let temp_db = format!("/tmp/llmgateway-test-models-{}.db", Uuid::new_v4());
        let db_url = format!("sqlite://{}", temp_db);

        let config_toml = format!(
            r#"
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "group-viable"

[storage]
database_url = "{db_url}"

[[providers]]
id = "p1"
kind = "openai-compatible"
base_url = "https://api.p1.com"

[[accounts]]
id = "acc-enabled"
provider = "p1"
api_key_env = "API_KEY"
enabled = true

[[accounts]]
id = "acc-disabled"
provider = "p1"
api_key_env = "API_KEY"
enabled = false

[[routes]]
id = "route-active"
account = "acc-enabled"
model = "model-enabled"
enabled = true

[[routes]]
id = "route-for-disabled-acc"
account = "acc-disabled"
model = "model-enabled"
enabled = true

[[routes]]
id = "route-for-disabled-model"
account = "acc-enabled"
model = "model-disabled"
enabled = true

[[routes]]
id = "route-disabled"
account = "acc-enabled"
model = "model-enabled"
enabled = false

[virtual_models.group-viable]
enabled = true

[[virtual_models.group-viable.tiers]]
priority = 1
models = ["p1/model-enabled"]

[virtual_models.group-disabled]
enabled = false

[[virtual_models.group-disabled.tiers]]
priority = 1
models = ["p1/model-enabled"]

[virtual_models.group-no-viable-models]
enabled = true

[[virtual_models.group-no-viable-models.tiers]]
priority = 1
models = ["p1/model-disabled"]
"#
        );

        let config = Arc::new(AppConfig::parse(&config_toml).unwrap());
        let live_config = LiveConfig::new(config.clone());
        let catalog = Arc::new(ModelCatalog::connect(live_config.clone()).await.unwrap());
        let conversations = Arc::new(ConversationStore::connect(config.clone()).await.unwrap());
        let execution_traces =
            Arc::new(ExecutionTraceStore::connect(config.clone()).await.unwrap());
        let gateway_api_key = Arc::new("test-key".to_string());
        let client_policies = Arc::new(
            ClientPolicyStore::connect(
                config.clone(),
                live_config.clone(),
                gateway_api_key.clone(),
            )
            .await
            .unwrap(),
        );
        let gateway = Arc::new(
            Gateway::new(
                config.clone(),
                live_config.clone(),
                catalog.clone(),
                execution_traces,
            )
            .unwrap(),
        );

        let state = AppState {
            gateway,
            catalog,
            conversations,
            gateway_api_key,
            client_policies,
        };

        let pool = SqlitePoolOptions::new().connect(&db_url).await.unwrap();

        // Seed model-enabled (enabled=1)
        sqlx::query(
            "INSERT INTO models (canonical_id, provider_id, external_id, display_name, owned_by, enabled)
             VALUES ('p1/model-enabled', 'p1', 'model-enabled', 'Model Enabled', 'p1', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO account_models (account_id, canonical_model_id, availability, enabled, configured, discovered)
             VALUES ('acc-enabled', 'p1/model-enabled', 'available', 1, 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Seed model-disabled (enabled=0)
        sqlx::query(
            "INSERT INTO models (canonical_id, provider_id, external_id, display_name, owned_by, enabled)
             VALUES ('p1/model-disabled', 'p1', 'model-disabled', 'Model Disabled', 'p1', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO account_models (account_id, canonical_model_id, availability, enabled, configured, discovered)
             VALUES ('acc-enabled', 'p1/model-disabled', 'available', 0, 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer test-key"));

        let response = models(State(state), headers).await;
        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json_val: Value = serde_json::from_slice(&body_bytes).unwrap();
        let items = json_val["data"].as_array().unwrap();
        let ids: HashSet<&str> = items
            .iter()
            .filter_map(|item| item["id"].as_str())
            .collect();

        // Physical model: enabled should be present, disabled should NOT
        assert!(
            ids.contains("p1/model-enabled"),
            "p1/model-enabled should be present"
        );
        assert!(
            !ids.contains("p1/model-disabled"),
            "p1/model-disabled should NOT be present"
        );

        // Virtual model (group): enabled groups should be present; disabled groups should NOT
        assert!(
            ids.contains("group-viable"),
            "group-viable should be present"
        );
        assert!(
            !ids.contains("group-disabled"),
            "group-disabled should NOT be present"
        );
        assert!(
            ids.contains("group-no-viable-models"),
            "group-no-viable-models should be present because it is enabled"
        );

        // Routes: active route on enabled account with enabled model should be present
        assert!(
            ids.contains("route-active"),
            "route-active should be present"
        );
        assert!(
            !ids.contains("route-for-disabled-acc"),
            "route-for-disabled-acc should NOT be present"
        );
        assert!(
            !ids.contains("route-for-disabled-model"),
            "route-for-disabled-model should NOT be present"
        );
        assert!(
            !ids.contains("route-disabled"),
            "route-disabled should NOT be present"
        );

        pool.close().await;
        let _ = fs::remove_file(&temp_db);
    }

    #[test]
    fn claude_code_headers_define_session_affinity_without_body_parsing() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            HeaderValue::from_static("session-a"),
        );
        headers.insert(
            "x-claude-code-agent-id",
            HeaderValue::from_static("agent-b"),
        );
        headers.insert(
            "x-claude-code-parent-agent-id",
            HeaderValue::from_static("agent-parent"),
        );
        let context = ClaudeCodeRequestContext::from_headers(&headers);
        assert_eq!(context.thread_id().as_deref(), Some("claude-code:session-a"));
        assert_eq!(context.agent_id.as_deref(), Some("agent-b"));
        assert_eq!(context.parent_agent_id.as_deref(), Some("agent-parent"));

        let mut other = headers.clone();
        other.insert(
            "x-claude-code-session-id",
            HeaderValue::from_static("session-b"),
        );
        assert_ne!(
            context.thread_id(),
            ClaudeCodeRequestContext::from_headers(&other).thread_id()
        );
    }

    #[tokio::test]
    async fn compatibility_threads_keep_sessions_isolated_and_sticky() {
        let temp_db = format!("/tmp/llmgateway-claude-affinity-{}.db", Uuid::new_v4());
        let config = Arc::new(
            AppConfig::parse(&format!(
                r#"
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "model-a"

[storage]
database_url = "sqlite://{temp_db}"
"#
            ))
            .unwrap(),
        );
        let store = ConversationStore::connect(config).await.unwrap();
        let a = store
            .ensure_compatibility_thread("claude-code:a", "Claude Code session", "model-a")
            .await
            .unwrap();
        let b = store
            .ensure_compatibility_thread("claude-code:b", "Claude Code session", "model-a")
            .await
            .unwrap();
        assert_eq!(a.sticky_route, None);
        assert_eq!(b.sticky_route, None);

        store
            .update_thread_route_and_model("claude-code:a", "route-a", "model-a")
            .await
            .unwrap();
        assert_eq!(
            store.context("claude-code:a").await.unwrap().sticky_route.as_deref(),
            Some("route-a")
        );
        assert_eq!(
            store.context("claude-code:b").await.unwrap().sticky_route,
            None
        );
        let _ = fs::remove_file(&temp_db);
    }

    #[test]
    fn anthropic_errors_preserve_status_and_recovery_wording() {
        let response = anthropic_gateway_error(GatewayError::Execution {
            request_id: "req_529".into(),
            source: Box::new(GatewayError::Upstream {
                status: StatusCode::from_u16(529).unwrap(),
                body: "overloaded: retry this request".into(),
            }),
        });
        assert_eq!(response.status().as_u16(), 529);
        assert_eq!(
            response.headers().get("request-id").unwrap(),
            "req_529"
        );

        let capability =
            anthropic_gateway_error(GatewayError::BrowserAdapterIncompatible("tools unsupported".into()));
        assert_eq!(capability.status(), StatusCode::BAD_GATEWAY);
    }


    async fn anthropic_protocol_test_state(temp_db: &str) -> AppState {
        let config = Arc::new(
            AppConfig::parse(&format!(
                r#"
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "p1/model-a"

[storage]
database_url = "sqlite://{temp_db}"

[[providers]]
id = "p1"
kind = "openai-compatible"
base_url = "http://127.0.0.1:9"

[[accounts]]
id = "a1"
provider = "p1"
api_key_env = "UPSTREAM_KEY"
enabled = true

[[routes]]
id = "route-a"
account = "a1"
model = "model-a"
enabled = true
"#
            ))
            .unwrap(),
        );
        let live_config = LiveConfig::new(config.clone());
        let catalog = Arc::new(ModelCatalog::connect(live_config.clone()).await.unwrap());
        let conversations = Arc::new(ConversationStore::connect(config.clone()).await.unwrap());
        let execution_traces =
            Arc::new(ExecutionTraceStore::connect(config.clone()).await.unwrap());
        let gateway_api_key = Arc::new("test-key".to_string());
        let client_policies = Arc::new(
            ClientPolicyStore::connect(
                config.clone(),
                live_config.clone(),
                gateway_api_key.clone(),
            )
            .await
            .unwrap(),
        );
        let gateway = Arc::new(
            Gateway::new(
                config,
                live_config,
                catalog.clone(),
                execution_traces,
            )
            .unwrap(),
        );
        AppState {
            gateway,
            catalog,
            conversations,
            gateway_api_key,
            client_policies,
        }
    }

    #[tokio::test]
    async fn anthropic_messages_accepts_body_larger_than_axum_default_limit() {
        let temp_db = format!("/tmp/llmgateway-anthropic-large-{}.db", Uuid::new_v4());
        let state = anthropic_protocol_test_state(&temp_db).await;
        let app = axum::Router::new()
            .route(
                "/v1/messages",
                axum::routing::post(anthropic_messages)
                    .layer(axum::extract::DefaultBodyLimit::max(
                        ANTHROPIC_MAX_REQUEST_BODY_BYTES,
                    )),
            )
            .with_state(state);

        let padding = "x".repeat(2 * 1024 * 1024 + 64 * 1024);
        let payload = json!({
            "model":"p1/model-a",
            "max_tokens":16,
            "messages":[{"role":"user","content":"hello"}],
            "future_optional_field":padding
        })
        .to_string();
        assert!(payload.len() > 2 * 1024 * 1024);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "a >2 MB body must reach the Anthropic handler instead of Axum's default 2 MB rejection"
        );
        assert!(response.headers().get("request-id").is_some());
        let _ = fs::remove_file(&temp_db);
    }

    #[tokio::test]
    async fn anthropic_payload_too_large_uses_error_envelope_and_request_id() {
        let temp_db = format!("/tmp/llmgateway-anthropic-413-{}.db", Uuid::new_v4());
        let state = anthropic_protocol_test_state(&temp_db).await;
        let app = axum::Router::new()
            .route(
                "/v1/messages",
                axum::routing::post(anthropic_messages)
                    .layer(axum::extract::DefaultBodyLimit::max(128)),
            )
            .with_state(state);

        let payload = json!({
            "model":"p1/model-a",
            "max_tokens":16,
            "messages":[{"role":"user","content":"x".repeat(512)}]
        })
        .to_string();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let header_request_id = response
            .headers()
            .get("request-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(
            response.headers().get("x-llmgateway-request-id").unwrap(),
            header_request_id.as_str()
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "request_too_large");
        assert_eq!(body["request_id"], header_request_id);
        let _ = fs::remove_file(&temp_db);
    }


    #[tokio::test]
    async fn anthropic_upstream_error_matrix_preserves_http_and_wording() {
        let cases = [
            (400u16, "invalid_request_error"),
            (401u16, "authentication_error"),
            (403u16, "permission_error"),
            (409u16, "invalid_request_error"),
            (413u16, "request_too_large"),
            (429u16, "rate_limit_error"),
            (500u16, "api_error"),
            (504u16, "api_error"),
            (529u16, "overloaded_error"),
        ];

        for (status, expected_type) in cases {
            let message = format!("upstream-{status}: preserve capability_rejected: detail");
            let request_id = format!("req_{status}");
            let response = anthropic_gateway_error(GatewayError::Execution {
                request_id: request_id.clone(),
                source: Box::new(GatewayError::Upstream {
                    status: StatusCode::from_u16(status).unwrap(),
                    body: message.clone(),
                }),
            });
            assert_eq!(response.status().as_u16(), status);
            assert_eq!(
                response.headers().get("request-id").unwrap(),
                request_id.as_str()
            );
            assert_eq!(
                response.headers().get("x-llmgateway-request-id").unwrap(),
                request_id.as_str()
            );
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["type"], "error");
            assert_eq!(body["error"]["type"], expected_type);
            assert_eq!(body["error"]["message"], message);
            assert_eq!(body["request_id"], request_id);
        }
    }

}
