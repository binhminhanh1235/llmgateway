use crate::{
    browser_auth::{BrowserAuthMaterial, BrowserAuthVault},
    browser_auth_runtime,
    browser_provider::{
        BrowserAccountBinding, BrowserAdapterDiagnostics, BrowserAdapterRequest,
        BrowserDiscoveredModel, BrowserProviderAdapter, BrowserProviderError, BrowserTransportMode,
        BrowserlessCapabilities, BROWSER_ADAPTER_CONTRACT_VERSION,
    },
    conversation_runtime,
};
use async_trait::async_trait;
use axum::http::Response as HttpResponse;
use bytes::Bytes;
use chrono::Utc;
use futures_util::StreamExt;
use reqwest::{
    header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, COOKIE, ORIGIN, REFERER, USER_AGENT},
    Client, RequestBuilder, Response, Url,
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use tokio::time::timeout;
use uuid::Uuid;

const MIMO_HOST: &str = "aistudio.xiaomimimo.com";
const MIMO_BASE_URL: &str = "https://aistudio.xiaomimimo.com";
const MIMO_CHAT_URL: &str = "https://aistudio.xiaomimimo.com/open-apis/bot/chat";
const MIMO_CONFIG_URL: &str = "https://aistudio.xiaomimimo.com/open-apis/bot/config";
const MIMO_ADAPTER_VERSION: &str = "experimental-1";

#[derive(Clone)]
pub struct MimoWebHttpAdapter {
    client: Client,
}

#[derive(Clone, Debug)]
struct MimoCredentials {
    material: BrowserAuthMaterial,
    ph_token: String,
}

#[derive(Clone, Debug)]
struct MimoModelSelection {
    external_id: String,
    enable_thinking: bool,
}

#[derive(Clone, Debug)]
struct MimoNativeConversation {
    conversation_id: String,
    parent_id: Option<String>,
}

#[derive(Clone, Debug)]
struct MimoModelCatalog {
    models: Vec<BrowserDiscoveredModel>,
    default_model: String,
}

#[derive(Clone, Debug, Default)]
struct MimoFrameUpdate {
    text: String,
    dialog_id: Option<String>,
    usage: Option<Value>,
    completed: bool,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct MimoSseDecoder {
    buffer: Vec<u8>,
}

#[derive(Debug, Default)]
struct MimoStreamState {
    raw_text: String,
    output: String,
    reasoning: String,
    dialog_id: Option<String>,
    usage: Option<Value>,
    completed: bool,
}

impl MimoSseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<MimoFrameUpdate>, String> {
        self.buffer.extend_from_slice(bytes);
        let mut updates = Vec::new();
        while let Some((index, separator_len)) = find_sse_separator(&self.buffer) {
            let block = self
                .buffer
                .drain(..index + separator_len)
                .collect::<Vec<_>>();
            let payload = &block[..index];
            if payload.iter().all(|byte| byte.is_ascii_whitespace()) {
                continue;
            }
            let frame = std::str::from_utf8(payload)
                .map_err(|error| format!("MiMo SSE returned invalid UTF-8: {error}"))?;
            if let Some(update) = parse_sse_frame(frame)? {
                updates.push(update);
            }
        }
        Ok(updates)
    }

    fn finish(&mut self) -> Result<Vec<MimoFrameUpdate>, String> {
        if self.buffer.iter().all(|byte| byte.is_ascii_whitespace()) {
            self.buffer.clear();
            return Ok(Vec::new());
        }
        let mut tail = self.buffer.clone();
        tail.extend_from_slice(b"\n\n");
        self.buffer.clear();
        self.push(&tail)
    }
}

impl MimoStreamState {
    fn apply(&mut self, update: MimoFrameUpdate) -> Result<(String, String), String> {
        if let Some(error) = update.error {
            return Err(error);
        }
        if let Some(dialog_id) = update.dialog_id {
            if !dialog_id.trim().is_empty() {
                self.dialog_id = Some(dialog_id);
            }
        }
        if let Some(usage) = update.usage {
            self.usage = Some(usage);
        }
        self.completed |= update.completed;

        if update.text.is_empty() {
            return Ok((String::new(), String::new()));
        }

        if update.text == self.raw_text {
            return Ok((String::new(), String::new()));
        }
        if !self.raw_text.is_empty() && update.text.starts_with(&self.raw_text) {
            self.raw_text = update.text;
        } else {
            self.raw_text.push_str(&update.text);
        }

        let (next_reasoning, next_output) = split_reasoning(&self.raw_text);
        let reasoning_delta = monotonic_delta(&self.reasoning, &next_reasoning, "reasoning")?;
        let output_delta = monotonic_delta(&self.output, &next_output, "answer")?;
        self.reasoning = next_reasoning;
        self.output = next_output;
        Ok((output_delta, reasoning_delta))
    }

    fn validate_completion(&self) -> Result<(), String> {
        if !self.completed {
            return Err("upstream_stream_dropped: MiMo stream ended before finish event".into());
        }
        if self.output.trim().is_empty() {
            return Err("MiMo direct stream completed without assistant output".into());
        }
        Ok(())
    }
}

impl MimoWebHttpAdapter {
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

