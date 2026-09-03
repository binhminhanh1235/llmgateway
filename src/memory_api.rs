use crate::{
    api::{authorize, json_error, json_response, AppState},
    context_engine::ContextError,
    context_runtime,
    conversation::ConversationError,
};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Response, StatusCode},
};
use serde_json::json;

pub async fn get_thread_memory(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    let engine = match context_runtime::get() {
        Some(engine) => engine,
        None => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "memory_engine_error",
                "context engine is not initialized",
            )
        }
    };

    match engine.status(&thread_id, None).await {
        Ok(status) => json_response(
            StatusCode::OK,
            json!({"thread_id":thread_id,"memory":status.memory}),
            None,
        ),
        Err(ContextError::Conversation(ConversationError::ThreadNotFound(message))) => {
            json_error(StatusCode::NOT_FOUND, "not_found_error", &message)
        }
        Err(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "memory_error",
            &error.to_string(),
        ),
    }
}
