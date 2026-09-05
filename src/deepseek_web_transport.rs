use crate::{
    browser_auth::{BrowserAuthMaterial, BrowserAuthVault},
    browser_auth_runtime,
    browser_provider::{
        BrowserAccountBinding, BrowserAdapterDiagnostics, BrowserAdapterRequest,
        BrowserDiscoveredModel, BrowserProviderAdapter, BrowserProviderError,
        BrowserTransportMode, BrowserlessCapabilities, BROWSER_ADAPTER_CONTRACT_VERSION,
    },
    conversation_runtime,
    deepseek_pow::{solve_challenge, DeepSeekPowChallenge},
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
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

const DEEPSEEK_HOST: &str = "chat.deepseek.com";
const DEEPSEEK_BASE_URL: &str = "https://chat.deepseek.com";
const CURRENT_USER_URL: &str = "https://chat.deepseek.com/api/v0/users/current";
const CREATE_SESSION_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/create";
const POW_URL: &str = "https://chat.deepseek.com/api/v0/chat/create_pow_challenge";
const COMPLETION_URL: &str = "https://chat.deepseek.com/api/v0/chat/completion";
const COMPLETION_PATH: &str = "/api/v0/chat/completion";
const DEEPSEEK_ADAPTER_VERSION: &str = "experimental-1";
const DEFAULT_CLIENT_VERSION: &str = "2.0.0";

#[derive(Clone)]
pub struct DeepSeekWebHttpAdapter {
    client: Client,
}

#[derive(Clone, Debug)]
struct DeepSeekNativeConversation {
    chat_session_id: String,
    response_message_id: String,
}

#[derive(Clone, Debug)]
struct DeepSeekModelSelection {
    external_id: String,
    model_type: &'static str,
    thinking_enabled: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DeepSeekChannel {
    #[default]
    Output,
    Reasoning,
}

#[derive(Clone, Debug, Default)]
struct DeepSeekFrameUpdate {
    response_message_id: Option<String>,
    output: String,
    reasoning: String,
    completed: bool,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct DeepSeekSseDecoder {
    buffer: Vec<u8>,
    channel: DeepSeekChannel,
}

#[derive(Debug, Default)]
struct DeepSeekStreamState {
    output: String,
    reasoning: String,
    response_message_id: String,
    completed: bool,
}

impl DeepSeekStreamState {
    fn apply(&mut self, update: DeepSeekFrameUpdate) -> Result<(String, String), String> {
        if let Some(error) = update.error {
            return Err(error);
        }
        if let Some(id) = update
            .response_message_id
            .filter(|value| !value.trim().is_empty())
        {
            self.response_message_id = id;
        }
        self.completed |= update.completed;
        self.output.push_str(&update.output);
        self.reasoning.push_str(&update.reasoning);
        Ok((update.output, update.reasoning))
    }

    fn validate_completion(&self) -> Result<(), String> {
        if !self.completed {
            return Err("upstream_stream_dropped: DeepSeek stream ended before logical completion"
                .into());
        }
        if self.output.trim().is_empty() {
            return Err("DeepSeek direct stream completed without assistant output".into());
        }
        if self.response_message_id.trim().is_empty() {
            return Err(
                "DeepSeek direct stream completed without response_message_id".into(),
            );
        }
        Ok(())
    }
}

impl DeepSeekSseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<DeepSeekFrameUpdate>, String> {
        self.buffer.extend_from_slice(bytes);
        let mut updates = Vec::new();
        while let Some((index, separator_len)) = find_sse_separator(&self.buffer) {
            let block = self
                .buffer
                .drain(..index + separator_len)
                .collect::<Vec<_>>();
            let payload = &block[..index];
            if payload.is_empty() {
                continue;
            }
            let text = std::str::from_utf8(payload)
                .map_err(|error| format!("DeepSeek SSE returned invalid UTF-8: {error}"))?;
            if let Some(update) = self.parse_frame(text)? {
                updates.push(update);
            }
        }
        Ok(updates)
    }

    fn finish(&mut self) -> Result<Vec<DeepSeekFrameUpdate>, String> {
        if self.buffer.iter().all(|byte| byte.is_ascii_whitespace()) {
            self.buffer.clear();
            return Ok(Vec::new());
        }
        let mut tail = self.buffer.clone();
        tail.extend_from_slice(b"\n\n");
        self.buffer.clear();
        self.push(&tail)
    }

    fn parse_frame(&mut self, frame: &str) -> Result<Option<DeepSeekFrameUpdate>, String> {
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
            return Ok(Some(DeepSeekFrameUpdate {
                completed: true,
                ..Default::default()
            }));
        }
        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("DeepSeek SSE returned invalid JSON: {error}"))?;
        let mut update = DeepSeekFrameUpdate::default();

        if let Some(code) = value.get("code").and_then(Value::as_i64).filter(|code| *code != 0) {
            update.error = Some(format!(
                "DeepSeek upstream error {code}: {}",
                error_message(&value)
            ));
            return Ok(Some(update));
        }

        if event == "ready" {
            update.response_message_id = value
                .pointer("/data/response_message_id")
                .or_else(|| value.get("response_message_id"))
                .and_then(value_to_id);
        }
        if event == "close" {
            update.completed = true;
        }

        if update.response_message_id.is_none() {
            update.response_message_id = value
                .pointer("/data/response_message_id")
                .or_else(|| value.get("response_message_id"))
                .and_then(value_to_id);
        }

        if let Some(response) = value.pointer("/data/v/response") {
            if let Some(fragments) = response.get("fragments").and_then(Value::as_array) {
                for fragment in fragments {
                    self.append_fragment(fragment, &mut update);
                }
            }
        }

        let path = value
            .pointer("/data/p")
            .or_else(|| value.get("p"))
            .and_then(Value::as_str);
        let operation = value
            .pointer("/data/o")
            .or_else(|| value.get("o"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let patch_value = value.pointer("/data/v").or_else(|| value.get("v"));

        match path {
            Some("response/fragments") => {
                if let Some(items) = patch_value.and_then(Value::as_array) {
                    for fragment in items {
                        self.append_fragment(fragment, &mut update);
                    }
                } else if let Some(fragment) = patch_value {
                    self.append_fragment(fragment, &mut update);
                }
            }
            Some("response/fragments/-1/content") if operation == "APPEND" => {
                if let Some(text) = patch_value.and_then(Value::as_str) {
                    self.append_by_channel(text, &mut update);
                }
            }
            Some("response/status") => {
                if patch_value.and_then(Value::as_str) == Some("FINISHED") {
                    update.completed = true;
                }
            }
            None if operation == "APPEND" => {
                if let Some(text) = patch_value.and_then(Value::as_str) {
                    self.append_by_channel(text, &mut update);
                }
            }
            _ => {}
        }

        Ok(Some(update))
    }

    fn append_fragment(&mut self, fragment: &Value, update: &mut DeepSeekFrameUpdate) {
        let kind = fragment
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_uppercase();
        match kind.as_str() {
            "THINK" => self.channel = DeepSeekChannel::Reasoning,
            "RESPONSE" | "ANSWER" => self.channel = DeepSeekChannel::Output,
            _ => {}
        }
        if let Some(content) = fragment.get("content").and_then(Value::as_str) {
            self.append_by_channel(content, update);
        }
    }

    fn append_by_channel(&self, text: &str, update: &mut DeepSeekFrameUpdate) {
        match self.channel {
            DeepSeekChannel::Output => update.output.push_str(text),
            DeepSeekChannel::Reasoning => update.reasoning.push_str(text),
        }
    }
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

impl DeepSeekWebHttpAdapter {
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
                    "DeepSeek browserless auth material is unavailable; login with browser again: {error}"
                ),
            })?;
        if deepseek_user_token(&material).is_none() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "login_required".into(),
                message:
                    "DeepSeek auth snapshot does not contain a usable localStorage userToken"
                        .into(),
            });
        }
        Ok(material)
    }

    fn common_headers(
        &self,
        mut builder: RequestBuilder,
        material: &BrowserAuthMaterial,
        referer: &str,
        token: &str,
        json_body: bool,
    ) -> RequestBuilder {
        builder = builder
            .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .header(ORIGIN, DEEPSEEK_BASE_URL)
            .header(REFERER, referer)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header("x-client-platform", "web")
            .header("x-client-version", client_version(material))
            .header("x-client-locale", "en-US")
            .header("x-client-bundle-id", "com.deepseek.chat");
        if json_body {
            builder = builder.header(CONTENT_TYPE, "application/json");
        }
        if !material.user_agent.trim().is_empty() {
            builder = builder.header(USER_AGENT, material.user_agent.trim());
        }
        let cookies = material.cookie_header_for_host(DEEPSEEK_HOST);
        if !cookies.is_empty() {
            builder = builder.header(COOKIE, cookies);
        }
        builder
    }

    async fn access_token(
        &self,
        account_id: &str,
        binding: &BrowserAccountBinding,
    ) -> Result<(BrowserAuthMaterial, String), BrowserProviderError> {
        let material = self.auth_material(&binding.session, account_id)?;
        let user_token = deepseek_user_token(&material).expect("validated userToken");
        let response = self
            .common_headers(
                self.client.get(CURRENT_USER_URL),
                &material,
                DEEPSEEK_BASE_URL,
                &user_token,
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
        let raw = String::from_utf8_lossy(&bytes);

        if status.as_u16() == 401 {
            return Err(login_required(
                account_id,
                "DeepSeek userToken was rejected by /users/current",
            ));
        }
        if status.as_u16() == 403 {
            return Err(provider_challenge(
                account_id,
                "DeepSeek /users/current requires a browser challenge",
            ));
        }
        if status.as_u16() == 429 {
            return Err(BrowserProviderError::Transport(
                "rate_limited: DeepSeek /users/current returned HTTP 429".into(),
            ));
        }
        if !status.is_success() {
            return Err(BrowserProviderError::Transport(format!(
                "DeepSeek /users/current failed: HTTP {} body={}",
                status.as_u16(),
                body_preview(&raw)
            )));
        }

        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            BrowserProviderError::Transport(format!(
                "DeepSeek /users/current returned invalid JSON: {error}"
            ))
        })?;
        if let Some(code) = deepseek_error_code(&value) {
            if code == 40003 {
                return Err(login_required(
                    account_id,
                    "DeepSeek userToken is expired or invalid",
                ));
            }
            return Err(BrowserProviderError::Transport(format!(
                "DeepSeek /users/current error {code}: {}",
                error_message(&value)
            )));
        }
        let token = value
            .pointer("/data/biz_data/token")
            .or_else(|| value.pointer("/biz_data/token"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if token.is_empty() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "adapter_incompatible".into(),
                message: "DeepSeek /users/current returned no access token".into(),
            });
        }
        Ok((material, token.to_string()))
    }

    async fn create_session(
        &self,
        request: &BrowserAdapterRequest,
        material: &BrowserAuthMaterial,
        access_token: &str,
    ) -> Result<String, BrowserProviderError> {
        let response = self
            .common_headers(
                self.client.post(CREATE_SESSION_URL),
                material,
                DEEPSEEK_BASE_URL,
                access_token,
                true,
            )
            .header(ACCEPT, "application/json, text/plain, */*")
            .timeout(response_timeout(&request.binding))
            .json(&json!({}))
            .send()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let value = self
            .json_response(&request.account.id, "chat_session/create", response)
            .await?;
        let id = value
            .pointer("/data/biz_data/chat_session/id")
            .or_else(|| value.pointer("/biz_data/chat_session/id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if id.is_empty() {
            return Err(BrowserProviderError::Transport(
                "DeepSeek chat_session/create returned no session id".into(),
            ));
        }
        Ok(id.to_string())
    }

    async fn pow_challenge(
        &self,
        request: &BrowserAdapterRequest,
        material: &BrowserAuthMaterial,
        access_token: &str,
    ) -> Result<DeepSeekPowChallenge, BrowserProviderError> {
        let response = self
            .common_headers(
                self.client.post(POW_URL),
                material,
                DEEPSEEK_BASE_URL,
                access_token,
                true,
            )
            .header(ACCEPT, "application/json, text/plain, */*")
            .timeout(response_timeout(&request.binding))
            .json(&json!({"target_path": COMPLETION_PATH}))
            .send()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let value = self
            .json_response(&request.account.id, "create_pow_challenge", response)
            .await?;
        serde_json::from_value(
            value
                .pointer("/data/biz_data/challenge")
                .or_else(|| value.pointer("/biz_data/challenge"))
                .cloned()
                .ok_or_else(|| {
                    BrowserProviderError::Transport(
                        "DeepSeek create_pow_challenge returned no challenge".into(),
                    )
                })?,
        )
        .map_err(|error| {
            BrowserProviderError::Transport(format!(
                "DeepSeek PoW challenge schema is incompatible: {error}"
            ))
        })
    }

    async fn json_response(
        &self,
        account_id: &str,
        operation: &str,
        response: Response,
    ) -> Result<Value, BrowserProviderError> {
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let raw = String::from_utf8_lossy(&bytes);
        if status.as_u16() == 401 {
            return Err(login_required(
                account_id,
                &format!("DeepSeek {operation} returned HTTP 401"),
            ));
        }
        if status.as_u16() == 403 {
            return Err(provider_challenge(
                account_id,
                &format!("DeepSeek {operation} returned HTTP 403"),
            ));
        }
        if status.as_u16() == 429 {
            return Err(BrowserProviderError::Transport(format!(
                "rate_limited: DeepSeek {operation} returned HTTP 429"
            )));
        }
        if !status.is_success() {
            return Err(BrowserProviderError::Transport(format!(
                "DeepSeek {operation} failed: HTTP {} body={}",
                status.as_u16(),
                body_preview(&raw)
            )));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            BrowserProviderError::Transport(format!(
                "DeepSeek {operation} returned invalid JSON: {error}; body={}",
                body_preview(&raw)
            ))
        })?;
        if let Some(code) = deepseek_error_code(&value) {
            if code == 40003 {
                return Err(login_required(
                    account_id,
                    &format!("DeepSeek {operation} reported expired authentication"),
                ));
            }
            if code == 40002 {
                return Err(BrowserProviderError::Transport(format!(
                    "rate_limited: DeepSeek {operation} returned code 40002"
                )));
            }
            return Err(BrowserProviderError::Transport(format!(
                "DeepSeek {operation} error {code}: {}",
                error_message(&value)
            )));
        }
        Ok(value)
    }

    async fn native_conversation(
        &self,
        request: &BrowserAdapterRequest,
    ) -> Result<Option<DeepSeekNativeConversation>, BrowserProviderError> {
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
                message:
                    "DeepSeek direct conversation state requires resync after browser fallback"
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
                        "DeepSeek provider conversation exists without direct HTTP state; browser recovery is required"
                            .into(),
                });
            }
            return Ok(None);
        };

        let chat_session_id = state
            .get("chat_session_id")
            .or_else(|| state.get("conversation_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let response_message_id = state
            .get("next_parent_id")
            .or_else(|| state.get("response_message_id"))
            .or_else(|| state.get("response_id"))
            .and_then(value_to_id)
            .unwrap_or_default();
        if chat_session_id.is_empty() || response_message_id.is_empty() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "direct_state_unsynced".into(),
                message:
                    "DeepSeek direct conversation state is missing chat_session_id/response_message_id"
                        .into(),
            });
        }
        Ok(Some(DeepSeekNativeConversation {
            chat_session_id,
            response_message_id,
        }))
    }

    async fn submit_completion(
        &self,
        request: &BrowserAdapterRequest,
        material: &BrowserAuthMaterial,
        access_token: &str,
        chat_session_id: &str,
        parent_message_id: Option<&str>,
        prompt: &str,
        model: &DeepSeekModelSelection,
    ) -> Result<Response, BrowserProviderError> {
        let challenge = self.pow_challenge(request, material, access_token).await?;
        let pow_timeout = request
            .binding
            .first_byte_timeout_ms
            .unwrap_or(30_000)
            .clamp(1_000, 30_000);
        let (_, pow_header) = solve_challenge(challenge, pow_timeout)
            .await
            .map_err(|error| {
                BrowserProviderError::AdapterIncompatible {
                    account_id: request.account.id.clone(),
                    code: "adapter_incompatible".into(),
                    message: format!("DeepSeek PoW solver failed: {error}"),
                }
            })?;
        let referer = format!("{DEEPSEEK_BASE_URL}/a/chat/s/{chat_session_id}");
        let search_enabled = request
            .body
            .get("search_enabled")
            .or_else(|| request.body.get("web_search"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && model.model_type != "expert";
        let body = json!({
            "chat_session_id": chat_session_id,
            "parent_message_id": parent_message_id,
            "model_type": model.model_type,
            "prompt": prompt,
            "ref_file_ids": [],
            "thinking_enabled": model.thinking_enabled,
            "search_enabled": search_enabled,
            "action": Value::Null,
            "preempt": false
        });
        let response = self
            .common_headers(
                self.client.post(COMPLETION_URL),
                material,
                &referer,
                access_token,
                true,
            )
            .header(ACCEPT, "text/event-stream")
            .header("x-ds-pow-response", pow_header)
            .header("x-client-timezone-offset", "0")
            .timeout(response_timeout(&request.binding))
            .json(&body)
            .send()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let status = response.status();
        if status.as_u16() == 401 {
            return Err(login_required(
                &request.account.id,
                "DeepSeek completion returned HTTP 401",
            ));
        }
        if status.as_u16() == 403 {
            return Err(provider_challenge(
                &request.account.id,
                "DeepSeek completion rejected the browserless request with HTTP 403",
            ));
        }
        if status.as_u16() == 429 {
            return Err(BrowserProviderError::Transport(
                "rate_limited: DeepSeek completion returned HTTP 429".into(),
            ));
        }
        if !status.is_success() {
            let bytes = response
                .bytes()
                .await
                .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
            return Err(BrowserProviderError::Transport(format!(
                "DeepSeek completion failed: HTTP {} body={}",
                status.as_u16(),
                body_preview(&String::from_utf8_lossy(&bytes))
            )));
        }
        Ok(response)
    }

    async fn persist_conversation_state(
        request: &BrowserAdapterRequest,
        chat_session_id: &str,
        request_parent_id: Option<&str>,
        state: &DeepSeekStreamState,
        model: &DeepSeekModelSelection,
    ) -> Result<(), BrowserProviderError> {
        let Some(thread_id) = request.thread_id.as_deref() else {
            return Ok(());
        };
        let Some(store) = conversation_runtime::get() else {
            return Ok(());
        };
        let conversation_url = format!("{DEEPSEEK_BASE_URL}/a/chat/s/{chat_session_id}");
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
                    "schema_version": 1,
                    "transport": "deepseek-http",
                    "model_external_id": model.external_id,
                    "chat_session_id": chat_session_id,
                    "conversation_id": chat_session_id,
                    "request_parent_id": request_parent_id,
                    "response_message_id": state.response_message_id,
                    "response_id": state.response_message_id,
                    "next_parent_id": state.response_message_id
                }),
            )
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))
    }

    async fn buffered_response(
        &self,
        request: &BrowserAdapterRequest,
        response: Response,
        chat_session_id: String,
        request_parent_id: Option<String>,
        model: DeepSeekModelSelection,
    ) -> Result<Response, BrowserProviderError> {
        let mut decoder = DeepSeekSseDecoder::default();
        let mut state = DeepSeekStreamState::default();
        let mut upstream = response.bytes_stream();
        while let Some(chunk) = upstream.next().await {
            let chunk =
                chunk.map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
            for update in decoder
                .push(&chunk)
                .map_err(BrowserProviderError::Transport)?
            {
                state
                    .apply(update)
                    .map_err(BrowserProviderError::Transport)?;
            }
        }
        for update in decoder
            .finish()
            .map_err(BrowserProviderError::Transport)?
        {
            state
                .apply(update)
                .map_err(BrowserProviderError::Transport)?;
        }
        state
            .validate_completion()
            .map_err(BrowserProviderError::Transport)?;
        Self::persist_conversation_state(
            request,
            &chat_session_id,
            request_parent_id.as_deref(),
            &state,
            &model,
        )
        .await?;

        let mut message = json!({"role": "assistant", "content": state.output});
        if !state.reasoning.is_empty() {
            message["reasoning_content"] = Value::String(state.reasoning);
        }
        let body = json!({
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
        chat_session_id: String,
        request_parent_id: Option<String>,
        model_selection: DeepSeekModelSelection,
    ) -> Result<Response, BrowserProviderError> {
        let request = request.clone();
        let model = request.route.model.clone();
        let completion_id = format!("chatcmpl-{}", Uuid::new_v4());
        let created = Utc::now().timestamp();
        let mut upstream = response.bytes_stream();

        let stream = async_stream::stream! {
            let mut decoder = DeepSeekSseDecoder::default();
            let mut state = DeepSeekStreamState::default();
            let mut emitted_role = false;

            while let Some(chunk) = upstream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        yield Err(std::io::Error::other(format!(
                            "upstream_stream_dropped: DeepSeek direct stream body error: {error}"
                        )));
                        return;
                    }
                };
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
                yield Err(std::io::Error::other(error));
                return;
            }
            if let Err(error) = Self::persist_conversation_state(
                &request,
                &chat_session_id,
                request_parent_id.as_deref(),
                &state,
                &model_selection,
            )
            .await
            {
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
impl BrowserProviderAdapter for DeepSeekWebHttpAdapter {
    fn kind(&self) -> &'static str {
        "browser-deepseek-http"
    }

    fn adapter_id(&self) -> &'static str {
        "deepseek-web-http"
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
        let _ = self.access_token(account_id, binding).await?;
        Ok(deepseek_models())
    }

    async fn diagnose(
        &self,
        account_id: &str,
        _profile_dir: &str,
        binding: &BrowserAccountBinding,
    ) -> BrowserAdapterDiagnostics {
        match self.access_token(account_id, binding).await {
            Ok(_) => BrowserAdapterDiagnostics {
                account_id: account_id.to_string(),
                provider_kind: "browser-deepseek".into(),
                adapter_id: Some(self.adapter_id().into()),
                adapter_version: Some(DEEPSEEK_ADAPTER_VERSION.into()),
                contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
                expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                status: "ready".into(),
                message:
                    "DeepSeek direct HTTP auth is valid; Chromium is not required for chat"
                        .into(),
                page_signature: None,
                target_url_prefix: Some(DEEPSEEK_BASE_URL.into()),
                configured_models: binding.models.clone(),
            },
            Err(BrowserProviderError::AdapterIncompatible { code, message, .. }) => {
                BrowserAdapterDiagnostics {
                    account_id: account_id.to_string(),
                    provider_kind: "browser-deepseek".into(),
                    adapter_id: Some(self.adapter_id().into()),
                    adapter_version: Some(DEEPSEEK_ADAPTER_VERSION.into()),
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
                    target_url_prefix: Some(DEEPSEEK_BASE_URL.into()),
                    configured_models: binding.models.clone(),
                }
            }
            Err(error) => BrowserAdapterDiagnostics {
                account_id: account_id.to_string(),
                provider_kind: "browser-deepseek".into(),
                adapter_id: Some(self.adapter_id().into()),
                adapter_version: Some(DEEPSEEK_ADAPTER_VERSION.into()),
                contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
                expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                status: "unavailable".into(),
                message: error.to_string(),
                page_signature: None,
                target_url_prefix: Some(DEEPSEEK_BASE_URL.into()),
                configured_models: binding.models.clone(),
            },
        }
    }

    async fn execute_chat(
        &self,
        request: BrowserAdapterRequest,
    ) -> Result<Response, BrowserProviderError> {
        let model = resolve_model(&request.account.id, &request.route.model)?;
        let native = self.native_conversation(&request).await?;
        let prompt = serialize_prompt(&request.body, native.is_some())?;
        let (material, access_token) = self
            .access_token(&request.account.id, &request.binding)
            .await?;
        let (chat_session_id, request_parent_id) = if let Some(native) = native {
            (
                native.chat_session_id,
                Some(native.response_message_id),
            )
        } else {
            (
                self.create_session(&request, &material, &access_token)
                    .await?,
                None,
            )
        };
        let upstream = self
            .submit_completion(
                &request,
                &material,
                &access_token,
                &chat_session_id,
                request_parent_id.as_deref(),
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
            self.streaming_response(
                &request,
                upstream,
                chat_session_id,
                request_parent_id,
                model,
            )
        } else {
            self.buffered_response(
                &request,
                upstream,
                chat_session_id,
                request_parent_id,
                model,
            )
            .await
        }
    }
}

