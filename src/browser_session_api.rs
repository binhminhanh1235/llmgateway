use crate::{
    api::{authorize, json_error, json_response, AppState},
    browser_session::{BrowserSessionError, BrowserSessionStore},
    browser_session_runtime,
};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Response, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct AttentionRequest {
    pub error: String,
}

pub async fn list_browser_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let Some(store) = store() else {
        return unavailable();
    };
    match store.summary().await {
        Ok(summary) => json_response(StatusCode::OK, json!(summary), None),
        Err(error) => browser_error(error),
    }
}

pub async fn get_browser_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let Some(store) = store() else {
        return unavailable();
    };
    match store.session(&session_id).await {
        Ok(session) => json_response(StatusCode::OK, json!(session), None),
        Err(error) => browser_error(error),
    }
}

pub async fn begin_browser_login(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let Some(store) = store() else {
        return unavailable();
    };
    match store.begin_login(&session_id).await {
        Ok(login) => json_response(StatusCode::OK, json!(login), None),
        Err(error) => browser_error(error),
    }
}

pub async fn complete_browser_login(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let Some(store) = store() else {
        return unavailable();
    };
    match store.mark_ready(&session_id).await {
        Ok(session) => json_response(StatusCode::OK, json!({"ready":true,"session":session}), None),
        Err(error) => browser_error(error),
    }
}

pub async fn verify_browser_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let Some(store) = store() else {
        return unavailable();
    };
    match store.mark_verified(&session_id).await {
        Ok(session) => json_response(StatusCode::OK, json!({"verified":true,"session":session}), None),
        Err(error) => browser_error(error),
    }
}

pub async fn require_browser_attention(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<AttentionRequest>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    if body.error.trim().is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "browser_session_error",
            "error must not be empty",
        );
    }
    let Some(store) = store() else {
        return unavailable();
    };
    match store.require_attention(&session_id, body.error.trim()).await {
        Ok(session) => json_response(StatusCode::OK, json!({"requires_attention":true,"session":session}), None),
        Err(error) => browser_error(error),
    }
}

pub async fn reset_browser_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let Some(store) = store() else {
        return unavailable();
    };
    match store.reset(&session_id).await {
        Ok(session) => json_response(StatusCode::OK, json!({"reset":true,"session":session}), None),
        Err(error) => browser_error(error),
    }
}

fn store() -> Option<&'static Arc<BrowserSessionStore>> {
    browser_session_runtime::get()
}

fn unavailable() -> Response<Body> {
    json_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "browser_session_error",
        "browser session registry is unavailable",
    )
}

fn browser_error(error: BrowserSessionError) -> Response<Body> {
    match error {
        BrowserSessionError::NotFound(message) => {
            json_error(StatusCode::NOT_FOUND, "browser_session_error", &format!("browser session '{message}' was not found"))
        }
        BrowserSessionError::InvalidConfig(message) => {
            json_error(StatusCode::BAD_REQUEST, "browser_session_error", &message)
        }
        BrowserSessionError::Database(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "browser_session_database_error",
            &error.to_string(),
        ),
        BrowserSessionError::Io(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "browser_session_storage_error",
            &error.to_string(),
        ),
        BrowserSessionError::Toml(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "browser_session_config_error",
            &error.to_string(),
        ),
    }
}