    fn credentials(
        &self,
        session_id: &str,
        account_id: &str,
    ) -> Result<MimoCredentials, BrowserProviderError> {
        let material = Self::vault()?
            .load(session_id)
            .map_err(|error| login_required(
                account_id,
                &format!("MiMo browserless auth material is unavailable; login with browser again: {error}"),
            ))?;

        let service_token = cookie_value(&material, "xiaomichatbot_serviceToken")
            .or_else(|| cookie_value(&material, "serviceToken"));
        let user_id = cookie_value(&material, "userId");
        let ph_token = cookie_value(&material, "xiaomichatbot_ph");

        let missing = [
            ("serviceToken", service_token.is_none()),
            ("userId", user_id.is_none()),
            ("xiaomichatbot_ph", ph_token.is_none()),
        ]
        .into_iter()
        .filter_map(|(name, missing)| missing.then_some(name))
        .collect::<Vec<_>>();

        if !missing.is_empty() {
            return Err(login_required(
                account_id,
                &format!(
                    "MiMo auth snapshot is missing required cookie(s): {}",
                    missing.join(", ")
                ),
            ));
        }

        let ph_raw = ph_token.expect("validated MiMo ph cookie");
        let ph_clean = ph_raw.trim_matches('"').to_string();

        Ok(MimoCredentials {
            material,
            ph_token: ph_clean,
        })
    }

    fn common_headers(
        &self,
        mut builder: RequestBuilder,
        material: &BrowserAuthMaterial,
        json_body: bool,
    ) -> RequestBuilder {
        builder = builder
            .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .header(ORIGIN, MIMO_BASE_URL)
            .header(REFERER, format!("{MIMO_BASE_URL}/"))
            .header("x-timezone", "Asia/Shanghai")
            .header("cache-control", "no-cache")
            .header("pragma", "no-cache");
        if json_body {
            builder = builder.header(CONTENT_TYPE, "application/json");
        }
        if !material.user_agent.trim().is_empty() {
            builder = builder.header(USER_AGENT, material.user_agent.trim());
        }
        let mut cookies = material.cookie_header_for_host(MIMO_HOST);
        if !cookies.contains("serviceToken=") {
            if let Some(token) = cookie_value(material, "xiaomichatbot_serviceToken") {
                if !cookies.is_empty() {
                    cookies.push_str("; ");
                }
                cookies.push_str(&format!("serviceToken={token}"));
            }
        }
        if !cookies.is_empty() {
            builder = builder.header(COOKIE, cookies);
        }
        builder
    }

    async fn validate_auth(
        &self,
        account_id: &str,
        binding: &BrowserAccountBinding,
    ) -> Result<MimoCredentials, BrowserProviderError> {
        let credentials = self.credentials(&binding.session, account_id)?;
        let _ = self.model_catalog(account_id, binding).await?;
        Ok(credentials)
    }

