use crate::{
    browser_auth::{BrowserAuthMaterial, BrowserAuthVault},
    browser_auth_runtime, browser_provider_runtime, conversation_runtime,
    browser_provider::{
        BrowserAccountBinding, BrowserAdapterDiagnostics, BrowserAdapterRequest,
        BrowserDiscoveredModel, BrowserProviderAdapter, BrowserProviderError,
        BROWSER_ADAPTER_CONTRACT_VERSION,
    },
};
use async_trait::async_trait;
use axum::http::Response as HttpResponse;
use bytes::Bytes;
use chrono::Utc;
use futures_util::StreamExt;
use reqwest::{
    header::{
        ACCEPT, ACCEPT_LANGUAGE, AUTHORIZATION, CONTENT_TYPE, COOKIE, ORIGIN, REFERER, USER_AGENT,
    },
    Client, RequestBuilder, Response,
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use uuid::Uuid;

const QWEN_HOST: &str = "chat.qwen.ai";
const QWEN_BASE_URL: &str = "https://chat.qwen.ai";
const QWEN_MODELS_URL: &str = "https://chat.qwen.ai/api/models";
const QWEN_MODELS_V2_URL: &str = "https://chat.qwen.ai/api/v2/models";
const QWEN_CHATS_NEW_URL: &str = "https://chat.qwen.ai/api/v2/chats/new";
const QWEN_COMPLETIONS_URL: &str = "https://chat.qwen.ai/api/v2/chat/completions";
const QWEN_DEFAULT_SPA_VERSION: &str = "0.2.83";
const QWEN_ADAPTER_VERSION: &str = "experimental-1";

#[derive(Clone)]
pub struct QwenWebHttpAdapter {
    client: Client,
}

#[derive(Clone, Debug)]
struct QwenNativeConversation {
    chat_id: String,
    response_id: String,
}

#[derive(Clone, Debug, Default)]
struct QwenFrameUpdate {
    response_id: Option<String>,
    parent_id: Option<String>,
    content: String,
    completed: bool,
    done: bool,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct QwenSseDecoder {
    buffer: String,
}

#[derive(Debug, Default)]
struct QwenStreamState {
    text: String,
    response_id: String,
    parent_id: Option<String>,
    completed: bool,
    done: bool,
}

impl QwenStreamState {
    fn apply(&mut self, update: QwenFrameUpdate) -> Result<String, String> {
        if let Some(error) = update.error {
            return Err(error);
        }
        if let Some(response_id) = update.response_id.filter(|value| !value.trim().is_empty()) {
            self.response_id = response_id;
        }
        if let Some(parent_id) = update.parent_id.filter(|value| !value.trim().is_empty()) {
            self.parent_id = Some(parent_id);
        }
        self.completed |= update.completed;
        self.done |= update.done;
        if !update.content.is_empty() {
            self.text.push_str(&update.content);
        }
        Ok(update.content)
    }

    fn validate_completion(&self) -> Result<(), String> {
        if !(self.completed || self.done) {
            return Err("Qwen direct stream ended before logical completion".into());
        }
        if self.text.trim().is_empty() {
            return Err("Qwen direct stream completed without assistant output".into());
        }
        if self.response_id.trim().is_empty() {
            return Err("Qwen direct stream completed without response_id".into());
        }
        Ok(())
    }
}

impl QwenSseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<QwenFrameUpdate> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        self.buffer = self.buffer.replace("\r\n", "\n");
        let mut updates = Vec::new();
        while let Some(index) = self.buffer.find("\n\n") {
            let frame = self.buffer[..index].to_string();
            self.buffer.drain(..index + 2);
            if let Some(update) = parse_sse_frame(&frame) {
                updates.push(update);
            }
        }
        updates
    }

    fn finish(&mut self) -> Vec<QwenFrameUpdate> {
        let frame = self.buffer.trim().to_string();
        self.buffer.clear();
        if frame.is_empty() {
            Vec::new()
        } else {
            parse_sse_frame(&frame).into_iter().collect()
        }
    }
}

