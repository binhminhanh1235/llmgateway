use crate::{
    api::{authorize, json_response, AppState},
    execution_trace::ExecutionTraceError,
};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, Response, StatusCode},
};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
pub struct ListExecutionsQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
}

pub async fn list_executions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListExecutionsQuery>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    match state.gateway.execution_traces.list(query.limit).await {
        Ok(executions) => json_response(StatusCode::OK, json!({"data": executions}), None),
        Err(error) => trace_error(error),
    }
}

pub async fn get_execution(
    State(state): State<AppState>,
    Path(request_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    match state.gateway.execution_traces.get(&request_id).await {
        Ok(trace) => json_response(StatusCode::OK, json!(trace), None),
        Err(error) => trace_error(error),
    }
}

fn trace_error(error: ExecutionTraceError) -> Response<Body> {
    match error {
        ExecutionTraceError::NotFound(request_id) => json_response(
            StatusCode::NOT_FOUND,
            json!({"error":{"type":"execution_trace_not_found","message":format!("execution trace '{request_id}' was not found")}}),
            None,
        ),
        ExecutionTraceError::Database(error) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":{"type":"execution_trace_database_error","message":error.to_string()}}),
            None,
        ),
        ExecutionTraceError::Io(error) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":{"type":"execution_trace_storage_error","message":error.to_string()}}),
            None,
        ),
    }
}

fn default_limit() -> usize {
    50
}