    async fn model_catalog(
        &self,
        account_id: &str,
        binding: &BrowserAccountBinding,
    ) -> Result<MimoModelCatalog, BrowserProviderError> {
        let credentials = self.credentials(&binding.session, account_id)?;
        let response = self
            .common_headers(
                self.client.get(MIMO_CONFIG_URL),
                &credentials.material,
                false,
            )
            .header(ACCEPT, "application/json, text/plain, */*")
            .timeout(Duration::from_millis(
                binding.probe_timeout_ms.unwrap_or(8_000).max(1_000),
            ))
            .send()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        if status.as_u16() == 401 {
            return Err(login_required(
                account_id,
                "MiMo bot config rejected the saved Xiaomi session",
            ));
        }
        if status.as_u16() == 403 {
            return Err(provider_challenge(
                account_id,
                "MiMo bot config requires browser re-authentication or a provider challenge",
            ));
        }
        if status.as_u16() == 429 {
            return Err(BrowserProviderError::Transport(
                "rate_limited: MiMo bot config returned HTTP 429".into(),
            ));
        }
        if !status.is_success() {
            return Err(BrowserProviderError::Transport(format!(
                "MiMo bot config failed: HTTP {} body={}",
                status.as_u16(),
                body_preview(&String::from_utf8_lossy(&bytes))
            )));
        }

        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "adapter_incompatible".into(),
                message: format!("MiMo bot config returned invalid JSON: {error}"),
            }
        })?;
        if let Some(code) = nonzero_code(&value) {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "adapter_incompatible".into(),
                message: format!(
                    "MiMo bot config returned code {code}: {}",
                    error_message(&value)
                ),
            });
        }

        parse_model_catalog(account_id, &value)
    }

    async fn resolve_model(
        &self,
        account_id: &str,
        binding: &BrowserAccountBinding,
        requested: &str,
        body: &Value,
    ) -> Result<MimoModelSelection, BrowserProviderError> {
        let catalog = self.model_catalog(account_id, binding).await?;
        let normalized = requested.trim();
        let desired = if normalized.is_empty()
            || normalized.eq_ignore_ascii_case("mimo-web-default")
            || normalized.eq_ignore_ascii_case("default")
        {
            catalog.default_model.as_str()
        } else {
            normalized
        };
        let model = catalog
            .models
            .iter()
            .find(|model| model.external_id.eq_ignore_ascii_case(desired))
            .ok_or_else(|| BrowserProviderError::ModelUnavailable {
                account_id: account_id.to_string(),
                model: requested.to_string(),
            })?;
        let explicit_thinking = body
            .get("enable_thinking")
            .or_else(|| body.get("thinking_enabled"))
            .and_then(Value::as_bool);
        let reasoning_effort = body
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .map(|value| {
                !matches!(
                    value.to_ascii_lowercase().as_str(),
                    "none" | "off" | "disabled"
                )
            });
        let enable_thinking = explicit_thinking
            .or(reasoning_effort)
            .unwrap_or_else(|| model.external_id.to_ascii_lowercase().contains("pro"));
        Ok(MimoModelSelection {
            external_id: model.external_id.clone(),
            enable_thinking,
        })
    }

    async fn native_conversation(
        &self,
        request: &BrowserAdapterRequest,
        model: &MimoModelSelection,
    ) -> Result<Option<MimoNativeConversation>, BrowserProviderError> {
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
                message: "MiMo direct conversation state requires resync after browser transport"
                    .into(),
            });
        }

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
                        "MiMo provider conversation exists without direct HTTP state; browser recovery is required"
                            .into(),
                });
            }
            return Ok(None);
        };

        let stored_model = state
            .get("model_external_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if !stored_model.is_empty() && !stored_model.eq_ignore_ascii_case(&model.external_id) {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "model_binding_conflict".into(),
                message: format!(
                    "MiMo conversation is bound to model '{stored_model}'; start a new llmgateway thread to use '{}'",
                    model.external_id
                ),
            });
        }

        let conversation_id = state
            .get("conversation_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if conversation_id.is_empty() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "direct_state_unsynced".into(),
                message: "MiMo direct conversation state is missing conversation_id".into(),
            });
        }
        let parent_id = state
            .get("parent_id")
            .or_else(|| state.get("parent_message_id"))
            .or_else(|| state.get("response_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        Ok(Some(MimoNativeConversation {
            conversation_id,
            parent_id,
        }))
    }

    async fn submit_chat(
        &self,
        request: &BrowserAdapterRequest,
        credentials: &MimoCredentials,
        conversation_id: &str,
        parent_id: Option<&str>,
        prompt: &str,
        model: &MimoModelSelection,
    ) -> Result<Response, BrowserProviderError> {
        let mut url = Url::parse(MIMO_CHAT_URL)
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        url.query_pairs_mut()
            .append_pair("xiaomichatbot_ph", &credentials.ph_token);

        let temperature = request
            .body
            .get("temperature")
            .and_then(Value::as_f64)
            .unwrap_or(0.8)
            .clamp(0.0, 2.0);
        let top_p = request
            .body
            .get("top_p")
            .and_then(Value::as_f64)
            .unwrap_or(0.95)
            .clamp(0.0, 1.0);
        let mut body = json!({
            "msgId": Uuid::new_v4().simple().to_string(),
            "conversationId": conversation_id,
            "query": prompt,
            "modelConfig": {
                "enableThinking": model.enable_thinking,
                "webSearchStatus": "disabled",
                "model": model.external_id,
                "temperature": temperature,
                "topP": top_p
            },
            "multiMedias": []
        });
        if let Some(parent_id) = parent_id.filter(|value| !value.trim().is_empty()) {
            body["parentId"] = Value::String(parent_id.to_string());
        }

        let response = self
            .common_headers(self.client.post(url), &credentials.material, true)
            .header(ACCEPT, "text/event-stream, */*")
            .timeout(response_timeout(&request.binding))
            .json(&body)
            .send()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;

        let status = response.status();
        if status.as_u16() == 401 {
            return Err(login_required(
                &request.account.id,
                "MiMo chat returned HTTP 401",
            ));
        }
        if status.as_u16() == 403 {
            return Err(provider_challenge(
                &request.account.id,
                "MiMo chat requires browser re-authentication or a provider challenge",
            ));
        }
        if status.as_u16() == 429 {
            return Err(BrowserProviderError::Transport(
                "rate_limited: MiMo chat returned HTTP 429".into(),
            ));
        }
        if !status.is_success() {
            let bytes = response
                .bytes()
                .await
                .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
            return Err(BrowserProviderError::Transport(format!(
                "MiMo chat failed: HTTP {} body={}",
                status.as_u16(),
                body_preview(&String::from_utf8_lossy(&bytes))
            )));
        }
        Ok(response)
    }

    async fn persist_conversation_state(
        request: &BrowserAdapterRequest,
        conversation_id: &str,
        state: &MimoStreamState,
        model: &MimoModelSelection,
    ) -> Result<(), BrowserProviderError> {
        let Some(thread_id) = request.thread_id.as_deref() else {
            return Ok(());
        };
        let Some(store) = conversation_runtime::get() else {
            return Ok(());
        };
        let safe_url = format!("{MIMO_BASE_URL}/#/c/{conversation_id}");
        store
            .upsert_provider_conversation(
                thread_id,
                &request.provider.id,
                &request.account.id,
                &safe_url,
            )
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        store
            .upsert_provider_conversation_state(
                thread_id,
                &request.provider.id,
                &request.account.id,
                &json!({
                    "schema_version": 1,
                    "transport": "mimo-http",
                    "conversation_id": conversation_id,
                    "parent_id": state.dialog_id,
                    "response_id": state.dialog_id,
                    "model_external_id": model.external_id,
                    "metadata": {
                        "thinking_enabled": model.enable_thinking
                    }
                }),
            )
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))
    }

    async fn buffered_response(
        &self,
        request: &BrowserAdapterRequest,
        response: Response,
        conversation_id: String,
        model: MimoModelSelection,
    ) -> Result<Response, BrowserProviderError> {
        let mut decoder = MimoSseDecoder::default();
        let mut state = MimoStreamState::default();
        let mut upstream = response.bytes_stream();
        let mut seen_bytes = false;

        loop {
            let wait = if seen_bytes {
                idle_timeout(&request.binding)
            } else {
                first_byte_timeout(&request.binding)
            };
            let next = timeout(wait, upstream.next()).await.map_err(|_| {
                BrowserProviderError::Transport(if seen_bytes {
                    "idle_stream_timeout: MiMo direct stream was idle too long".into()
                } else {
                    "first_byte_timeout: MiMo direct stream produced no data".into()
                })
            })?;
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|error| {
                BrowserProviderError::Transport(format!(
                    "upstream_stream_dropped: MiMo direct stream body error: {error}"
                ))
            })?;
            seen_bytes = true;
            for update in decoder
                .push(&chunk)
                .map_err(BrowserProviderError::Transport)?
            {
                state
                    .apply(update)
                    .map_err(BrowserProviderError::Transport)?;
            }
        }
        for update in decoder.finish().map_err(BrowserProviderError::Transport)? {
            state
                .apply(update)
                .map_err(BrowserProviderError::Transport)?;
        }
        state
            .validate_completion()
            .map_err(BrowserProviderError::Transport)?;
        Self::persist_conversation_state(request, &conversation_id, &state, &model).await?;

        let mut message = json!({"role": "assistant", "content": state.output});
        if !state.reasoning.is_empty() {
            message["reasoning_content"] = Value::String(state.reasoning);
        }
        let mut body = json!({
            "id": format!("chatcmpl-{}", Uuid::new_v4()),
            "object": "chat.completion",
            "created": Utc::now().timestamp(),
            "model": request.route.model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": "stop"
            }]
        });
        if let Some(usage) = state.usage {
            body["usage"] = normalize_usage(&usage);
        }
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
        conversation_id: String,
        model_selection: MimoModelSelection,
    ) -> Result<Response, BrowserProviderError> {
        let request = request.clone();
        let model = request.route.model.clone();
        let completion_id = format!("chatcmpl-{}", Uuid::new_v4());
        let created = Utc::now().timestamp();
        let mut upstream = response.bytes_stream();

        let stream = async_stream::stream! {
            let mut decoder = MimoSseDecoder::default();
            let mut state = MimoStreamState::default();
            let mut emitted_role = false;
            let mut seen_bytes = false;

            loop {
                let has_content = !state.output.is_empty() || !state.reasoning.is_empty();
                let wait = if has_content {
                    idle_timeout(&request.binding)
                } else {
                    first_byte_timeout(&request.binding)
                };
                let next = match timeout(wait, upstream.next()).await {
                    Ok(value) => value,
                    Err(_) => {
                        if has_content {
                            tracing::warn!(
                                account_id = %request.account.id,
                                model = %model,
                                "MiMo stream timed out after emitting content; finalizing gracefully"
                            );
                            break;
                        }
                        yield Err(std::io::Error::other(if seen_bytes {
                            "idle_stream_timeout: MiMo direct stream was idle too long"
                        } else {
                            "first_byte_timeout: MiMo direct stream produced no data"
                        }));
                        return;
                    }
                };
                let Some(chunk) = next else { break };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        if has_content {
                            tracing::warn!(
                                account_id = %request.account.id,
                                model = %model,
                                error = %error,
                                "MiMo stream dropped after emitting content; finalizing gracefully"
                            );
                            break;
                        }
                        yield Err(std::io::Error::other(format!(
                            "upstream_stream_dropped: MiMo direct stream body error: {error}"
                        )));
                        return;
                    }
                };
                seen_bytes = true;
                let updates = match decoder.push(&chunk) {
                    Ok(updates) => updates,
                    Err(error) => {
                        yield Err(std::io::Error::other(error));
                        return;
                    }
                };
                for update in updates {
                    let (output, reasoning) = match state.apply(update) {
                        Ok(delta) => delta,
                        Err(error) => {
                            yield Err(std::io::Error::other(error));
                            return;
                        }
                    };
                    if !output.is_empty() || !reasoning.is_empty() {
                        let mut delta = json!({});
                        if !emitted_role {
                            emitted_role = true;
                            delta["role"] = Value::String("assistant".into());
                        }
                        if !output.is_empty() {
                            delta["content"] = Value::String(output);
                        }
                        if !reasoning.is_empty() {
                            delta["reasoning_content"] = Value::String(reasoning);
                        }
                        let event = json!({
                            "id": completion_id,
                            "object": "chat.completion.chunk",
                            "created": created,
                            "model": model,
                            "choices": [{"index": 0, "delta": delta, "finish_reason": Value::Null}]
                        });
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("data: {event}\n\n")));
                    }
                }
            }

            let updates = match decoder.finish() {
                Ok(updates) => updates,
                Err(error) => {
                    yield Err(std::io::Error::other(error));
                    return;
                }
            };
            for update in updates {
                let (output, reasoning) = match state.apply(update) {
                    Ok(delta) => delta,
                    Err(error) => {
                        yield Err(std::io::Error::other(error));
                        return;
                    }
                };
                if !output.is_empty() || !reasoning.is_empty() {
                    let mut delta = json!({});
                    if !emitted_role {
                        emitted_role = true;
                        delta["role"] = Value::String("assistant".into());
                    }
                    if !output.is_empty() {
                        delta["content"] = Value::String(output);
                    }
                    if !reasoning.is_empty() {
                        delta["reasoning_content"] = Value::String(reasoning);
                    }
                    let event = json!({
                        "id": completion_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{"index": 0, "delta": delta, "finish_reason": Value::Null}]
                    });
                    yield Ok(Bytes::from(format!("data: {event}\n\n")));
                }
            }

            if let Err(error) = state.validate_completion() {
                if !state.output.is_empty() {
                    tracing::warn!(
                        account_id = %request.account.id,
                        model = %model,
                        error = %error,
                        "MiMo stream completed without formal finish marker; finalizing gracefully"
                    );
                } else {
                    yield Err(std::io::Error::other(error));
                    return;
                }
            }
            if let Err(error) = Self::persist_conversation_state(
                &request,
                &conversation_id,
                &state,
                &model_selection,
            )
            .await
            {
                yield Err(std::io::Error::other(error.to_string()));
                return;
            }

            let finish_reason = if state.completed { "stop" } else { "length" };
            let mut final_event = json!({
                "id": completion_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}]
            });
            if let Some(usage) = &state.usage {
                final_event["usage"] = normalize_usage(usage);
            }
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
impl BrowserProviderAdapter for MimoWebHttpAdapter {
    fn kind(&self) -> &'static str {
        "browser-mimo-http"
    }

    fn adapter_id(&self) -> &'static str {
        "mimo-web-http"
    }

    fn browserless_capabilities(&self) -> BrowserlessCapabilities {
        BrowserlessCapabilities::preferred(BrowserTransportMode::Auto, true, true, true)
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
        let _ = self.validate_auth(account_id, binding).await?;
        Ok(self.model_catalog(account_id, binding).await?.models)
    }

    async fn diagnose(
        &self,
        account_id: &str,
        _profile_dir: &str,
        binding: &BrowserAccountBinding,
    ) -> BrowserAdapterDiagnostics {
        match self.credentials(&binding.session, account_id) {
            Ok(_) => BrowserAdapterDiagnostics {
                account_id: account_id.to_string(),
                provider_kind: "browser-mimo".into(),
                adapter_id: Some(self.adapter_id().into()),
                adapter_version: Some(MIMO_ADAPTER_VERSION.into()),
                contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
                expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                status: "ready".into(),
                message:
                    "MiMo Studio direct HTTP auth is valid; Chromium is not required for normal chat"
                        .into(),
                page_signature: None,
                target_url_prefix: Some(MIMO_BASE_URL.into()),
                configured_models: binding.models.clone(),
            },
            Err(BrowserProviderError::AdapterIncompatible { code, message, .. }) => {
                BrowserAdapterDiagnostics {
                    account_id: account_id.to_string(),
                    provider_kind: "browser-mimo".into(),
                    adapter_id: Some(self.adapter_id().into()),
                    adapter_version: Some(MIMO_ADAPTER_VERSION.into()),
                    contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
                    expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                    status: if code == "login_required" {
                        "login_required".into()
                    } else if code == "browser_challenge_required" {
                        "provider_challenge".into()
                    } else {
                        "adapter_incompatible".into()
                    },
                    message,
                    page_signature: None,
                    target_url_prefix: Some(MIMO_BASE_URL.into()),
                    configured_models: binding.models.clone(),
                }
            }
            Err(error) => BrowserAdapterDiagnostics {
                account_id: account_id.to_string(),
                provider_kind: "browser-mimo".into(),
                adapter_id: Some(self.adapter_id().into()),
                adapter_version: Some(MIMO_ADAPTER_VERSION.into()),
                contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
                expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                status: "unavailable".into(),
                message: error.to_string(),
                page_signature: None,
                target_url_prefix: Some(MIMO_BASE_URL.into()),
                configured_models: binding.models.clone(),
            },
        }
    }

    async fn execute_chat(
        &self,
        request: BrowserAdapterRequest,
    ) -> Result<Response, BrowserProviderError> {
        let model = self
            .resolve_model(
                &request.account.id,
                &request.binding,
                &request.route.model,
                &request.body,
            )
            .await?;
        let native = self.native_conversation(&request, &model).await?;
        let prompt = serialize_prompt(&request.body, native.is_some())?;
        let credentials = self
            .validate_auth(&request.account.id, &request.binding)
            .await?;
        let conversation_id = native
            .as_ref()
            .map(|state| state.conversation_id.clone())
            .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
        let parent_id = native.as_ref().and_then(|state| state.parent_id.as_deref());
        let upstream = self
            .submit_chat(
                &request,
                &credentials,
                &conversation_id,
                parent_id,
                &prompt,
                &model,
            )
            .await?;

        if request
            .body
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            self.streaming_response(&request, upstream, conversation_id, model)
        } else {
            self.buffered_response(&request, upstream, conversation_id, model)
                .await
        }
    }
}

