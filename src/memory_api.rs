use crate::{
    api::{authorize, json_error, json_response, AppState},
    context_engine::ContextError,
    context_runtime,
    conversation::ConversationError,
    memory_provenance::MemoryProvenanceError,
    memory_provenance_runtime,
};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Response, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
pub struct AddMemoryPinRequest {
    pub category: String,
    pub value: String,
    #[serde(default = "default_manual_confidence")]
    pub confidence: f64,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMemoryItemRequest {
    pub pinned: bool,
}

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
        None => return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "memory_engine_error",
            "context engine is not initialized",
        ),
    };
    let provenance = match memory_provenance_runtime::get() {
        Some(store) => store,
        None => return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "memory_provenance_error",
            "memory provenance store is not initialized",
        ),
    };

    match engine.status(&thread_id, None).await {
        Ok(status) => {
            if let Some(snapshot) = status.memory.as_ref() {
                if let Err(error) = provenance.sync_snapshot(snapshot).await {
                    return provenance_error(error);
                }
            }
            match provenance.list_items(&thread_id).await {
                Ok(items) => json_response(
                    StatusCode::OK,
                    json!({"thread_id":thread_id,"memory":status.memory,"items":items}),
                    None,
                ),
                Err(error) => provenance_error(error),
            }
        }
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

pub async fn add_thread_memory_pin(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<AddMemoryPinRequest>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    if let Err(error) = state.conversations.thread(&thread_id).await {
        return conversation_error(error);
    }
    let store = match memory_provenance_runtime::get() {
        Some(store) => store,
        None => return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "memory_provenance_error",
            "memory provenance store is not initialized",
        ),
    };
    match store
        .add_manual_pin(&thread_id, &body.category, &body.value, body.confidence)
        .await
    {
        Ok(item) => json_response(StatusCode::CREATED, json!({"item":item}), None),
        Err(error) => provenance_error(error),
    }
}

pub async fn update_thread_memory_item(
    State(state): State<AppState>,
    Path((thread_id, item_key)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<UpdateMemoryItemRequest>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let store = match memory_provenance_runtime::get() {
        Some(store) => store,
        None => return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "memory_provenance_error",
            "memory provenance store is not initialized",
        ),
    };
    match store.set_pinned(&thread_id, &item_key, body.pinned).await {
        Ok(item) => json_response(StatusCode::OK, json!({"item":item}), None),
        Err(error) => provenance_error(error),
    }
}

fn provenance_error(error: MemoryProvenanceError) -> Response<Body> {
    match error {
        MemoryProvenanceError::InvalidCategory(_) | MemoryProvenanceError::InvalidConfidence(_) => {
            json_error(StatusCode::BAD_REQUEST, "invalid_memory_item", &error.to_string())
        }
        MemoryProvenanceError::ItemNotFound(_) => {
            json_error(StatusCode::NOT_FOUND, "memory_item_not_found", &error.to_string())
        }
        MemoryProvenanceError::Database(_) | MemoryProvenanceError::Io(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "memory_provenance_error",
            &error.to_string(),
        ),
    }
}

fn conversation_error(error: ConversationError) -> Response<Body> {
    match error {
        ConversationError::ThreadNotFound(message) | ConversationError::ResponseNotFound(message) => {
            json_error(StatusCode::NOT_FOUND, "not_found_error", &message)
        }
        ConversationError::InvalidJson(message) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "conversation_json_error",
            &message,
        ),
        ConversationError::Database(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "conversation_database_error",
            &error.to_string(),
        ),
        ConversationError::Io(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "conversation_storage_error",
            &error.to_string(),
        ),
    }
}

fn default_manual_confidence() -> f64 { 1.0 }
