use crate::{
    api::{authorize, json_error, json_response, AppState},
    quota_usage::UsageError,
    quota_usage_runtime,
};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Response, StatusCode},
};
use serde_json::json;

pub async fn get_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let Some(store) = quota_usage_runtime::get() else {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "usage_unavailable",
            "quota usage runtime is not initialized",
        );
    };
    match store.summary().await {
        Ok(summary) => json_response(StatusCode::OK, json!(summary), None),
        Err(error) => usage_error(error),
    }
}

pub async fn get_account_usage(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let Some(store) = quota_usage_runtime::get() else {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "usage_unavailable",
            "quota usage runtime is not initialized",
        );
    };
    match store.account_snapshot(&account_id).await {
        Ok(snapshot) => json_response(StatusCode::OK, json!(snapshot), None),
        Err(error) => usage_error(error),
    }
}

pub async fn reset_account_quota(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let Some(store) = quota_usage_runtime::get() else {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "usage_unavailable",
            "quota usage runtime is not initialized",
        );
    };
    match store.reset_account_state(&account_id).await {
        Ok(()) => json_response(
            StatusCode::OK,
            json!({"account_id":account_id,"reset":true}),
            None,
        ),
        Err(error) => usage_error(error),
    }
}

fn usage_error(error: UsageError) -> Response<Body> {
    match error {
        UsageError::InvalidConfig(message) => {
            json_error(StatusCode::BAD_REQUEST, "usage_error", &message)
        }
        UsageError::Database(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "usage_database_error",
            &error.to_string(),
        ),
        UsageError::Io(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "usage_storage_error",
            &error.to_string(),
        ),
        UsageError::Toml(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "usage_config_error",
            &error.to_string(),
        ),
    }
}