fn parse_model_catalog(
    account_id: &str,
    value: &Value,
) -> Result<MimoModelCatalog, BrowserProviderError> {
    let entries = value
        .pointer("/data/modelConfigListNg")
        .or_else(|| value.get("modelConfigListNg"))
        .and_then(Value::as_array)
        .ok_or_else(|| BrowserProviderError::AdapterIncompatible {
            account_id: account_id.to_string(),
            code: "adapter_incompatible".into(),
            message: "MiMo bot config did not contain data.modelConfigListNg".into(),
        })?;

    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    let mut default_model = None;
    for entry in entries {
        let page_type = entry
            .get("pageType")
            .and_then(Value::as_str)
            .unwrap_or("chat");
        if !page_type.eq_ignore_ascii_case("chat") {
            continue;
        }
        if entry.get("isNew").and_then(Value::as_bool) == Some(false)
            && !entry
                .get("isDefault")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            continue;
        }
        let Some(external_id) = entry
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        if !seen.insert(external_id.to_ascii_lowercase()) {
            continue;
        }
        let display_name = entry
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(external_id);
        if entry
            .get("isDefault")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            default_model = Some(external_id.to_string());
        }
        let mut capabilities = vec!["chat".into(), "streaming".into(), "coding".into()];
        if external_id.to_ascii_lowercase().contains("pro") {
            capabilities.push("reasoning".into());
        }
        models.push(BrowserDiscoveredModel {
            external_id: external_id.to_string(),
            display_name: display_name.to_string(),
            owned_by: "Xiaomi MiMo".into(),
            context_window: None,
            capabilities,
        });
    }

    if models.is_empty() {
        return Err(BrowserProviderError::AdapterIncompatible {
            account_id: account_id.to_string(),
            code: "adapter_incompatible".into(),
            message: "MiMo bot config exposed no selectable chat models".into(),
        });
    }
    let default_model = default_model
        .or_else(|| {
            models
                .iter()
                .find(|model| model.external_id.eq_ignore_ascii_case("mimo-v2.5-pro"))
                .map(|model| model.external_id.clone())
        })
        .unwrap_or_else(|| models[0].external_id.clone());

    Ok(MimoModelCatalog {
        models,
        default_model,
    })
}