fn deepseek_models() -> Vec<BrowserDiscoveredModel> {
    vec![
        BrowserDiscoveredModel {
            external_id: "deepseek-web-default".into(),
            display_name: "DeepSeek".into(),
            owned_by: "DeepSeek".into(),
            context_window: None,
            capabilities: vec!["chat".into(), "streaming".into(), "coding".into()],
        },
        BrowserDiscoveredModel {
            external_id: "deepseek-web-reasoning".into(),
            display_name: "DeepSeek Reasoning".into(),
            owned_by: "DeepSeek".into(),
            context_window: None,
            capabilities: vec![
                "chat".into(),
                "streaming".into(),
                "reasoning".into(),
                "coding".into(),
            ],
        },
        BrowserDiscoveredModel {
            external_id: "deepseek-web-expert".into(),
            display_name: "DeepSeek Expert".into(),
            owned_by: "DeepSeek".into(),
            context_window: None,
            capabilities: vec![
                "chat".into(),
                "streaming".into(),
                "reasoning".into(),
                "coding".into(),
            ],
        },
    ]
}

fn resolve_model(
    account_id: &str,
    requested: &str,
) -> Result<DeepSeekModelSelection, BrowserProviderError> {
    let normalized = requested.trim().to_ascii_lowercase();
    let (external_id, model_type, thinking_enabled) = match normalized.as_str() {
        "" | "deepseek-web-default" | "deepseek-chat" | "default" => {
            ("deepseek-web-default", "default", false)
        }
        "deepseek-web-reasoning" | "deepseek-reasoner" | "reasoning" | "think" => {
            ("deepseek-web-reasoning", "default", true)
        }
        "deepseek-web-expert" | "deepseek-pro" | "expert" => {
            ("deepseek-web-expert", "expert", true)
        }
        _ => {
            return Err(BrowserProviderError::ModelUnavailable {
                account_id: account_id.to_string(),
                model: requested.to_string(),
            })
        }
    };
    Ok(DeepSeekModelSelection {
        external_id: external_id.into(),
        model_type,
        thinking_enabled,
    })
}