impl QwenWebHttpAdapter {
    pub fn new() -> Result<Self, BrowserProviderError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        Ok(Self { client })
    }

    fn vault() -> Result<&'static Arc<BrowserAuthVault>, BrowserProviderError> {
        browser_auth_runtime::get().ok_or_else(|| {
            BrowserProviderError::InvalidConfig(
                "browser auth vault runtime is not initialized".into(),
            )
        })
    }

    fn auth_material(
        &self,
        session_id: &str,
        account_id: &str,
    ) -> Result<BrowserAuthMaterial, BrowserProviderError> {
        let material = Self::vault()?
            .load(session_id)
            .map_err(|error| BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "login_required".into(),
                message: format!(
                    "Qwen browserless auth material is unavailable; login with browser again: {error}"
                ),
            })?;
        let cookies = material.cookie_header_for_host(QWEN_HOST);
        let token = qwen_token(&material);
        if cookies.trim().is_empty() && token.is_none() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "login_required".into(),
                message:
                    "Qwen auth snapshot has neither chat.qwen.ai cookies nor a captured session token"
                        .into(),
            });
        }
        Ok(material)
    }

    fn apply_common_headers(
        &self,
        mut builder: RequestBuilder,
        material: &BrowserAuthMaterial,
        referer: &str,
        json_body: bool,
    ) -> RequestBuilder {
        builder = builder
            .header(ACCEPT, "application/json, text/plain, */*")
            .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .header(ORIGIN, QWEN_BASE_URL)
            .header(REFERER, referer)
            .header("source", "web")
            .header("version", qwen_spa_version(material))
            .header("x-request-id", Uuid::new_v4().to_string());

        if json_body {
            builder = builder.header(CONTENT_TYPE, "application/json");
        }
        if !material.user_agent.trim().is_empty() {
            builder = builder.header(USER_AGENT, material.user_agent.trim());
        }
        let cookies = material.cookie_header_for_host(QWEN_HOST);
        if !cookies.is_empty() {
            builder = builder.header(COOKIE, cookies);
        }
        if let Some(token) = qwen_token(material) {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }

        // Replay anti-bot/session headers only when the authenticated browser snapshot
        // already contains them. Never synthesize or refresh WAF material here.
        for (header, keys) in [
            ("bx-ua", &["bx-ua", "bx_ua"][..]),
            (
                "bx-umidtoken",
                &["bx-umidtoken", "bx_umidtoken", "bxUmidToken"][..],
            ),
            ("bx-v", &["bx-v", "bx_v", "bxV"][..]),
        ] {
            if let Some(value) = storage_value(material, keys) {
                builder = builder.header(header, value);
            }
        }
        builder
    }

    async fn discover_models_impl(
        &self,
        account_id: &str,
        binding: &BrowserAccountBinding,
    ) -> Result<Vec<BrowserDiscoveredModel>, BrowserProviderError> {
        let material = self.auth_material(&binding.session, account_id)?;
        let mut attempts = Vec::new();
        let mut saw_waf = false;

        for url in [QWEN_MODELS_URL, QWEN_MODELS_V2_URL] {
            let response = self
                .apply_common_headers(self.client.get(url), &material, QWEN_BASE_URL, false)
                .timeout(Duration::from_millis(
                    binding.probe_timeout_ms.unwrap_or(8_000).max(3_000),
                ))
                .send()
                .await
                .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
            let status = response.status();
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            let bytes = response
                .bytes()
                .await
                .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
            let body = String::from_utf8_lossy(&bytes);

            if status.as_u16() == 401 {
                return Err(BrowserProviderError::AdapterIncompatible {
                    account_id: account_id.to_string(),
                    code: "login_required".into(),
                    message: format!(
                        "Qwen model discovery returned HTTP 401 from {url}; login with browser again"
                    ),
                });
            }

            if status.is_success() && !is_waf_response(status.as_u16(), &content_type, &body) {
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    let models = parse_model_catalog(&value);
                    if !models.is_empty() {
                        return Ok(models);
                    }
                }
            }

            let waf = is_waf_response(status.as_u16(), &content_type, &body);
            saw_waf |= waf;
            attempts.push(format!(
                "{} => HTTP {} classification={} body={}",
                url,
                status.as_u16(),
                if waf { "waf" } else { "incompatible" },
                body_preview(&body)
            ));
        }

        let detail = attempts.join(" | ");
        if saw_waf {
            Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "upstream_waf_rejected".into(),
                message: format!(
                    "Qwen direct model discovery was rejected by upstream/WAF; compatibility probes exhausted: {detail}"
                ),
            })
        } else {
            Err(BrowserProviderError::Transport(format!(
                "Qwen direct model discovery returned no usable catalog: {detail}"
            )))
        }
    }

    async fn native_conversation(
        &self,
        request: &BrowserAdapterRequest,
    ) -> Result<Option<QwenNativeConversation>, BrowserProviderError> {
        let Some(thread_id) = request.thread_id.as_deref() else {
            return Ok(None);
        };
        let Some(store) = conversation_runtime::get() else {
            return Ok(None);
        };
        let state = store
            .provider_conversation_state(thread_id, &request.provider.id, &request.account.id)
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;

        if state
            .as_ref()
            .and_then(|value| value.get("needs_resync"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "direct_state_unsynced".into(),
                message: "Qwen direct conversation state requires resync after browser fallback"
                    .into(),
            });
        }

        validate_thread_model_affinity(state.as_ref(), &request.route.model).map_err(|message| {
            // Keep this as Transport. Gateway model-binding conflict handling recognizes
            // the message and makes it non-retryable with no cooldown/fallback.
            BrowserProviderError::Transport(message)
        })?;

        let Some(state) = state else {
            if store
                .provider_conversation(thread_id, &request.provider.id, &request.account.id)
                .await
                .map_err(|error| BrowserProviderError::Transport(error.to_string()))?
                .is_some()
            {
                return Err(BrowserProviderError::AdapterIncompatible {
                    account_id: request.account.id.clone(),
                    code: "direct_state_unsynced".into(),
                    message:
                        "Qwen provider conversation exists without direct HTTP state; browser recovery is required"
                            .into(),
                });
            }
            return Ok(None);
        };

        let chat_id = state
            .get("chat_id")
            .or_else(|| state.get("conversation_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let response_id = state
            .get("next_parent_id")
            .or_else(|| state.get("response_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if chat_id.is_empty() || response_id.is_empty() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "direct_state_unsynced".into(),
                message: "Qwen direct conversation state is missing chat_id/response_id".into(),
            });
        }
        Ok(Some(QwenNativeConversation {
            chat_id,
            response_id,
        }))
    }

    async fn create_chat(
        &self,
        request: &BrowserAdapterRequest,
        material: &BrowserAuthMaterial,
    ) -> Result<String, BrowserProviderError> {
        let body = json!({
            "title": "New Chat",
            "models": [request.route.model],
            "chat_mode": "normal",
            "chat_type": "t2t",
            "timestamp": Utc::now().timestamp_millis()
        });
        let response = self
            .apply_common_headers(
                self.client.post(QWEN_CHATS_NEW_URL),
                material,
                QWEN_BASE_URL,
                true,
            )
            .timeout(Duration::from_millis(
                request.binding.response_timeout_ms.unwrap_or(120_000),
            ))
            .json(&body)
            .send()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let raw = String::from_utf8_lossy(&bytes);

        if status.as_u16() == 401 {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "login_required".into(),
                message: "Qwen create-chat returned HTTP 401; login with browser again".into(),
            });
        }
        if is_waf_response(status.as_u16(), &content_type, &raw) || status.as_u16() == 403 {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "upstream_waf_rejected".into(),
                message: format!(
                    "Qwen create-chat rejected direct HTTP: HTTP {} classification=waf body={}",
                    status.as_u16(),
                    body_preview(&raw)
                ),
            });
        }
        if !status.is_success() {
            return Err(BrowserProviderError::Transport(format!(
                "Qwen create-chat failed: HTTP {} body={}",
                status.as_u16(),
                body_preview(&raw)
            )));
        }

        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            BrowserProviderError::Transport(format!(
                "Qwen create-chat returned invalid JSON: {error}; body={}",
                body_preview(&raw)
            ))
        })?;
        if value.get("success").and_then(Value::as_bool) == Some(false) {
            let detail = body_preview(&raw);
            if looks_like_waf_body(&raw) {
                return Err(BrowserProviderError::AdapterIncompatible {
                    account_id: request.account.id.clone(),
                    code: "upstream_waf_rejected".into(),
                    message: format!(
                        "Qwen create-chat returned upstream risk-control payload: HTTP {} classification=waf body={detail}",
                        status.as_u16()
                    ),
                });
            }
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "upstream_rejected".into(),
                message: format!(
                    "Qwen create-chat was rejected before generation: HTTP {} body={detail}",
                    status.as_u16()
                ),
            });
        }
        let chat_id = value
            .pointer("/data/id")
            .or_else(|| value.pointer("/data/chat_id"))
            .or_else(|| value.get("chat_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if chat_id.is_empty() {
            return Err(BrowserProviderError::Transport(format!(
                "Qwen create-chat returned no chat id: body={}",
                body_preview(&raw)
            )));
        }
        Ok(chat_id.to_string())
    }

    async fn submit_completion(
        &self,
        request: &BrowserAdapterRequest,
        material: &BrowserAuthMaterial,
        chat_id: &str,
        parent_id: Option<&str>,
        prompt: &str,
    ) -> Result<Response, BrowserProviderError> {
        let body = build_chat_payload(chat_id, &request.route.model, parent_id, prompt);
        let url = format!("{QWEN_COMPLETIONS_URL}?chat_id={chat_id}");
        let referer = format!("{QWEN_BASE_URL}/c/{chat_id}");
        let response = self
            .apply_common_headers(self.client.post(&url), material, &referer, true)
            .header("x-accel-buffering", "no")
            .timeout(Duration::from_millis(
                request.binding.response_timeout_ms.unwrap_or(120_000),
            ))
            .json(&body)
            .send()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;

        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        if status.as_u16() == 401 {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "login_required".into(),
                message: "Qwen completion returned HTTP 401; login with browser again".into(),
            });
        }
        if !status.is_success() || !content_type.to_ascii_lowercase().contains("text/event-stream")
        {
            let bytes = response
                .bytes()
                .await
                .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
            let raw = String::from_utf8_lossy(&bytes);
            if is_waf_response(status.as_u16(), &content_type, &raw) || status.as_u16() == 403 {
                return Err(BrowserProviderError::AdapterIncompatible {
                    account_id: request.account.id.clone(),
                    code: "upstream_waf_rejected".into(),
                    message: format!(
                        "Qwen completion rejected direct HTTP before SSE: HTTP {} classification=waf body={}",
                        status.as_u16(),
                        body_preview(&raw)
                    ),
                });
            }
            return Err(BrowserProviderError::Transport(format!(
                "Qwen completion returned incompatible upstream response: HTTP {} content-type={} body={}",
                status.as_u16(),
                content_type,
                body_preview(&raw)
            )));
        }
        Ok(response)
    }

    async fn persist_conversation_state(
        request: &BrowserAdapterRequest,
        chat_id: &str,
        request_parent_id: Option<&str>,
        stream_state: &QwenStreamState,
    ) -> Result<(), BrowserProviderError> {
        let Some(thread_id) = request.thread_id.as_deref() else {
            return Ok(());
        };
        let Some(store) = conversation_runtime::get() else {
            return Ok(());
        };
        let conversation_url = format!("{QWEN_BASE_URL}/c/{chat_id}");
        store
            .upsert_provider_conversation(
                thread_id,
                &request.provider.id,
                &request.account.id,
                &conversation_url,
            )
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        store
            .upsert_provider_conversation_state(
                thread_id,
                &request.provider.id,
                &request.account.id,
                &json!({
                    "transport": "qwen-http",
                    "model_external_id": request.route.model,
                    "chat_id": chat_id,
                    "conversation_id": chat_id,
                    "parent_id": stream_state.parent_id,
                    "request_parent_id": request_parent_id,
                    "response_id": stream_state.response_id,
                    "next_parent_id": stream_state.response_id
                }),
            )
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))
    }

    async fn buffered_response(
        &self,
        request: &BrowserAdapterRequest,
        response: Response,
        chat_id: String,
        request_parent_id: Option<String>,
    ) -> Result<Response, BrowserProviderError> {
        let mut decoder = QwenSseDecoder::default();
        let mut state = QwenStreamState::default();
        let mut upstream = response.bytes_stream();
        while let Some(chunk) = upstream.next().await {
            let chunk = chunk.map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
            for update in decoder.push(&chunk) {
                state
                    .apply(update)
                    .map_err(|error| BrowserProviderError::Transport(format!("Qwen SSE error: {error}")))?;
            }
        }
        for update in decoder.finish() {
            state
                .apply(update)
                .map_err(|error| BrowserProviderError::Transport(format!("Qwen SSE error: {error}")))?;
        }
        state
            .validate_completion()
            .map_err(BrowserProviderError::Transport)?;
        Self::persist_conversation_state(
            request,
            &chat_id,
            request_parent_id.as_deref(),
            &state,
        )
        .await?;

        let body = json!({
            "id": format!("chatcmpl-{}", Uuid::new_v4()),
            "object": "chat.completion",
            "created": Utc::now().timestamp(),
            "model": request.route.model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": state.text},
                "finish_reason": "stop"
            }]
        });
        let response = HttpResponse::builder()
            .status(200)
            .header(CONTENT_TYPE, "application/json")
            .body(reqwest::Body::from(body.to_string()))
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        Ok(reqwest::Response::from(response))
    }

    fn streaming_response(
        &self,
        request: &BrowserAdapterRequest,
        response: Response,
        chat_id: String,
        request_parent_id: Option<String>,
    ) -> Result<Response, BrowserProviderError> {
        let request = request.clone();
        let model = request.route.model.clone();
        let completion_id = format!("chatcmpl-{}", Uuid::new_v4());
        let created = Utc::now().timestamp();
        let mut upstream = response.bytes_stream();

        let stream = async_stream::stream! {
            let mut decoder = QwenSseDecoder::default();
            let mut state = QwenStreamState::default();
            let mut emitted_role = false;

            while let Some(chunk) = upstream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        yield Err(std::io::Error::other(format!("Qwen direct stream body error: {error}")));
                        return;
                    }
                };
                for update in decoder.push(&chunk) {
                    let delta = match state.apply(update) {
                        Ok(delta) => delta,
                        Err(error) => {
                            yield Err(std::io::Error::other(format!("Qwen SSE error: {error}")));
                            return;
                        }
                    };
                    if !delta.is_empty() {
                        let event = if emitted_role {
                            json!({
                                "id": completion_id,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model,
                                "choices": [{"index": 0, "delta": {"content": delta}, "finish_reason": Value::Null}]
                            })
                        } else {
                            emitted_role = true;
                            json!({
                                "id": completion_id,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model,
                                "choices": [{"index": 0, "delta": {"role": "assistant", "content": delta}, "finish_reason": Value::Null}]
                            })
                        };
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("data: {event}\n\n")));
                    }
                }
            }

            for update in decoder.finish() {
                let delta = match state.apply(update) {
                    Ok(delta) => delta,
                    Err(error) => {
                        yield Err(std::io::Error::other(format!("Qwen SSE error: {error}")));
                        return;
                    }
                };
                if !delta.is_empty() {
                    let event = if emitted_role {
                        json!({
                            "id": completion_id,
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{"index": 0, "delta": {"content": delta}, "finish_reason": Value::Null}]
                        })
                    } else {
                        emitted_role = true;
                        json!({
                            "id": completion_id,
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{"index": 0, "delta": {"role": "assistant", "content": delta}, "finish_reason": Value::Null}]
                        })
                    };
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("data: {event}\n\n")));
                }
            }

            if let Err(error) = state.validate_completion() {
                yield Err(std::io::Error::other(error));
                return;
            }
            if let Err(error) = Self::persist_conversation_state(
                &request,
                &chat_id,
                request_parent_id.as_deref(),
                &state,
            ).await {
                yield Err(std::io::Error::other(error.to_string()));
                return;
            }

            let final_event = json!({
                "id": completion_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
            });
            yield Ok(Bytes::from(format!("data: {final_event}\n\n")));
            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        };

        let response = HttpResponse::builder()
            .status(200)
            .header(CONTENT_TYPE, "text/event-stream")
            .body(reqwest::Body::wrap_stream(stream))
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        Ok(reqwest::Response::from(response))
    }
}

