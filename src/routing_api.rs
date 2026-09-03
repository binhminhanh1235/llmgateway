use crate::api::{authorize, json_response, AppState};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Response, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
pub struct ExplainRoutesRequest {
    #[serde(default)]
    pub model: Option<String>,
}

pub async fn explain_routes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ExplainRoutesRequest>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    let requested_model = request
        .model
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| state.gateway.config.api.default_model.clone());
    let trace = state.gateway.router.explain(&requested_model).await;
    json_response(StatusCode::OK, json!(trace), None)
}