fn parse_sse_frame(frame: &str) -> Result<Option<MimoFrameUpdate>, String> {
    let mut event = "";
    let mut data = Vec::new();
    for line in frame.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event = value.trim();
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    let raw = data.join("\n");
    if raw.trim() == "[DONE]" {
        return Ok(Some(MimoFrameUpdate {
            completed: true,
            ..Default::default()
        }));
    }
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("MiMo SSE returned invalid JSON: {error}"))?;
    if !value.is_object() {
        return Ok(None);
    }

    if let Some(code) = nonzero_code(&value) {
        return Ok(Some(MimoFrameUpdate {
            error: Some(format!(
                "MiMo upstream error {code}: {}",
                error_message(&value)
            )),
            ..Default::default()
        }));
    }

    let mut update = MimoFrameUpdate::default();
    match event.to_ascii_lowercase().as_str() {
        "message" => {
            update.text = value
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        "usage" => update.usage = Some(value.clone()),
        "dialogid" | "dialog_id" => {
            update.dialog_id = first_string(
                &value,
                &["content", "dialogId", "dialog_id", "id", "messageId"],
            );
        }
        "finish" | "close" | "done" => update.completed = true,
        _ => {}
    }

    if update.text.is_empty() && value.get("type").and_then(Value::as_str) == Some("text") {
        update.text = value
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
    }
    if update.usage.is_none()
        && (value.get("promptTokens").is_some()
            || value.get("completionTokens").is_some()
            || value.get("totalTokens").is_some())
    {
        update.usage = Some(value.clone());
    }
    if update.dialog_id.is_none() {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(kind.as_str(), "dialogid" | "dialog_id") {
            update.dialog_id = first_string(
                &value,
                &["content", "dialogId", "dialog_id", "id", "messageId"],
            );
        }
    }
    if value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            matches!(
                kind.to_ascii_lowercase().as_str(),
                "finish" | "done" | "close"
            )
        })
    {
        update.completed = true;
    }
    Ok(Some(update))
}

