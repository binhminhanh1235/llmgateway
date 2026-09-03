use crate::{
    api::{authorize, json_error, json_response, AppState},
    context_runtime,
    semantic_retrieval::retrieve,
};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub struct RetrievalQuery {
    pub q: String,
}

pub async fn get_thread_retrieval(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Query(query): Query<RetrievalQuery>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    if query.q.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "retrieval_error", "q must not be empty");
    }
    let detail = match state.conversations.thread(&thread_id).await {
        Ok(detail) => detail,
        Err(error) => {
            return json_error(StatusCode::NOT_FOUND, "retrieval_error", &error.to_string())
        }
    };
    let engine = match context_runtime::get() {
        Some(engine) => engine,
        None => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "retrieval_error",
                "context engine is not initialized",
            )
        }
    };
    let status = match engine.status(&thread_id, None).await {
        Ok(status) => status,
        Err(error) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "retrieval_error",
                &error.to_string(),
            )
        }
    };
    let through_ordinal = status
        .checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.through_ordinal)
        .unwrap_or_else(|| detail.messages.last().map(|message| message.ordinal).unwrap_or(0));
    let current = json!({"role":"user","content":query.q});
    let chunks = retrieve(
        &detail,
        &current,
        through_ordinal,
        &state.gateway.config.context,
    );
    json_response(
        StatusCode::OK,
        json!({
            "thread_id": thread_id,
            "through_ordinal": through_ordinal,
            "scorer": "lexical-v1",
            "data": chunks
        }),
        None,
    )
}
