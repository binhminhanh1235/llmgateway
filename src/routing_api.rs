use crate::api::{authorize, json_response, AppState};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Response, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

#[derive(Debug, Deserialize)]
pub struct ExplainRoutesRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub body: Option<Value>,
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

    let has_task_context = request.task.is_some() || request.body.is_some();
    let mut body = request
        .body
        .unwrap_or_else(|| Value::Object(Map::new()));
    if let Some(task) = request.task.filter(|task| !task.trim().is_empty()) {
        if let Some(object) = body.as_object_mut() {
            object.insert("llmgateway_task".into(), Value::String(task));
        }
    }

    let trace = state
        .gateway
        .router
        .explain_for_body(&requested_model, has_task_context.then_some(&body))
        .await;
    json_response(StatusCode::OK, json!(trace), None)
}