fn split_reasoning(raw: &str) -> (String, String) {
    let text = raw.replace('\0', "");
    let open_start = text.find("<think");
    let Some(open_start) = open_start else {
        if !text.is_empty() && ("<think>".starts_with(&text) || "<think".starts_with(&text)) {
            return (String::new(), String::new());
        }
        return (String::new(), text);
    };

    let prefix = text[..open_start].to_string();
    let Some(open_end_rel) = text[open_start..].find('>') else {
        return (String::new(), prefix);
    };
    let reasoning_start = open_start + open_end_rel + 1;
    let tail = &text[reasoning_start..];
    let close = tail
        .find("</think>")
        .map(|index| (index, "</think>".len()))
        .or_else(|| {
            tail.find("</thinkgt;>")
                .map(|index| (index, "</thinkgt;>".len()))
        });
    if let Some((close_index, close_len)) = close {
        let reasoning = tail[..close_index].to_string();
        let output = format!("{prefix}{}", &tail[close_index + close_len..]);
        return (reasoning, output);
    }

    let withheld = partial_suffix_len(tail, &["</think>", "</thinkgt;>"]);
    let reasoning_end = tail.len().saturating_sub(withheld);
    (tail[..reasoning_end].to_string(), prefix)
}

fn partial_suffix_len(text: &str, tokens: &[&str]) -> usize {
    tokens
        .iter()
        .flat_map(|token| 1..token.len())
        .filter(|length| {
            text.len() >= *length
                && text.is_char_boundary(text.len() - *length)
                && tokens
                    .iter()
                    .any(|token| token.starts_with(&text[text.len() - *length..]))
        })
        .max()
        .unwrap_or(0)
}

