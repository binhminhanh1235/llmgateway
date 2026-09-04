use crate::{
    catalog::{CatalogError, ModelCatalog},
    compat::{anthropic, responses},
    conversation::{ConversationError, ConversationStore},
    gateway::{Gateway, GatewayError},
    response_state::{response_to_openai_assistant, responses_stream_with_capture},
};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{
        header::{AUTHORIZATION, CONTENT_TYPE},
        HeaderMap, HeaderValue, Response, StatusCode,
    },
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
}

pub async fn openai_chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&state.gateway.config.api.default_model)
        .to_string();
    let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    match state
        .gateway
        .execute_openai_chat(&requested_model, &body)
        .await
    {
        Ok(routed) => {
            let route_id = routed.route.id.clone();
            let request_id = routed.request_id.clone();
            if is_stream {
                let stream = routed.response.bytes_stream().map_err(std::io::Error::other);
                response_with_route_and_request(
                    StatusCode::OK,
                    "text/event-stream",
                    Body::from_stream(stream),
                    &route_id,
                    &request_id,
                )
            } else {
                match routed.response.bytes().await {
                    Ok(bytes) => response_with_route_and_request(
                        StatusCode::OK,
                        "application/json",
                        Body::from(bytes),
                        &route_id,
                        &request_id,
                    ),
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
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

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
        preferred_route = previous.route_id.clone();
        let current = request_messages(&openai_body);
        let mut merged = previous.messages;
        merged.extend(current);
        set_request_messages(&mut openai_body, merged);
    }

    let history_before_response = request_messages(&openai_body);
    match state
        .gateway
        .execute_openai_chat_with_affinity(
            &requested_model,
            &openai_body,
            preferred_route.as_deref(),
        )
        .await
    {
        Ok(routed) => {
            let route_id = routed.route.id.clone();
            let request_id = routed.request_id.clone();
            if is_stream {
                let stream = responses::openai_stream_to_responses(
                    routed.response,
                    requested_model.clone(),
                );
                let (tx, rx) = oneshot::channel();
                let stream = responses_stream_with_capture(stream, tx);
                let conversations = state.conversations.clone();
                let model_for_task = requested_model.clone();
                let route_for_task = route_id.clone();
                let history_for_task = history_before_response.clone();
                tokio::spawn(async move {
                    if let Ok(response) = rx.await {
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
                match routed.response.json::<Value>().await {
                    Ok(openai) => {
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
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let (requested_model, openai_body) = match anthropic::to_openai_request(&body) {
        Ok(value) => value,
        Err(message) => {
            return json_error(StatusCode::BAD_REQUEST, "invalid_request_error", &message)
        }
    };
    let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    match state
        .gateway
        .execute_openai_chat(&requested_model, &openai_body)
        .await
    {
        Ok(routed) => {
            let route_id = routed.route.id.clone();
            let request_id = routed.request_id.clone();
            if is_stream {
                let stream =
                    anthropic::openai_stream_to_anthropic(routed.response, requested_model);
                response_with_route_and_request(
                    StatusCode::OK,
                    "text/event-stream",
                    Body::from_stream(stream),
                    &route_id,
                    &request_id,
                )
            } else {
                match routed.response.json::<Value>().await {
                    Ok(openai) => {
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
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    let physical = match state.catalog.selectable_models().await {
        Ok(models) => models,
        Err(error) => return catalog_error(error),
    };
    let mut data: BTreeMap<String, Value> = BTreeMap::new();

    for id in state.gateway.config.virtual_models.keys() {
        data.insert(
            id.clone(),
            json!({
                "id":id,
                "object":"model",
                "owned_by":"llmgateway",
                "llmgateway":{"kind":"virtual"}
            }),
        );
    }

    for model in physical {
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

    // Preserve v0.1 route IDs as selectable aliases for existing clients.
    for route in state.gateway.config.routes.iter().filter(|route| route.enabled) {
        data.entry(route.id.clone()).or_insert_with(|| {
            json!({
                "id":route.id,
                "object":"model",
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
        "default_model":state.gateway.config.api.default_model,
        "catalog_models":catalog_models,
        "threads":threads,
        "routes":routes
    }))
}

pub(crate) fn authorize(headers: &HeaderMap, expected: &str) -> Result<(), Response<Body>> {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let x_api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok());
    if bearer == Some(expected) || x_api_key == Some(expected) {
        Ok(())
    } else {
        Err(json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid llmgateway API key",
        ))
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
