use crate::{
    api::{authorize, json_error, json_response, AppState},
    chromium_driver::{ChromiumDriver, ChromiumDriverError},
    chromium_driver_runtime,
};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Response, StatusCode},
};
use serde_json::json;
use std::sync::Arc;

pub async fn launch_chromium_login(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let Some(driver) = driver() else {
        return unavailable();
    };
    match driver.launch(&session_id).await {
        Ok(launch) => json_response(StatusCode::OK, json!({"launched":true,"launch":launch}), None),
        Err(error) => driver_error(error),
    }
}

pub async fn chromium_status(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let Some(driver) = driver() else {
        return unavailable();
    };
    match driver.status(&session_id).await {
        Ok(status) => json_response(StatusCode::OK, json!(status), None),
        Err(error) => driver_error(error),
    }
}

pub async fn verify_chromium_login(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let Some(driver) = driver() else {
        return unavailable();
    };
    match driver.verify(&session_id).await {
        Ok(verification) => json_response(StatusCode::OK, json!(verification), None),
        Err(error) => driver_error(error),
    }
}

pub async fn stop_chromium(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let Some(driver) = driver() else {
        return unavailable();
    };
    match driver.stop(&session_id).await {
        Ok(status) => json_response(StatusCode::OK, json!({"stopped":true,"status":status}), None),
        Err(error) => driver_error(error),
    }
}

fn driver() -> Option<&'static Arc<ChromiumDriver>> {
    chromium_driver_runtime::get()
}

fn unavailable() -> Response<Body> {
    json_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "chromium_driver_error",
        "Chromium driver is unavailable",
    )
}

fn driver_error(error: ChromiumDriverError) -> Response<Body> {
    match error {
        ChromiumDriverError::Disabled
        | ChromiumDriverError::SessionDisabled(_)
        | ChromiumDriverError::SessionNotConfigured(_)
        | ChromiumDriverError::ExecutableNotFound
        | ChromiumDriverError::AlreadyRunning(_)
        | ChromiumDriverError::NotRunning(_)
        | ChromiumDriverError::StartupTimeout
        | ChromiumDriverError::InvalidDevToolsPort(_)
        | ChromiumDriverError::InvalidConfig(_) => {
            json_error(StatusCode::BAD_REQUEST, "chromium_driver_error", &error.to_string())
        }
        ChromiumDriverError::BrowserSession(error) => json_error(
            StatusCode::BAD_REQUEST,
            "browser_session_error",
            &error.to_string(),
        ),
        ChromiumDriverError::Launch(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "chromium_launch_error",
            &error.to_string(),
        ),
        ChromiumDriverError::DevToolsTransport(error) => json_error(
            StatusCode::BAD_GATEWAY,
            "chromium_devtools_error",
            &error.to_string(),
        ),
        ChromiumDriverError::DevToolsResponse(message) => json_error(
            StatusCode::BAD_GATEWAY,
            "chromium_devtools_error",
            &message,
        ),
        ChromiumDriverError::Toml(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "chromium_config_error",
            &error.to_string(),
        ),
        ChromiumDriverError::ConfigIo(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "chromium_config_error",
            &error.to_string(),
        ),
    }
}
