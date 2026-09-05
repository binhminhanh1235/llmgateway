use crate::{
    api::{authorize, json_error, json_response, AppState},
    context_engine::ContextError,
    context_runtime,
    conversation::ConversationError,
    semantic_retrieval::retrieve_relevant_history,
};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub struct RetrievalInspectRequest {
    pub query: Value,
    pub max_chunks: Option<usize>,
    pub max_tokens: Option<usize>,
    pub min_score: Option<f64>,
}

pub async fn inspect_thread_retrieval(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<RetrievalInspectRequest>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    let engine = match context_runtime::get() {
        Some(engine) => engine,
        None => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "context_engine_error",
                "context engine is not initialized",
            )
        }
    };
    let detail = match state.conversations.thread(&thread_id).await {
        Ok(detail) => detail,
        Err(ConversationError::ThreadNotFound(message)) => {
            return json_error(StatusCode::NOT_FOUND, "not_found_error", &message)
        }
        Err(error) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "retrieval_context_error",
                &error.to_string(),
            )
        }
    };
    let status = match engine.status(&thread_id, None).await {
        Ok(status) => status,
        Err(ContextError::Conversation(ConversationError::ThreadNotFound(message))) => {
            return json_error(StatusCode::NOT_FOUND, "not_found_error", &message)
        }
        Err(error) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "retrieval_context_error",
                &error.to_string(),
            )
        }
    };
    let Some(checkpoint) = status.checkpoint else {
        return json_response(
            StatusCode::OK,
            json!({
                "thread_id":thread_id,
                "checkpoint_through_ordinal":null,
                "chunks":[],
                "estimated_tokens":0,
                "rendered":"",
                "reason":"retrieval only searches transcript history already represented by a checkpoint"
            }),
            None,
        );
    };
    let config = &state.gateway.config.context;
    let max_chunks = body.max_chunks.unwrap_or(config.retrieval_max_chunks).max(1);
    let max_tokens = body.max_tokens.unwrap_or(config.retrieval_max_tokens).max(1);
    let min_score = body.min_score.unwrap_or(config.retrieval_min_score).max(0.0);
    let query_message = json!({"role":"user","content":body.query});
    let result = retrieve_relevant_history(
        &detail.messages,
        checkpoint.through_ordinal,
        &query_message,
        max_chunks,
        max_tokens,
        min_score,
    );

    json_response(
        StatusCode::OK,
        json!({
            "thread_id":thread_id,
            "checkpoint_through_ordinal":checkpoint.through_ordinal,
            "chunks":result.chunks,
            "estimated_tokens":result.estimated_tokens,
            "rendered":result.rendered
        }),
        None,
    )
}
