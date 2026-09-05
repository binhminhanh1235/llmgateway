use crate::{admin::set_account_model_enabled, api::AppState};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header::{AUTHORIZATION, CONTENT_TYPE}, HeaderMap, HeaderValue, Response, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
pub struct AccountModelToggle {
    pub model_id: String,
    pub enabled: bool,
}

pub async fn set_account_model(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<AccountModelToggle>,
) -> Response<Body> {
    if !authorized(&headers, &state.gateway_api_key) {
        return json_response(
            StatusCode::UNAUTHORIZED,
            json!({"error":{"type":"authentication_error","message":"invalid llmgateway API key"}}),
        );
    }

    if state.gateway.config_snapshot().account(&account_id).is_none() {
        return json_response(
            StatusCode::NOT_FOUND,
            json!({"error":{"type":"not_found_error","message":format!("unknown account '{account_id}'")}}),
        );
    }

    match set_account_model_enabled(
        &state.gateway.config,
        &account_id,
        &body.model_id,
        body.enabled,
    )
    .await
    {
        Ok(()) => json_response(
            StatusCode::OK,
            json!({
                "account_id": account_id,
                "model_id": body.model_id,
                "enabled": body.enabled
            }),
        ),
        Err(message) => json_response(
            StatusCode::BAD_REQUEST,
            json!({"error":{"type":"admin_error","message":message}}),
        ),
    }
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let x_api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok());
    bearer == Some(expected) || x_api_key == Some(expected)
}

fn json_response(status: StatusCode, value: serde_json::Value) -> Response<Body> {
    let mut response = Response::builder()
        .status(status)
        .body(Body::from(value.to_string()))
        .expect("valid admin response");
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}