#[async_trait]
impl BrowserProviderAdapter for QwenWebHttpAdapter {
    fn kind(&self) -> &'static str {
        "browser-qwen-http"
    }

    fn adapter_id(&self) -> &'static str {
        "qwen-web-http"
    }

    fn supports_model_discovery(&self) -> bool {
        true
    }

    async fn discover_models(
        &self,
        account_id: &str,
        binding: &BrowserAccountBinding,
        _force: bool,
    ) -> Result<Vec<BrowserDiscoveredModel>, BrowserProviderError> {
        self.discover_models_impl(account_id, binding).await
    }

    async fn diagnose(
        &self,
        account_id: &str,
        _profile_dir: &str,
        binding: &BrowserAccountBinding,
    ) -> BrowserAdapterDiagnostics {
        match self.auth_material(&binding.session, account_id) {
            Ok(_) => BrowserAdapterDiagnostics {
                account_id: account_id.to_string(),
                provider_kind: "browser-qwen".into(),
                adapter_id: Some(self.adapter_id().into()),
                adapter_version: Some(QWEN_ADAPTER_VERSION.into()),
                contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
                expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                status: "ready".into(),
                message:
                    "Qwen direct HTTP auth snapshot is ready; Chromium is not required for chat"
                        .into(),
                page_signature: None,
                target_url_prefix: Some(QWEN_BASE_URL.into()),
                configured_models: binding.models.clone(),
            },
            Err(BrowserProviderError::AdapterIncompatible { code, message, .. }) => {
                BrowserAdapterDiagnostics {
                    account_id: account_id.to_string(),
                    provider_kind: "browser-qwen".into(),
                    adapter_id: Some(self.adapter_id().into()),
                    adapter_version: Some(QWEN_ADAPTER_VERSION.into()),
                    contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
                    expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                    status: if code == "login_required" {
                        "login_required".into()
                    } else {
                        "adapter_incompatible".into()
                    },
                    message,
                    page_signature: None,
                    target_url_prefix: Some(QWEN_BASE_URL.into()),
                    configured_models: binding.models.clone(),
                }
            }
            Err(error) => BrowserAdapterDiagnostics {
                account_id: account_id.to_string(),
                provider_kind: "browser-qwen".into(),
                adapter_id: Some(self.adapter_id().into()),
                adapter_version: Some(QWEN_ADAPTER_VERSION.into()),
                contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
                expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                status: "unavailable".into(),
                message: error.to_string(),
                page_signature: None,
                target_url_prefix: Some(QWEN_BASE_URL.into()),
                configured_models: binding.models.clone(),
            },
        }
    }

    async fn execute_chat(
        &self,
        request: BrowserAdapterRequest,
    ) -> Result<Response, BrowserProviderError> {
        let material = self.auth_material(&request.session_id, &request.account.id)?;
        let native = self.native_conversation(&request).await?;
        let prompt = serialize_prompt(&request.body, native.is_some())?;
        let (chat_id, request_parent_id) = if let Some(native) = native {
            (native.chat_id, Some(native.response_id))
        } else {
            (self.create_chat(&request, &material).await?, None)
        };
        let upstream = self
            .submit_completion(
                &request,
                &material,
                &chat_id,
                request_parent_id.as_deref(),
                &prompt,
            )
            .await?;

        if request
            .body
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            self.streaming_response(&request, upstream, chat_id, request_parent_id)
        } else {
            self.buffered_response(&request, upstream, chat_id, request_parent_id)
                .await
        }
    }
}

