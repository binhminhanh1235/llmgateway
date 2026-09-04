use crate::{
    api::{authorize, gateway_error, json_error, json_response, response_with_route, AppState},
    context_engine::{ContextError, PreparedContext},
    context_runtime,
    conversation::{openai_stream_with_capture, ConversationError},
    embedding_runtime,
    gateway::GatewayError,
    memory_provenance::{inject_pinned_memory, MemoryProvenanceError},
    memory_provenance_runtime,
    semantic_retrieval::{
        augment_messages_with_retrieval, estimate_json_messages_tokens, retrieve_relevant_history,
        RetrievalResult,
    },
};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::Response,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tracing::warn;

#[derive(Debug, Deserialize)]
pub struct CreateThreadRequest {
    pub title: Option<String>,
    pub model: Option<String>,
    pub messages: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize)]
pub struct ThreadMessageRequest {
    pub content: Value,
    pub model: Option<String>,
    #[serde(default)]
    pub stream: bool,
}

#[derive(Debug, Deserialize)]
pub struct CompactContextRequest {
    pub model: Option<String>,
}

pub async fn create_thread(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateThreadRequest>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let model = body
        .model
        .as_deref()
        .unwrap_or(&state.gateway.config.api.default_model);
    let created = match state
        .conversations
        .create_thread(body.title.as_deref(), model)
        .await
    {
        Ok(thread) => thread,
        Err(error) => return conversation_error(error),
    };

    if let Some(messages) = body.messages {
        for message in messages {
            if let Err(error) = state
                .conversations
                .append_message(&created.id, &message, Some(model), None)
                .await
            {
                return conversation_error(error);
            }
        }
    }

    match state.conversations.thread(&created.id).await {
        Ok(thread) => json_response(StatusCode::CREATED, json!(thread), None),
        Err(error) => conversation_error(error),
    }
}

pub async fn list_threads(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    match state.conversations.list_threads().await {
        Ok(threads) => json_response(StatusCode::OK, json!({"data":threads}), None),
        Err(error) => conversation_error(error),
    }
}

pub async fn get_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    match state.conversations.thread(&thread_id).await {
        Ok(thread) => json_response(StatusCode::OK, json!(thread), None),
        Err(error) => conversation_error(error),
    }
}

pub async fn delete_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    match state.conversations.delete_thread(&thread_id).await {
        Ok(()) => json_response(StatusCode::OK, json!({"deleted":true,"id":thread_id}), None),
        Err(error) => conversation_error(error),
    }
}

pub async fn get_thread_context(
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
                "context_engine_error",
                "context engine is not initialized",
            )
        }
    };
    match engine.status(&thread_id, None).await {
        Ok(status) => json_response(StatusCode::OK, json!(status), None),
        Err(error) => context_error(error),
    }
}

pub async fn compact_thread_context(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CompactContextRequest>,
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
    match engine.compact(&thread_id, body.model.as_deref()).await {
        Ok(status) => {
            if let (Some(store), Some(snapshot)) =
                (memory_provenance_runtime::get(), status.memory.as_ref())
            {
                if let Err(error) = store.sync_snapshot(snapshot).await {
                    return memory_provenance_error(error);
                }
            }
            json_response(StatusCode::OK, json!(status), None)
        }
        Err(error) => context_error(error),
    }
}

