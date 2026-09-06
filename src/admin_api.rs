use crate::{
    admin::{
        set_account_all_models_enabled, set_account_enabled_in_config, set_account_model_enabled,
        set_model_enabled,
    },
    api::AppState,
};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{
        header::{AUTHORIZATION, CONTENT_TYPE},
        HeaderMap, HeaderValue, Response, StatusCode,
    },
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::{env, sync::Arc};

#[derive(Debug, Deserialize)]
pub struct AccountModelToggle {
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub all: bool,
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct AccountToggle {
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct ModelToggle {
    pub enabled: bool,
}

pub async fn set_account(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<AccountToggle>,
) -> Response<Body> {
    if !authorized(&headers, &state.gateway_api_key) {
        return unauthorized();
    }

    let config_path =
        env::var("LLMGATEWAY_CONFIG").unwrap_or_else(|_| "config/llmgateway.toml".into());
    match set_account_enabled_in_config(&config_path, &account_id, body.enabled) {
        Ok(config) => {
            let config = Arc::new(config);
            if let Err(error) = state.catalog.seed_from_app_config(config.as_ref()).await {
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error":{"type":"admin_error","message":format!("failed to refresh catalog after account toggle: {error}")}}),
                );
            }
            state.gateway.live_config.replace(config);
            json_response(
                StatusCode::OK,
                json!({
                    "account_id": account_id,
                    "enabled": body.enabled,
                    "restart_required": false
                }),
            )
        }
        Err(message) if message.starts_with("unknown account") => json_response(
            StatusCode::NOT_FOUND,
            json!({"error":{"type":"not_found_error","message":message}}),
        ),
        Err(message) => json_response(
            StatusCode::BAD_REQUEST,
            json!({"error":{"type":"admin_error","message":message}}),
        ),
    }
}

pub async fn set_model(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ModelToggle>,
) -> Response<Body> {
    if !authorized(&headers, &state.gateway_api_key) {
        return unauthorized();
    }

    let config = state.gateway.config_snapshot();
    match set_model_enabled(config.as_ref(), &model_id, body.enabled).await {
        Ok(affected_models) => json_response(
            StatusCode::OK,
            json!({
                "model_id": model_id,
                "enabled": body.enabled,
                "affected_models": affected_models
            }),
        ),
        Err(message) if message.starts_with("unknown model") => json_response(
            StatusCode::NOT_FOUND,
            json!({"error":{"type":"not_found_error","message":message}}),
        ),
        Err(message) => json_response(
            StatusCode::BAD_REQUEST,
            json!({"error":{"type":"admin_error","message":message}}),
        ),
    }
}

pub async fn set_account_model(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<AccountModelToggle>,
) -> Response<Body> {
    if !authorized(&headers, &state.gateway_api_key) {
        return unauthorized();
    }

    let config = state.gateway.config_snapshot();
    if config.account(&account_id).is_none() {
        return json_response(
            StatusCode::NOT_FOUND,
            json!({"error":{"type":"not_found_error","message":format!("unknown account '{account_id}'")}}),
        );
    }

    if body.all || body.model_id.as_deref() == Some("*") {
        match set_account_all_models_enabled(config.as_ref(), &account_id, body.enabled).await {
            Ok(count) => json_response(
                StatusCode::OK,
                json!({
                    "account_id": account_id,
                    "all": true,
                    "enabled": body.enabled,
                    "affected": count
                }),
            ),
            Err(message) => json_response(
                StatusCode::BAD_REQUEST,
                json!({"error":{"type":"admin_error","message":message}}),
            ),
        }
    } else if let Some(model_id) = body.model_id.as_deref() {
        match set_account_model_enabled(config.as_ref(), &account_id, model_id, body.enabled).await
        {
            Ok(()) => json_response(
                StatusCode::OK,
                json!({
                    "account_id": account_id,
                    "model_id": model_id,
                    "enabled": body.enabled
                }),
            ),
            Err(message) => json_response(
                StatusCode::BAD_REQUEST,
                json!({"error":{"type":"admin_error","message":message}}),
            ),
        }
    } else {
        json_response(
            StatusCode::BAD_REQUEST,
            json!({"error":{"type":"bad_request","message":"either 'model_id' or 'all: true' must be specified"}}),
        )
    }
}

fn unauthorized() -> Response<Body> {
    json_response(
        StatusCode::UNAUTHORIZED,
        json!({"error":{"type":"authentication_error","message":"invalid llmgateway API key"}}),
    )
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
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}
