use crate::{
    compat::{anthropic, responses},
    gateway::{Gateway, GatewayError},
};
use axum::{
    body::Body,
    extract::State,
    http::{
        header::{AUTHORIZATION, CONTENT_TYPE},
        HeaderMap, HeaderValue, Response, StatusCode,
    },
    response::IntoResponse,
    Json,
};
use futures_util::TryStreamExt;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub gateway: Arc<Gateway>,
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
            if is_stream {
                let stream = routed.response.bytes_stream().map_err(std::io::Error::other);
                response_with_route(
                    StatusCode::OK,
                    "text/event-stream",
                    Body::from_stream(stream),
                    &route_id,
                )
            } else {
                match routed.response.bytes().await {
                    Ok(bytes) => response_with_route(
                        StatusCode::OK,
                        "application/json",
                        Body::from(bytes),
                        &route_id,
                    ),
                    Err(error) => gateway_error(GatewayError::Transport(error.to_string())),
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
    let (requested_model, openai_body) = match responses::to_openai_request(&body) {
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
            if is_stream {
                let stream =
                    responses::openai_stream_to_responses(routed.response, requested_model);
                response_with_route(
                    StatusCode::OK,
                    "text/event-stream",
                    Body::from_stream(stream),
                    &route_id,
                )
            } else {
                match routed.response.json::<Value>().await {
                    Ok(openai) => {
                        let response = responses::from_openai_response(&openai, &requested_model);
                        json_response(StatusCode::OK, response, Some(&route_id))
                    }
                    Err(error) => gateway_error(GatewayError::Transport(error.to_string())),
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
            if is_stream {
                let stream =
                    anthropic::openai_stream_to_anthropic(routed.response, requested_model);
                response_with_route(
                    StatusCode::OK,
                    "text/event-stream",
                    Body::from_stream(stream),
                    &route_id,
                )
            } else {
                match routed.response.json::<Value>().await {
                    Ok(openai) => {
                        let anthropic = anthropic::from_openai_response(&openai, &requested_model);
                        json_response(StatusCode::OK, anthropic, Some(&route_id))
                    }
                    Err(error) => gateway_error(GatewayError::Transport(error.to_string())),
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
    let mut ids = state
        .gateway
        .config
        .virtual_models
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    ids.extend(
        state
            .gateway
            .config
            .routes
            .iter()
            .filter(|route| route.enabled)
            .map(|route| route.id.clone()),
    );
    ids.sort();
    ids.dedup();
    let data = ids
        .into_iter()
        .map(|id| json!({"id":id,"object":"model","owned_by":"llmgateway"}))
        .collect::<Vec<_>>();
    json_response(
        StatusCode::OK,
        json!({"object":"list","data":data}),
        None,
    )
}

pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let routes = state.gateway.router.snapshot().await;
    Json(json!({
        "status":"ok",
        "service":"llmgateway",
        "default_model":state.gateway.config.api.default_model,
        "routes":routes
    }))
}

fn authorize(headers: &HeaderMap, expected: &str) -> Result<(), Response<Body>> {
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

fn gateway_error(error: GatewayError) -> Response<Body> {
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
        GatewayError::Upstream { status, body } => {
            json_error(status, "upstream_error", &body)
        }
    }
}

fn json_error(status: StatusCode, kind: &str, message: &str) -> Response<Body> {
    json_response(
        status,
        json!({"error":{"type":kind,"message":message}}),
        None,
    )
}

fn json_response(status: StatusCode, value: Value, route: Option<&str>) -> Response<Body> {
    response_with_route(
        status,
        "application/json",
        Body::from(value.to_string()),
        route.unwrap_or(""),
    )
}

fn response_with_route(
    status: StatusCode,
    content_type: &str,
    body: Body,
    route: &str,
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
    response
}
