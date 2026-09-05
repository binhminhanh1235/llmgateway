use crate::{
    api::{authorize, json_error, json_response, AppState},
    browser_provider_runtime,
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
use tracing::warn;

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
        Ok(mut verification) => {
            if verification.authenticated && verification.auth_material_captured {
                refresh_session_browser_models(&state, &session_id).await;
                if session_can_release_browser(&state, &session_id).await {
                    if let Ok(status) = driver.stop(&session_id).await {
                        verification.browser_closed_after_capture = true;
                        verification.status = status;
                    }
                }
            }
            json_response(StatusCode::OK, json!(verification), None)
        }
        Err(error) => driver_error(error),
    }
}

async fn refresh_session_browser_models(state: &AppState, session_id: &str) {
    let Some(registry) = browser_provider_runtime::get() else {
        return;
    };
    let config = state.gateway.config_snapshot();
    for account in config.accounts.iter().filter(|account| account.enabled) {
        if registry.session_id_for_account(&account.id).as_deref() != Some(session_id) {
            continue;
        }
        let Some(provider) = config.provider(&account.provider) else {
            continue;
        };
        if !registry.account_supports_model_discovery(&provider.kind, &account.id) {
            continue;
        }
        if let Err(error) = state.catalog.refresh_account(&account.id).await {
            warn!(
                %error,
                account = %account.id,
                provider = %provider.id,
                "browser account model discovery after login failed"
            );
        }
    }
}

async fn session_can_release_browser(state: &AppState, session_id: &str) -> bool {
    let Some(registry) = browser_provider_runtime::get() else {
        return false;
    };
    let config = state.gateway.config_snapshot();
    let mut matched = false;

    for account in config.accounts.iter().filter(|account| account.enabled) {
        if registry.session_id_for_account(&account.id).as_deref() != Some(session_id) {
            continue;
        }
        matched = true;
        let Some(provider) = config.provider(&account.provider) else {
            return false;
        };
        let diagnostics = registry
            .adapter_diagnostics(&provider.kind, &account.id)
            .await;
        if diagnostics.status != "ready"
            || !matches!(
                diagnostics.adapter_id.as_deref(),
                Some("gemini-web-http" | "chatgpt-web-http")
            )
        {
            return false;
        }
    }

    matched
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
        | ChromiumDriverError::InvalidDevToolsPort(_)
        | ChromiumDriverError::InvalidConfig(_) => {
            json_error(StatusCode::BAD_REQUEST, "chromium_driver_error", &error.to_string())
        }
        ChromiumDriverError::BrowserSession(error) => json_error(
            StatusCode::BAD_REQUEST,
            "browser_session_error",
            &error.to_string(),
        ),
        ChromiumDriverError::Launch(_)
        | ChromiumDriverError::DevToolsPortReservation(_)
        | ChromiumDriverError::EarlyExit(_)
        | ChromiumDriverError::StartupTimeout(_) => json_error(
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