fn serialize_prompt(body: &Value, native: bool) -> Result<String, BrowserProviderError> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            BrowserProviderError::InvalidConfig("DeepSeek request requires messages".into())
        })?;

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
            "DeepSeek native continuation requires a non-empty user message".into(),
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
            "DeepSeek request has no textual prompt".into(),
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
                    if item.get("type").and_then(Value::as_str) == Some("text") {
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

fn deepseek_user_token(material: &BrowserAuthMaterial) -> Option<String> {
    let raw = storage_value(
        material,
        &["userToken", "user_token", "accessToken", "access_token"],
    )?;
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
        for key in ["value", "token", "access_token", "accessToken"] {
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

fn client_version(material: &BrowserAuthMaterial) -> String {
    storage_value(
        material,
        &[
            "clientVersion",
            "client_version",
            "appVersion",
            "app_version",
            "version",
        ],
    )
    .filter(|value| {
        value.len() <= 32 && value.chars().any(|character| character.is_ascii_digit())
    })
    .unwrap_or_else(|| DEFAULT_CLIENT_VERSION.to_string())
}

fn response_timeout(binding: &BrowserAccountBinding) -> Duration {
    Duration::from_millis(binding.response_timeout_ms.unwrap_or(120_000))
}

fn deepseek_error_code(value: &Value) -> Option<i64> {
    value
        .get("code")
        .and_then(Value::as_i64)
        .filter(|code| *code != 0)
        .or_else(|| {
            value
                .pointer("/data/biz_code")
                .and_then(Value::as_i64)
                .filter(|code| *code != 0)
        })
}

fn error_message(value: &Value) -> String {
    value
        .get("msg")
        .or_else(|| value.pointer("/data/biz_msg"))
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("unknown DeepSeek error")
        .to_string()
}

fn value_to_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|id| id.to_string()))
        .or_else(|| value.as_u64().map(|id| id.to_string()))
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
    use crate::browser_auth::BrowserAuthMaterial;
    use std::collections::BTreeMap;

    #[test]
    fn token_normalization_accepts_deepseek_local_storage_shape() {
        let mut local_storage = BTreeMap::new();
        local_storage.insert(
            "userToken".into(),
            r#"{"value":"Bearer ds-user-token"}"#.into(),
        );
        let material = BrowserAuthMaterial::new(
            "session",
            "deepseek",
            DEEPSEEK_BASE_URL,
            "test-agent",
            Vec::new(),
            local_storage,
            BTreeMap::new(),
        );
        assert_eq!(
            deepseek_user_token(&material).as_deref(),
            Some("ds-user-token")
        );
    }

    #[test]
    fn model_selection_changes_wire_mode_and_thinking() {
        let default = resolve_model("account", "deepseek-web-default").unwrap();
        let reasoning = resolve_model("account", "deepseek-web-reasoning").unwrap();
        let expert = resolve_model("account", "deepseek-web-expert").unwrap();
        assert_eq!(default.model_type, "default");
        assert!(!default.thinking_enabled);
        assert_eq!(reasoning.model_type, "default");
        assert!(reasoning.thinking_enabled);
        assert_eq!(expert.model_type, "expert");
        assert!(expert.thinking_enabled);
        assert!(matches!(
            resolve_model("account", "not-a-model"),
            Err(BrowserProviderError::ModelUnavailable { .. })
        ));
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
    fn sse_decoder_tracks_reasoning_output_and_parent_id_across_chunks() {
        let mut decoder = DeepSeekSseDecoder::default();
        let first = b"event: ready\ndata: {\"data\":{\"response_message_id\":\"r1\"}}\n\ndata: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"type\":\"THINK\",\"content\":\"why \"}]}\n\n";
        let second = b"data: {\"p\":\"response/fragments/-1/content\",\"o\":\"APPEND\",\"v\":\"because\"}\n\ndata: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"type\":\"RESPONSE\",\"content\":\"hello\"}]}\n\nevent: close\ndata: {}\n\n";
        let mut state = DeepSeekStreamState::default();
        for update in decoder.push(first).unwrap() {
            state.apply(update).unwrap();
        }
        for update in decoder.push(second).unwrap() {
            state.apply(update).unwrap();
        }
        assert_eq!(state.response_message_id, "r1");
        assert_eq!(state.reasoning, "why because");
        assert_eq!(state.output, "hello");
        assert!(state.completed);
        state.validate_completion().unwrap();
    }

    #[test]
    fn incomplete_stream_is_never_accepted() {
        let state = DeepSeekStreamState {
            output: "partial".into(),
            response_message_id: "r1".into(),
            ..Default::default()
        };
        assert!(state
            .validate_completion()
            .unwrap_err()
            .contains("upstream_stream_dropped"));
    }
}
