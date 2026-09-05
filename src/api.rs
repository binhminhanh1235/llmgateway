use crate::{
    artifact_store::ArtifactStore,
    catalog::{canonical_model_id, CatalogError, ModelCatalog},
    compat::{anthropic, responses},
    client_policy::{ClientAccess, ClientPolicyError, ClientPolicyStore},
    conversation::{ConversationError, ConversationStore},
    gateway::{Gateway, GatewayError},
    multimodal::{
        canonical_input_modalities, canonical_output_modalities, AdapterCapabilities,
        ModelCapabilities, MultimodalError, MULTIMODAL_SCHEMA_VERSION,
    },
    multimodal_compat,
    quota_usage_runtime,
    response_state::{response_to_openai_assistant, responses_stream_with_capture},
};
use axum::{
    body::{to_bytes, Body},
    extract::{Path, Request, State},
    middleware::Next,
    http::{
        header::{AUTHORIZATION, CONTENT_TYPE},
        HeaderMap, HeaderValue, Response, StatusCode,
    },
    response::IntoResponse,
    Json,
};
use futures_util::TryStreamExt;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tokio::sync::oneshot;

#[derive(Clone)]
pub struct AppState {
    pub gateway: Arc<Gateway>,
    pub catalog: Arc<ModelCatalog>,
    pub conversations: Arc<ConversationStore>,
    pub artifacts: Arc<ArtifactStore>,
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
    let normalized = match multimodal_compat::normalize_chat_request(&body, &requested_model) {
        Ok(normalized) => normalized,
        Err(error) => return multimodal_error(error),
    };
    body = normalized.into_current_execution();
    let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if let Err(error) = state.client_policies.enforce_model(&access, &requested_model) {
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

    let normalized = match multimodal_compat::normalize_responses_request(&normalized_body) {
        Ok(normalized) => normalized,
        Err(error) => return multimodal_error(error),
    };
    let requested_model = normalized.canonical.model.clone();
    let mut openai_body = normalized.into_current_execution();
    let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if let Err(error) = state.client_policies.enforce_model(&access, &requested_model) {
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
        if response_owner.is_some()
            && previous.client_id.as_deref() != response_owner.as_deref()
        {
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
                let stream = responses::openai_stream_to_responses(
                    response,
                    requested_model.clone(),
                );
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

pub async fn anthropic_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };
    let normalized = match multimodal_compat::normalize_anthropic_request(&body) {
        Ok(normalized) => normalized,
        Err(error) => return multimodal_error(error),
    };
    let requested_model = normalized.canonical.model.clone();
    let mut openai_body = normalized.into_current_execution();
    let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if let Err(error) = state.client_policies.enforce_model(&access, &requested_model) {
        return client_policy_error(error);
    }
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
        .execute_openai_chat_for_client(&requested_model, &openai_body, access.policy())
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
                    anthropic::openai_stream_to_anthropic(response, requested_model);
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
                        let anthropic = anthropic::from_openai_response(&openai, &requested_model);
                        json_response_with_request(
                            StatusCode::OK,
                            anthropic,
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

    for id in config.virtual_models.keys() {
        if access.policy().is_some_and(|policy| !policy.model_allowed(id, id)) {
            continue;
        }
        data.insert(
            id.clone(),
            json!({
                "id":id,
                "object":"model",
                "owned_by":"llmgateway",
                "llmgateway":{
                    "kind":"virtual",
                    "multimodal_capabilities":ModelCapabilities::foundation_text_execution()
                }
            }),
        );
    }

    for model in physical {
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
        let multimodal_capabilities = ModelCapabilities::from_legacy_tags(&model.capabilities);
        data.insert(
            model.id.clone(),
            json!({
                "id":model.id,
                "object":"model",
                "owned_by":model.owned_by,
                "llmgateway":{
                    "kind":"physical",
                    "provider":model.provider,
                    "display_name":model.display_name,
                    "context_window":model.context_window,
                    "capabilities":model.capabilities,
                    "multimodal_capabilities":multimodal_capabilities,
                    "available_accounts":available_accounts
                }
            }),
        );
    }

    // Preserve v0.1 route IDs as selectable aliases for existing clients.
    for route in config.routes.iter().filter(|route| route.enabled) {
        if access.policy().is_some_and(|policy| {
            !policy.route_allowed(&route.id)
                || !policy.model_allowed(&route.id, &route.model)
        }) {
            continue;
        }
        data.entry(route.id.clone()).or_insert_with(|| {
            json!({
                "id":route.id,
                "object":"model",
                "owned_by":"llmgateway-route",
                "llmgateway":{
                    "kind":"route",
                    "upstream_model":route.model,
                    "account":route.account,
                    "capabilities":route.capabilities,
                    "multimodal_capabilities":ModelCapabilities::from_legacy_tags(&route.capabilities)
                }
            })
        });
    }

    json_response(
        StatusCode::OK,
        json!({"object":"list","data":data.into_values().collect::<Vec<_>>() }),
        None,
    )
}


pub async fn capabilities(State(state): State<AppState>, headers: HeaderMap) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };

    let config = state.gateway.config_snapshot();
    let physical = match state.catalog.selectable_models().await {
        Ok(models) => models,
        Err(error) => return catalog_error(error),
    };
    let models = physical
        .into_iter()
        .filter(|model| {
            !access
                .policy()
                .is_some_and(|policy| !policy.model_allowed(&model.id, &model.id))
        })
        .map(|model| {
            let structured = ModelCapabilities::from_legacy_tags(&model.capabilities);
            json!({
                "id":model.id,
                "legacy_capabilities":model.capabilities,
                "capabilities":structured
            })
        })
        .collect::<Vec<_>>();

    let mut tags_by_adapter: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut models_by_adapter: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for route in config.routes.iter().filter(|route| route.enabled) {
        if access.policy().is_some_and(|policy| {
            !policy.route_allowed(&route.id)
                || !policy.model_allowed(&route.id, &route.model)
        }) {
            continue;
        }
        let Some(account) = config.account(&route.account).filter(|account| account.enabled) else {
            continue;
        };
        let Some(provider) = config.provider(&account.provider) else {
            continue;
        };
        tags_by_adapter
            .entry(provider.id.clone())
            .or_default()
            .extend(route.capabilities.iter().cloned());
        models_by_adapter
            .entry(provider.id.clone())
            .or_default()
            .insert(canonical_model_id(&provider.id, &route.model));
    }

    let adapters = config
        .providers
        .iter()
        .filter_map(|provider| {
            let models = models_by_adapter.remove(&provider.id)?;
            let tags = tags_by_adapter
                .remove(&provider.id)
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<_>>();
            Some(AdapterCapabilities {
                id: provider.id.clone(),
                transport: provider.transport().to_string(),
                models: models.into_iter().collect(),
                capabilities: ModelCapabilities::from_legacy_tags(&tags),
            })
        })
        .collect::<Vec<_>>();

    json_response(
        StatusCode::OK,
        json!({
            "object":"llmgateway.capabilities",
            "schema_version":MULTIMODAL_SCHEMA_VERSION,
            "canonical_modalities":{
                "input":canonical_input_modalities(),
                "output":canonical_output_modalities()
            },
            "gateway_execution":ModelCapabilities::foundation_text_execution(),
            "live_attachments":false,
            "artifact_store":{
                "enabled":true,
                "max_file_size_bytes":state.artifacts.config().max_file_size_bytes,
                "max_request_size_bytes":state.artifacts.config().max_request_size_bytes,
                "max_files_per_request":state.artifacts.config().max_files_per_request,
                "allowed_mime_types":state.artifacts.config().allowed_mime_types.clone(),
                "denied_mime_types":state.artifacts.config().denied_mime_types.clone(),
                "remote_url_ingestion":state.artifacts.config().remote_url_ingestion
            },
            "models":models,
            "adapters":adapters
        }),
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
    let catalog_models = state.catalog.models().await.map(|models| models.len()).unwrap_or(0);
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

pub(crate) async fn normalize_json_rejections(
    request: Request,
    next: Next,
) -> Response<Body> {
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
        .or_else(|| headers.get("x-api-key").and_then(|value| value.to_str().ok()))
}

fn multimodal_error(error: MultimodalError) -> Response<Body> {
    json_error(StatusCode::BAD_REQUEST, error.code(), &error.to_string())
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
        ClientPolicyError::BudgetExceeded(message) => {
            json_error(StatusCode::TOO_MANY_REQUESTS, "client_budget_exceeded", &message)
        }
        ClientPolicyError::MissingEnv(message) => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "client_policy_configuration_error",
            &format!("configured client credential environment variable '{message}' is unavailable"),
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
        GatewayError::ClientPolicyDenied(message) => json_error(
            StatusCode::FORBIDDEN,
            "client_policy_error",
            &message,
        ),
        GatewayError::Transport(message) => {
            json_error(StatusCode::BAD_GATEWAY, "upstream_error", &message)
        }
        GatewayError::BrowserSessionUnavailable(message) => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "browser_session_error",
            &message,
        ),
        GatewayError::BrowserTransport(message) => json_error(
            StatusCode::BAD_GATEWAY,
            "browser_transport_error",
            &message,
        ),
        GatewayError::BrowserAdapterIncompatible(message) => json_error(
            StatusCode::BAD_GATEWAY,
            "browser_adapter_incompatible",
            &message,
        ),
        GatewayError::BrowserModelUnavailable(message) => json_error(
            StatusCode::BAD_GATEWAY,
            "browser_model_unavailable",
            &message,
        ),
        GatewayError::BrowserModelRecipeStale(message) => json_error(
            StatusCode::BAD_GATEWAY,
            "model_recipe_stale",
            &message,
        ),
        GatewayError::Upstream { status, body } => json_error(status, "upstream_error", &body),
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
        ConversationError::ThreadNotFound(message) | ConversationError::ResponseNotFound(message) => {
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
                .insert("x-llmgateway-request-id", value);
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