fn storage_value(material: &BrowserAuthMaterial, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = material
            .local_storage
            .get(*key)
            .or_else(|| material.session_storage.get(*key))
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_string());
        }
    }
    None
}

fn qwen_token(material: &BrowserAuthMaterial) -> Option<String> {
    let raw = storage_value(
        material,
        &[
            "token",
            "access_token",
            "accessToken",
            "auth_token",
            "authToken",
        ],
    )
    .or_else(|| {
        material
            .cookie_value_for_host(QWEN_HOST, "token")
            .map(str::to_string)
    })?;
    normalize_token(&raw)
}

fn normalize_token(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(value) {
        if let Some(token) = parsed.as_str() {
            return normalize_token(token);
        }
        for key in ["token", "access_token", "accessToken"] {
            if let Some(token) = parsed.get(key).and_then(Value::as_str) {
                return normalize_token(token);
            }
        }
    }
    let token = value
        .strip_prefix("Bearer ")
        .unwrap_or(value)
        .trim_matches('"')
        .trim();
    (!token.is_empty()).then(|| token.to_string())
}

fn qwen_spa_version(material: &BrowserAuthMaterial) -> String {
    storage_value(
        material,
        &[
            "version",
            "app_version",
            "appVersion",
            "spa_version",
            "qwen_version",
        ],
    )
    .filter(|value| value.len() <= 32 && value.chars().any(|ch| ch.is_ascii_digit()))
    .unwrap_or_else(|| QWEN_DEFAULT_SPA_VERSION.to_string())
}

