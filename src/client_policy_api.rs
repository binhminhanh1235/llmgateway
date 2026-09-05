use crate::{
    api::{authorize, client_policy_error, json_response, AppState},
};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Response, StatusCode},
};
use serde_json::json;

pub async fn list_client_policies(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    match state.client_policies.summaries().await {
        Ok(clients) => json_response(
            StatusCode::OK,
            json!({
                "data": clients,
                "secrets_exposed": false
            }),
            None,
        ),
        Err(error) => client_policy_error(error),
    }
}