fn monotonic_delta(previous: &str, next: &str, channel: &str) -> Result<String, String> {
    if next == previous || previous.starts_with(next) {
        return Ok(String::new());
    }
    if let Some(delta) = next.strip_prefix(previous) {
        return Ok(delta.to_string());
    }
    Err(format!(
        "stream_rewrite_detected: MiMo rewrote previously emitted {channel} text"
    ))
}

fn serialize_prompt(body: &Value, native: bool) -> Result<String, BrowserProviderError> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            BrowserProviderError::InvalidConfig("MiMo request requires messages".into())
        })?;

    for message in messages {
        if content_has_non_text(message.get("content").unwrap_or(&Value::Null)) {
            return Err(BrowserProviderError::InvalidConfig(
                "MiMo Web multimodal input is not enabled until Studio upload semantics are live-verified"
                    .into(),
            ));
        }
    }

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
            "MiMo native continuation requires a non-empty user message".into(),
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
        match role {
            "system" => rendered.push(format!("System: {text}")),
            "assistant" => rendered.push(format!("Assistant: {text}")),
            "tool" => rendered.push(format!("Tool result: {text}")),
            _ => rendered.push(format!("User: {text}")),
        }
    }
    if rendered.is_empty() {
        return Err(BrowserProviderError::InvalidConfig(
            "MiMo request has no textual prompt".into(),
        ));
    }
    Ok(rendered.join("\n\n"))
}

fn message_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                    if matches!(kind, "text" | "input_text") {
                        item.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn content_has_non_text(value: &Value) -> bool {
    value.as_array().is_some_and(|items| {
        items.iter().any(|item| {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            !kind.is_empty() && !matches!(kind, "text" | "input_text")
        })
    })
}

fn cookie_value(material: &BrowserAuthMaterial, name: &str) -> Option<String> {
    material
        .cookie_value_for_host(MIMO_HOST, name)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn find_sse_separator(bytes: &[u8]) -> Option<(usize, usize)> {
    if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
        return Some((index, 4));
    }
    bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2))
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn nonzero_code(value: &Value) -> Option<i64> {
    value
        .get("code")
        .and_then(Value::as_i64)
        .filter(|code| *code != 0)
}