fn parse_model_catalog(value: &Value) -> Vec<BrowserDiscoveredModel> {
    let arrays = [
        value.as_array(),
        value.get("models").and_then(Value::as_array),
        value.get("data").and_then(Value::as_array),
        value.pointer("/data/models").and_then(Value::as_array),
        value.pointer("/data/data").and_then(Value::as_array),
        value.pointer("/data/data/models").and_then(Value::as_array),
        value.pointer("/data/data/data").and_then(Value::as_array),
    ];
    let mut models = Vec::new();
    let mut seen = BTreeSet::new();
    for items in arrays.into_iter().flatten() {
        for item in items {
            let external_id = item
                .get("id")
                .or_else(|| item.get("model"))
                .or_else(|| item.get("model_id"))
                .or_else(|| item.get("slug"))
                .or_else(|| item.pointer("/info/id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if external_id.is_empty() || !seen.insert(external_id.to_string()) {
                continue;
            }
            let display_name = item
                .get("display_name")
                .or_else(|| item.get("displayName"))
                .or_else(|| item.get("name"))
                .or_else(|| item.get("label"))
                .or_else(|| item.pointer("/info/name"))
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(external_id)
                .to_string();
            models.push(BrowserDiscoveredModel {
                external_id: external_id.to_string(),
                display_name,
                owned_by: "Qwen".into(),
                context_window: item
                    .get("context_window")
                    .or_else(|| item.get("contextWindow"))
                    .and_then(Value::as_i64),
                capabilities: vec!["chat".into(), "streaming".into()],
            });
        }
        if !models.is_empty() {
            break;
        }
    }
    models
}

fn serialize_prompt(body: &Value, native: bool) -> Result<String, BrowserProviderError> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| BrowserProviderError::InvalidConfig("Qwen request requires messages".into()))?;
    if native {
        for message in messages.iter().rev() {
            if message.get("role").and_then(Value::as_str) == Some("user") {
                let text = message_text(message.get("content").unwrap_or(&Value::Null));
                if !text.trim().is_empty() {
                    return Ok(text);
                }
            }
        }
        return Err(BrowserProviderError::InvalidConfig(
            "Qwen native continuation requires a non-empty user message".into(),
        ));
    }

    let mut rendered = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let text = message_text(message.get("content").unwrap_or(&Value::Null));
        if text.trim().is_empty() {
            continue;
        }
        let label = match role {
            "system" => "System",
            "assistant" => "Assistant",
            _ => "User",
        };
        rendered.push(format!("[{label}]\n{text}"));
    }
    let prompt = rendered.join("\n\n");
    if prompt.trim().is_empty() {
        Err(BrowserProviderError::InvalidConfig(
            "Qwen request has no text content".into(),
        ))
    } else {
        Ok(prompt)
    }
}