pub async fn send_thread_message(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ThreadMessageRequest>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    let thread_context = match state.conversations.context(&thread_id).await {
        Ok(context) => context,
        Err(error) => return conversation_error(error),
    };
    let requested_model = body
        .model
        .as_deref()
        .unwrap_or(&thread_context.model)
        .to_string();
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

    let user_message = json!({"role":"user","content":body.content});
    let mut prepared = match engine
        .prepare_turn(&thread_id, &requested_model, &user_message)
        .await
    {
        Ok(prepared) => prepared,
        Err(error) => return context_error(error),
    };

    let mut pinned_memory = false;
    if let Some(store) = memory_provenance_runtime::get() {
        if prepared.checkpoint.is_some() {
            match engine.memory_snapshot(&thread_id).await {
                Ok(Some(snapshot)) => {
                    if let Err(error) = store.sync_snapshot(&snapshot).await {
                        return memory_provenance_error(error);
                    }
                }
                Ok(None) => {}
                Err(error) => return context_error(error),
            }
        }
        match store.pinned_prompt(&thread_id).await {
            Ok(Some(prompt)) => {
                prepared.estimated_prepared_tokens = inject_pinned_memory(
                    &mut prepared.messages,
                    &prompt,
                    prepared.budget_tokens,
                );
                pinned_memory = true;
            }
            Ok(None) => {}
            Err(error) => return memory_provenance_error(error),
        }
    }

    let mut retrieved_chunks = 0usize;
    let mut retrieval_backend = "local";
    if state.gateway.config.context.retrieval_enabled {
        if let Some(checkpoint) = prepared.checkpoint.as_ref() {
            let spare_tokens = prepared
                .budget_tokens
                .saturating_sub(prepared.estimated_prepared_tokens);
            let retrieval_budget = state
                .gateway
                .config
                .context
                .retrieval_max_tokens
                .min(spare_tokens.saturating_sub(32));
            if retrieval_budget >= 128 {
                let detail = match state.conversations.thread(&thread_id).await {
                    Ok(detail) => detail,
                    Err(error) => return conversation_error(error),
                };
                let local = || {
                    retrieve_relevant_history(
                        &detail.messages,
                        checkpoint.through_ordinal,
                        &user_message,
                        state.gateway.config.context.retrieval_max_chunks,
                        retrieval_budget,
                        state.gateway.config.context.retrieval_min_score,
                    )
                };
                let retrieval: RetrievalResult =
                    if state.gateway.config.context.retrieval_backend == "hybrid" {
                        match embedding_runtime::get() {
                            Some(retriever) => match retriever
                                .retrieve(
                                    &thread_id,
                                    &detail.messages,
                                    checkpoint.through_ordinal,
                                    &user_message,
                                    state.gateway.config.context.retrieval_max_chunks,
                                    retrieval_budget,
                                    state.gateway.config.context.retrieval_min_score,
                                )
                                .await
                            {
                                Ok(result) => {
                                    retrieval_backend = "hybrid";
                                    result
                                }
                                Err(error) => {
                                    warn!(%thread_id, %error, "hybrid retrieval failed; falling back to lexical retrieval");
                                    retrieval_backend = "local-fallback";
                                    local()
                                }
                            },
                            None => {
                                retrieval_backend = "local-fallback";
                                local()
                            }
                        }
                    } else {
                        local()
                    };

                if !retrieval.chunks.is_empty() {
                    let mut augmented = prepared.messages.clone();
                    augment_messages_with_retrieval(&mut augmented, &retrieval);
                    let augmented_tokens = estimate_json_messages_tokens(&augmented);
                    if augmented_tokens <= prepared.budget_tokens {
                        retrieved_chunks = retrieval.chunks.len();
                        prepared.messages = augmented;
                        prepared.estimated_prepared_tokens = augmented_tokens;
                    }
                }
            }
        }
    }

    let request = json!({
        "model":requested_model,
        "messages":prepared.messages,
        "stream":body.stream
    });

    let routed = match state
        .gateway
        .execute_openai_chat_with_affinity(
            &requested_model,
            &request,
            thread_context.sticky_route.as_deref(),
        )
        .await
    {
        Ok(routed) => routed,
        Err(error) => return gateway_error(error),
    };
    let route_id = routed.route.id.clone();

    if let Err(error) = state
        .conversations
        .append_message(&thread_id, &user_message, Some(&requested_model), Some(&route_id))
        .await
    {
        return conversation_error(error);
    }
    if let Err(error) = state
        .conversations
        .update_thread_route_and_model(&thread_id, &route_id, &requested_model)
        .await
    {
        return conversation_error(error);
    }

    if body.stream {
        let response = state.gateway.trace_stream_response(
            routed.response,
            routed.request_id.clone(),
            routed.route.id.clone(),
            routed.started_at,
        );
        let (tx, rx) = oneshot::channel();
        let stream = openai_stream_with_capture(response, tx);
        let conversations = state.conversations.clone();
        let thread_id_for_task = thread_id.clone();
        let model_for_task = requested_model.clone();
        let route_for_task = route_id.clone();
        tokio::spawn(async move {
            if let Ok(assistant) = rx.await {
                let _ = conversations
                    .append_message(
                        &thread_id_for_task,
                        &assistant,
                        Some(&model_for_task),
                        Some(&route_for_task),
                    )
                    .await;
            }
        });
        let response = response_with_route(
            StatusCode::OK,
            "text/event-stream",
            Body::from_stream(stream),
            &route_id,
        );
        return with_context_headers(
            response,
            &prepared,
            retrieved_chunks,
            retrieval_backend,
            pinned_memory,
        );
    }

    match routed.response.json::<Value>().await {
        Ok(openai) => {
            if let Some(assistant) = openai
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|choices| choices.first())
                .and_then(|choice| choice.get("message"))
                .cloned()
            {
                if let Err(error) = state
                    .conversations
                    .append_message(
                        &thread_id,
                        &assistant,
                        Some(&requested_model),
                        Some(&route_id),
                    )
                    .await
                {
                    return conversation_error(error);
                }
            }
            let response = json_response(StatusCode::OK, openai, Some(&route_id));
            with_context_headers(
                response,
                &prepared,
                retrieved_chunks,
                retrieval_backend,
                pinned_memory,
            )
        }
        Err(error) => gateway_error(GatewayError::Transport(error.to_string())),
    }
}