fn error_message(value: &Value) -> String {
    value
        .get("msg")
        .or_else(|| value.get("message"))
        .or_else(|| value.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("unknown MiMo error")
        .to_string()
}

fn normalize_usage(value: &Value) -> Value {
    let prompt = value
        .get("promptTokens")
        .or_else(|| value.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = value
        .get("completionTokens")
        .or_else(|| value.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = value
        .get("totalTokens")
        .or_else(|| value.get("total_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(prompt + completion);
    let reasoning = value
        .pointer("/nativeUsage/completion_tokens_details/reasoning_tokens")
        .or_else(|| value.pointer("/completion_tokens_details/reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": total,
        "completion_tokens_details": {
            "reasoning_tokens": reasoning
        }
    })
}

fn response_timeout(binding: &BrowserAccountBinding) -> Duration {
    Duration::from_millis(binding.response_timeout_ms.unwrap_or(180_000))
}

fn first_byte_timeout(binding: &BrowserAccountBinding) -> Duration {
    Duration::from_millis(binding.first_byte_timeout_ms.unwrap_or(60_000))
}

fn idle_timeout(binding: &BrowserAccountBinding) -> Duration {
    Duration::from_millis(binding.idle_stream_timeout_ms.unwrap_or(60_000))
}

fn login_required(account_id: &str, message: &str) -> BrowserProviderError {
    BrowserProviderError::AdapterIncompatible {
        account_id: account_id.to_string(),
        code: "login_required".into(),
        message: message.to_string(),
    }
}

fn provider_challenge(account_id: &str, message: &str) -> BrowserProviderError {
    BrowserProviderError::AdapterIncompatible {
        account_id: account_id.to_string(),
        code: "browser_challenge_required".into(),
        message: message.to_string(),
    }
}

fn body_preview(body: &str) -> String {
    body.chars()
        .filter(|character| !character.is_control())
        .take(256)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_auth::BrowserAuthCookie;
    use std::collections::BTreeMap;

    #[test]
    fn auth_requires_the_three_studio_cookies() {
        let material = BrowserAuthMaterial::new(
            "mimo-session",
            "mimo-web",
            MIMO_BASE_URL,
            "test-agent",
            vec![
                BrowserAuthCookie {
                    name: "serviceToken".into(),
                    value: "secret-service".into(),
                    domain: ".xiaomimimo.com".into(),
                    path: "/".into(),
                    expires: 0.0,
                    http_only: true,
                    secure: true,
                    same_site: None,
                },
                BrowserAuthCookie {
                    name: "userId".into(),
                    value: "user-1".into(),
                    domain: ".xiaomimimo.com".into(),
                    path: "/".into(),
                    expires: 0.0,
                    http_only: false,
                    secure: true,
                    same_site: None,
                },
                BrowserAuthCookie {
                    name: "xiaomichatbot_ph".into(),
                    value: "ph-secret".into(),
                    domain: "aistudio.xiaomimimo.com".into(),
                    path: "/".into(),
                    expires: 0.0,
                    http_only: false,
                    secure: true,
                    same_site: None,
                },
            ],
            BTreeMap::new(),
            BTreeMap::new(),
        );
        assert_eq!(
            cookie_value(&material, "serviceToken").as_deref(),
            Some("secret-service")
        );
        assert_eq!(cookie_value(&material, "userId").as_deref(), Some("user-1"));
        assert_eq!(
            cookie_value(&material, "xiaomichatbot_ph").as_deref(),
            Some("ph-secret")
        );
        assert!(!material.cookie_header_for_host(MIMO_HOST).is_empty());
    }

    #[test]
    fn model_catalog_uses_live_config_shape_and_default() {
        let value = json!({
            "code": 0,
            "data": {
                "modelConfigListNg": [
                    {"name":"MiMo V2.5 Pro","model":"mimo-v2.5-pro","pageType":"chat","isDefault":true},
                    {"name":"MiMo V2.5","model":"mimo-v2.5","pageType":"chat"},
                    {"name":"TTS","model":"mimo-tts","pageType":"voice"}
                ]
            }
        });
        let catalog = parse_model_catalog("account", &value).unwrap();
        assert_eq!(catalog.default_model, "mimo-v2.5-pro");
        assert_eq!(catalog.models.len(), 2);
        assert!(catalog.models[0]
            .capabilities
            .contains(&"reasoning".to_string()));
        assert!(!catalog
            .models
            .iter()
            .any(|model| model.external_id == "mimo-tts"));
    }

    #[test]
    fn sse_decoder_handles_message_usage_dialog_and_finish_events() {
        let stream = concat!(
            "event: message\ndata: {\"content\":\"<think>why\"}\n\n",
            "event: message\ndata: {\"content\":\" now</think>Hello\"}\n\n",
            "event: dialogId\ndata: {\"content\":\"response-1\"}\n\n",
            "event: usage\ndata: {\"promptTokens\":4,\"completionTokens\":6,\"totalTokens\":10}\n\n",
            "event: finish\ndata: {}\n\n"
        );
        let mut decoder = MimoSseDecoder::default();
        let mut state = MimoStreamState::default();
        for update in decoder.push(stream.as_bytes()).unwrap() {
            state.apply(update).unwrap();
        }
        assert_eq!(state.reasoning, "why now");
        assert_eq!(state.output, "Hello");
        assert_eq!(state.dialog_id.as_deref(), Some("response-1"));
        assert_eq!(state.usage.as_ref().unwrap()["totalTokens"], 10);
        assert!(state.completed);
        state.validate_completion().unwrap();
    }

    #[test]
    fn sse_decoder_accepts_generic_text_frames() {
        let stream = concat!(
            "data: {\"type\":\"text\",\"content\":\"Hel\"}\n\n",
            "data: {\"type\":\"text\",\"content\":\"lo\"}\n\n",
            "data: {\"type\":\"finish\"}\n\n"
        );
        let mut decoder = MimoSseDecoder::default();
        let mut state = MimoStreamState::default();
        for update in decoder.push(stream.as_bytes()).unwrap() {
            state.apply(update).unwrap();
        }
        assert_eq!(state.output, "Hello");
        assert!(state.completed);
    }

    #[test]
    fn accumulated_text_snapshots_do_not_duplicate_output() {
        let mut state = MimoStreamState::default();
        state
            .apply(MimoFrameUpdate {
                text: "Hel".into(),
                ..Default::default()
            })
            .unwrap();
        let (delta, _) = state
            .apply(MimoFrameUpdate {
                text: "Hello".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(delta, "lo");
        assert_eq!(state.output, "Hello");
    }

    #[test]
    fn split_reasoning_holds_partial_tags_until_safe() {
        assert_eq!(split_reasoning("<thi"), (String::new(), String::new()));
        assert_eq!(
            split_reasoning("<think>why</thi"),
            ("why".into(), "".into())
        );
        assert_eq!(
            split_reasoning("<think>why</think>Hello"),
            ("why".into(), "Hello".into())
        );
    }

    #[test]
    fn split_reasoning_handles_multibyte_utf8_without_panic() {
        let text = "<think>The user wants me to test \"tất cả\" sự ổn định";
        let (reasoning, prefix) = split_reasoning(text);
        assert_eq!(reasoning, "The user wants me to test \"tất cả\" sự ổn định");
        assert_eq!(prefix, "");
    }

    #[test]
    fn native_continuation_serializes_only_latest_user_turn() {
        let body = json!({
            "messages": [
                {"role":"user","content":"first"},
                {"role":"assistant","content":"answer"},
                {"role":"user","content":"second"}
            ]
        });
        assert_eq!(serialize_prompt(&body, true).unwrap(), "second");
    }

    #[test]
    fn multimodal_parts_are_rejected_until_verified() {
        let body = json!({
            "messages": [{
                "role":"user",
                "content":[
                    {"type":"text","text":"describe"},
                    {"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}
                ]
            }]
        });
        assert!(serialize_prompt(&body, false)
            .unwrap_err()
            .to_string()
            .contains("multimodal input is not enabled"));
    }

    #[test]
    fn incomplete_stream_is_never_accepted() {
        let state = MimoStreamState {
            output: "partial".into(),
            ..Default::default()
        };
        assert!(state
            .validate_completion()
            .unwrap_err()
            .contains("upstream_stream_dropped"));
    }
}