fn message_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                if let Some(text) = part.as_str() {
                    return Some(text.to_string());
                }
                part.get("text")
                    .or_else(|| part.get("input_text"))
                    .or_else(|| part.get("content"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => content
            .get("text")
            .or_else(|| content.get("input_text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn build_chat_payload(
    chat_id: &str,
    model: &str,
    parent_id: Option<&str>,
    prompt: &str,
) -> Value {
    let fid = Uuid::new_v4().to_string();
    let child_id = Uuid::new_v4().to_string();
    let timestamp = Utc::now().timestamp_millis();
    json!({
        "stream": true,
        "version": "2.1",
        "incremental_output": true,
        "chat_id": chat_id,
        "chat_mode": "normal",
        "model": model,
        "parent_id": parent_id,
        "messages": [{
            "id": Value::Null,
            "fid": fid,
            "parentId": parent_id,
            "childrenIds": [child_id],
            "role": "user",
            "content": prompt,
            "user_action": "chat",
            "files": [],
            "timestamp": timestamp,
            "models": [model],
            "model": "",
            "chat_type": "t2t",
            "feature_config": {
                "thinking_enabled": true,
                "output_schema": "phase",
                "research_mode": "normal",
                "auto_thinking": true,
                "thinking_mode": "Auto",
                "thinking_format": "summary",
                "auto_search": false
            },
            "extra": {"meta": {"subChatType": "t2t"}},
            "sub_chat_type": "t2t",
            "parent_id": parent_id
        }],
        "timestamp": timestamp
    })
}

fn parse_sse_frame(frame: &str) -> Option<QwenFrameUpdate> {
    let data = frame
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.trim().is_empty() {
        return None;
    }
    if data.trim() == "[DONE]" {
        return Some(QwenFrameUpdate {
            done: true,
            ..QwenFrameUpdate::default()
        });
    }

    let value: Value = serde_json::from_str(&data).ok()?;
    if let Some(error) = value.get("error") {
        return Some(QwenFrameUpdate {
            error: Some(error_message(error)),
            ..QwenFrameUpdate::default()
        });
    }
    if value.get("success").and_then(Value::as_bool) == Some(false) {
        return Some(QwenFrameUpdate {
            error: Some(format!("upstream rejected stream: {}", body_preview(&data))),
            ..QwenFrameUpdate::default()
        });
    }

    let created = value.get("response.created");
    let mut update = QwenFrameUpdate {
        response_id: created
            .and_then(|value| value.get("response_id"))
            .or_else(|| value.get("response_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        parent_id: created
            .and_then(|value| value.get("parent_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        ..QwenFrameUpdate::default()
    };

    if let Some(choice) = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    {
        let finish_reason = choice.get("finish_reason").filter(|value| !value.is_null());
        let delta = choice.get("delta").unwrap_or(&Value::Null);
        let phase = delta.get("phase").and_then(Value::as_str).unwrap_or("");
        let status = delta.get("status").and_then(Value::as_str).unwrap_or("");
        let answer_phase = phase.is_empty() || phase == "answer";
        if answer_phase {
            update.content = delta
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        update.completed = finish_reason.is_some() || (answer_phase && status == "finished");
    }
    Some(update)
}

fn error_message(error: &Value) -> String {
    if let Some(text) = error.as_str() {
        return text.to_string();
    }
    error
        .get("details")
        .or_else(|| error.get("message"))
        .or_else(|| error.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("Qwen upstream stream error")
        .to_string()
}

fn validate_thread_model_affinity(state: Option<&Value>, requested_model: &str) -> Result<(), String> {
    let Some(existing_model) = state
        .and_then(|value| value.get("model_external_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(());
    };
    if existing_model == requested_model {
        return Ok(());
    }
    Err(format!(
        "Qwen native conversation is already bound to model '{existing_model}'; start a new llmgateway thread to use model '{requested_model}'"
    ))
}

fn looks_like_waf_body(body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    [
        "aliyun_waf",
        "baxia",
        "rgv587",
        "punish",
        "action=deny",
        "puredenywait",
    ]
    .iter()
    .any(|needle| body.contains(needle))
}

fn is_waf_response(status: u16, content_type: &str, body: &str) -> bool {
    status == 403
        || content_type.to_ascii_lowercase().contains("text/html")
        || looks_like_waf_body(body)
}

fn body_preview(body: &str) -> String {
    let normalized = body.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_qwen_model_catalogs() {
        let models = parse_model_catalog(&json!({
            "data": {
                "data": [
                    {"id": "qwen3.8-max", "name": "Qwen 3.8 Max"},
                    {"id": "qwen3-coder-plus", "display_name": "Qwen3 Coder Plus"}
                ]
            }
        }));
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].external_id, "qwen3.8-max");
        assert_eq!(models[1].display_name, "Qwen3 Coder Plus");
    }

    #[test]
    fn qwen_model_switch_uses_gateway_binding_conflict_contract() {
        let state = json!({"model_external_id": "model-a"});
        let error = validate_thread_model_affinity(Some(&state), "model-b").unwrap_err();
        assert!(error.contains("native conversation is already bound to model"));
        assert!(error.contains("model-a"));
        assert!(error.contains("model-b"));
    }

    #[test]
    fn parses_qwen_response_ids_and_logical_answer_completion() {
        let created = parse_sse_frame(
            r#"data: {"response.created":{"chat_id":"chat-a","parent_id":"parent-a","response_id":"response-a"}}"#,
        )
        .unwrap();
        assert_eq!(created.response_id.as_deref(), Some("response-a"));
        assert_eq!(created.parent_id.as_deref(), Some("parent-a"));
        assert!(!created.completed);

        let thinking = parse_sse_frame(
            r#"data: {"choices":[{"delta":{"role":"assistant","content":"","phase":"thinking_summary","status":"finished"}}],"response_id":"response-a"}"#,
        )
        .unwrap();
        assert!(!thinking.completed);
        assert!(thinking.content.is_empty());

        let answer = parse_sse_frame(
            r#"data: {"choices":[{"delta":{"role":"assistant","content":"hello","phase":"answer","status":"typing"}}],"response_id":"response-a"}"#,
        )
        .unwrap();
        assert_eq!(answer.content, "hello");
        assert!(!answer.completed);

        let finished = parse_sse_frame(
            r#"data: {"choices":[{"delta":{"role":"assistant","content":"","phase":"answer","status":"finished"}}],"response_id":"response-a"}"#,
        )
        .unwrap();
        assert!(finished.completed);
    }

    #[test]
    fn continuation_payload_uses_previous_response_as_parent() {
        let payload = build_chat_payload("chat-a", "model-a", Some("response-a"), "next");
        assert_eq!(payload.get("parent_id").and_then(Value::as_str), Some("response-a"));
        assert_eq!(
            payload.pointer("/messages/0/parentId").and_then(Value::as_str),
            Some("response-a")
        );
        assert_eq!(
            payload.pointer("/messages/0/parent_id").and_then(Value::as_str),
            Some("response-a")
        );
    }

    #[test]
    fn incomplete_or_empty_stream_never_validates() {
        let mut state = QwenStreamState {
            text: "partial".into(),
            response_id: "response-a".into(),
            ..QwenStreamState::default()
        };
        assert!(state.validate_completion().is_err());
        state.done = true;
        assert!(state.validate_completion().is_ok());
        state.text.clear();
        assert!(state.validate_completion().is_err());
    }

    #[test]
    fn waf_classification_is_explicit() {
        assert!(is_waf_response(
            200,
            "application/json",
            r#"{"rgv587_flag":"sm","url":"https://example/punish?action=deny"}"#
        ));
        assert!(is_waf_response(403, "application/json", "{}"));
        assert!(!is_waf_response(200, "text/event-stream", "data: [DONE]"));
    }

    #[test]
    fn token_normalization_accepts_storage_json_and_bearer_values() {
        assert_eq!(
            normalize_token(r#"{"token":"abc"}"#).as_deref(),
            Some("abc")
        );
        assert_eq!(normalize_token("Bearer xyz").as_deref(), Some("xyz"));
    }
}