fn with_context_headers(
    mut response: Response<Body>,
    prepared: &PreparedContext,
    retrieved_chunks: usize,
    retrieval_backend: &str,
    pinned_memory: bool,
) -> Response<Body> {
    let state = if prepared.compressed { "compressed" } else { "full" };
    if let Ok(value) = HeaderValue::from_str(state) {
        response.headers_mut().insert("x-llmgateway-context", value);
    }
    if let Ok(value) = HeaderValue::from_str(&prepared.estimated_source_tokens.to_string()) {
        response
            .headers_mut()
            .insert("x-llmgateway-context-source-tokens", value);
    }
    if let Ok(value) = HeaderValue::from_str(&prepared.estimated_prepared_tokens.to_string()) {
        response
            .headers_mut()
            .insert("x-llmgateway-context-tokens", value);
    }
    if let Ok(value) = HeaderValue::from_str(&prepared.budget_tokens.to_string()) {
        response
            .headers_mut()
            .insert("x-llmgateway-context-budget", value);
    }
    if let Ok(value) = HeaderValue::from_str(&retrieved_chunks.to_string()) {
        response
            .headers_mut()
            .insert("x-llmgateway-retrieved-chunks", value);
    }
    if let Ok(value) = HeaderValue::from_str(retrieval_backend) {
        response
            .headers_mut()
            .insert("x-llmgateway-retrieval-backend", value);
    }
    response.headers_mut().insert(
        "x-llmgateway-pinned-memory",
        HeaderValue::from_static(if pinned_memory { "yes" } else { "no" }),
    );
    if let Some(checkpoint) = &prepared.checkpoint {
        if let Ok(value) = HeaderValue::from_str(&checkpoint.id) {
            response
                .headers_mut()
                .insert("x-llmgateway-context-checkpoint", value);
        }
    }
    response
}

fn memory_provenance_error(error: MemoryProvenanceError) -> Response<Body> {
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

fn context_error(error: ContextError) -> Response<Body> {
    match error {
        ContextError::Conversation(error) => conversation_error(error),
        ContextError::Gateway(error) => gateway_error(error),
        ContextError::Database(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "context_database_error",
            &error.to_string(),
        ),
        ContextError::Io(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "context_storage_error",
            &error.to_string(),
        ),
        ContextError::InvalidJson(message) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "context_json_error",
            &message,
        ),
        ContextError::EmptySummary => json_error(
            StatusCode::BAD_GATEWAY,
            "context_summary_error",
            "summary model returned no text",
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
