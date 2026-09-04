use crate::{
    browser_auth::{BrowserAuthMaterial, BrowserAuthVault},
    browser_auth_runtime, conversation_runtime,
    browser_provider::{
        BrowserAccountBinding, BrowserAdapterDiagnostics, BrowserAdapterRequest,
        BrowserProviderAdapter, BrowserProviderError, BROWSER_ADAPTER_CONTRACT_VERSION,
    },
};
use async_trait::async_trait;
use axum::http::Response as HttpResponse;
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{
    header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, COOKIE, ORIGIN, REFERER, USER_AGENT},
    Client, Response,
};
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;

const GEMINI_HOST: &str = "gemini.google.com";
const GEMINI_INIT_URL: &str = "https://gemini.google.com/app";
const GEMINI_GENERATE_URL: &str =
    "https://gemini.google.com/_/BardChatUi/data/assistant.lamda.BardFrontendService/StreamGenerate";
const GEMINI_REFERER: &str = "https://gemini.google.com/";
const GEMINI_DEFAULT_LANGUAGE: &str = "en";
const GEMINI_ADAPTER_VERSION: &str = "experimental-1";
const USAGE_LIMIT_EXCEEDED: i64 = 1037;
const STREAM_REWRITE_HOLD_CHARS: usize = 192;

#[derive(Clone)]
pub struct GeminiWebHttpAdapter {
    client: Client,
}

#[derive(Clone, Debug)]
struct GeminiInitSession {
    access_token: String,
    build_label: String,
    frontend_session_id: String,
    language: String,
    cookie_header: String,
    user_agent: String,
}

#[derive(Clone, Debug, Default)]
struct GeminiFrameUpdate {
    metadata: Option<Value>,
    conversation_id: String,
    response_id: String,
    candidate_id: String,
    text: String,
    completed: bool,
    error_code: Option<i64>,
}

#[derive(Debug, Default)]
struct GeminiFrameDecoder {
    bytes: Vec<u8>,
    preamble_handled: bool,
}

#[derive(Debug, Default)]
struct StableTextEmitter {
    emitted: String,
    latest: String,
}

impl GeminiWebHttpAdapter {
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
                    "Gemini browserless auth material is unavailable; login with browser again: {error}"
                ),
            })?;
        if material
            .cookie_value_for_host(GEMINI_HOST, "__Secure-1PSID")
            .is_none()
        {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "login_required".into(),
                message: "Gemini auth snapshot is missing __Secure-1PSID; login with browser again"
                    .into(),
            });
        }
        Ok(material)
    }

    async fn init_session(
        &self,
        material: &BrowserAuthMaterial,
        account_id: &str,
        timeout_duration: Duration,
    ) -> Result<GeminiInitSession, BrowserProviderError> {
        let cookie_header = material.cookie_header_for_host(GEMINI_HOST);
        if cookie_header.is_empty() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "login_required".into(),
                message: "Gemini auth snapshot has no cookies valid for gemini.google.com".into(),
            });
        }

        let mut request = self
            .client
            .get(GEMINI_INIT_URL)
            .header(COOKIE, &cookie_header)
            .header(REFERER, GEMINI_REFERER)
            .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .timeout(timeout_duration);
        if !material.user_agent.trim().is_empty() {
            request = request.header(USER_AGENT, material.user_agent.trim());
        }

        let response = request
            .send()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "login_required".into(),
                message: format!("Gemini session bootstrap rejected saved auth with HTTP {status}"),
            });
        }
        if status.as_u16() == 429 {
            return Err(BrowserProviderError::Transport(
                "Gemini session bootstrap was rate limited (HTTP 429)".into(),
            ));
        }
        if !status.is_success() {
            return Err(BrowserProviderError::Transport(format!(
                "Gemini session bootstrap returned HTTP {status}"
            )));
        }

        let html = response
            .text()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let access_token = extract_embedded_json_string(&html, "SNlM0e").ok_or_else(|| {
            BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "login_required".into(),
                message: "Gemini bootstrap returned no SNlM0e token; saved login is expired or the web protocol changed"
                    .into(),
            }
        })?;

        Ok(GeminiInitSession {
            access_token,
            build_label: extract_embedded_json_string(&html, "cfb2h").unwrap_or_default(),
            frontend_session_id: extract_embedded_json_string(&html, "FdrFJe")
                .unwrap_or_default(),
            language: extract_embedded_json_string(&html, "TuX5cc")
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| GEMINI_DEFAULT_LANGUAGE.into()),
            cookie_header,
            user_agent: material.user_agent.clone(),
        })
    }

    async fn submit_generation(
        &self,
        request: &BrowserAdapterRequest,
        session: &GeminiInitSession,
        prompt: &str,
        metadata: Value,
    ) -> Result<Response, BrowserProviderError> {
        if request.route.model != "gemini-web-default" {
            return Err(BrowserProviderError::ModelUnavailable {
                account_id: request.account.id.clone(),
                model: request.route.model.clone(),
            });
        }

        let request_uuid = Uuid::new_v4().to_string().to_uppercase();
        let inner = build_inner_request(
            prompt,
            &session.language,
            metadata,
            &request_uuid,
            request.binding.ephemeral_chat.unwrap_or(false) && request.thread_id.is_none(),
        );
        let f_req = serde_json::to_string(&json!([
            Value::Null,
            serde_json::to_string(&inner)
                .map_err(|error| BrowserProviderError::Transport(error.to_string()))?
        ]))
        .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;

        let mut query: Vec<(&str, String)> = vec![
            ("rt", "c".into()),
            ("_reqid", request_id().to_string()),
            ("hl", session.language.clone()),
        ];
        if !session.build_label.is_empty() {
            query.push(("bl", session.build_label.clone()));
        }
        if !session.frontend_session_id.is_empty() {
            query.push(("f.sid", session.frontend_session_id.clone()));
        }

        let mut upstream = self
            .client
            .post(GEMINI_GENERATE_URL)
            .query(&query)
            .header(
                CONTENT_TYPE,
                "application/x-www-form-urlencoded;charset=utf-8",
            )
            .header(ORIGIN, "https://gemini.google.com")
            .header(REFERER, GEMINI_REFERER)
            .header("x-same-domain", "1")
            .header(
                "x-goog-ext-525005358-jspb",
                format!("[\"{request_uuid}\",1]"),
            )
            .header(ACCEPT, "*/*")
            .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .header(COOKIE, &session.cookie_header)
            .form(&[("f.req", f_req), ("at", session.access_token.clone())])
            .timeout(Duration::from_millis(
                request.binding.response_timeout_ms.unwrap_or(180_000),
            ));
        if !session.user_agent.trim().is_empty() {
            upstream = upstream.header(USER_AGENT, session.user_agent.trim());
        }

        let response = upstream
            .send()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "login_required".into(),
                message: format!("Gemini StreamGenerate rejected saved auth with HTTP {status}"),
            });
        }
        if status.as_u16() == 429 {
            return Err(BrowserProviderError::Transport(
                "Gemini web usage endpoint was rate limited (HTTP 429)".into(),
            ));
        }
        if !status.is_success() {
            return Err(BrowserProviderError::Transport(format!(
                "Gemini StreamGenerate returned HTTP {status}"
            )));
        }
        Ok(response)
    }

    async fn conversation_metadata(
        &self,
        request: &BrowserAdapterRequest,
    ) -> Result<Value, BrowserProviderError> {
        let Some(thread_id) = request.thread_id.as_deref() else {
            return Ok(default_metadata());
        };
        let Some(store) = conversation_runtime::get() else {
            return Ok(default_metadata());
        };
        let state = store
            .provider_conversation_state(thread_id, &request.provider.id, &request.account.id)
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        Ok(state
            .and_then(|value| value.get("metadata").cloned())
            .unwrap_or_else(default_metadata))
    }

    async fn persist_conversation_state(
        thread_id: Option<&str>,
        provider_id: &str,
        account_id: &str,
        update: &GeminiFrameUpdate,
    ) -> Result<(), BrowserProviderError> {
        let Some(thread_id) = thread_id else {
            return Ok(());
        };
        let Some(store) = conversation_runtime::get() else {
            return Ok(());
        };
        let Some(metadata) = update.metadata.as_ref() else {
            return Ok(());
        };

        let state = json!({
            "transport": "gemini-http",
            "metadata": metadata,
            "conversation_id": update.conversation_id,
            "response_id": update.response_id,
            "candidate_id": update.candidate_id,
        });
        store
            .upsert_provider_conversation_state(thread_id, provider_id, account_id, &state)
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;

        if !update.conversation_id.is_empty() {
            let url = format!("https://gemini.google.com/app/{}", update.conversation_id);
            store
                .upsert_provider_conversation(thread_id, provider_id, account_id, &url)
                .await
                .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        }
        Ok(())
    }

    async fn buffered_response(
        &self,
        request: &BrowserAdapterRequest,
        response: Response,
    ) -> Result<reqwest::Response, BrowserProviderError> {
        let raw = response
            .bytes()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let updates = decode_complete_response(&raw)?;
        let mut latest = GeminiFrameUpdate::default();
        for update in updates {
            if let Some(error_code) = update.error_code {
                if error_code == USAGE_LIMIT_EXCEEDED {
                    return Err(BrowserProviderError::Transport(
                        "Gemini web usage limit exceeded".into(),
                    ));
                }
                if error_code != 0 {
                    return Err(BrowserProviderError::Transport(format!(
                        "Gemini StreamGenerate returned error code {error_code}"
                    )));
                }
            }
            merge_update(&mut latest, update);
        }

        if latest.text.is_empty() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "empty_response".into(),
                message: "Gemini StreamGenerate completed without assistant text".into(),
            });
        }

        Self::persist_conversation_state(
            request.thread_id.as_deref(),
            &request.provider.id,
            &request.account.id,
            &latest,
        )
        .await?;

        let completion_id = format!("chatcmpl_gemini_{}", Uuid::new_v4().simple());
        let body = json!({
            "id": completion_id,
            "object": "chat.completion",
            "created": chrono::Utc::now().timestamp(),
            "model": request.route.model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": latest.text},
                "finish_reason": "stop"
            }]
        });
        synthetic_json_response(body)
    }

    fn streaming_response(
        &self,
        request: &BrowserAdapterRequest,
        response: Response,
    ) -> Result<reqwest::Response, BrowserProviderError> {
        let provider_id = request.provider.id.clone();
        let account_id = request.account.id.clone();
        let thread_id = request.thread_id.clone();
        let model = request.route.model.clone();
        let completion_id = format!("chatcmpl_gemini_{}", Uuid::new_v4().simple());
        let created = chrono::Utc::now().timestamp();

        let stream = async_stream::stream! {
            let mut upstream = response.bytes_stream();
            let mut decoder = GeminiFrameDecoder::default();
            let mut emitter = StableTextEmitter::default();
            let mut latest = GeminiFrameUpdate::default();
            let mut saw_text = false;
            let mut finished = false;

            'outer: while let Some(chunk) = upstream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        yield Err(std::io::Error::other(format!(
                            "Gemini browserless stream body failed: {error}"
                        )));
                        break;
                    }
                };

                let frames = match decoder.push(&chunk) {
                    Ok(frames) => frames,
                    Err(error) => {
                        yield Err(std::io::Error::other(error));
                        break;
                    }
                };

                for part in frames {
                    let update = parse_frame_update(&part);
                    if let Some(error_code) = update.error_code {
                        let message = if error_code == USAGE_LIMIT_EXCEEDED {
                            "Gemini web usage limit exceeded".to_string()
                        } else {
                            format!("Gemini StreamGenerate returned error code {error_code}")
                        };
                        yield Err(std::io::Error::other(message));
                        break 'outer;
                    }

                    merge_update(&mut latest, update);
                    match emitter.observe(&latest.text, latest.completed) {
                        Ok(Some(delta)) if !delta.is_empty() => {
                            saw_text = true;
                            let event = json!({
                                "id": completion_id,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model,
                                "choices": [{
                                    "index": 0,
                                    "delta": {"content": delta},
                                    "finish_reason": Value::Null
                                }]
                            });
                            yield Ok(Bytes::from(format!("data: {event}\n\n")));
                        }
                        Ok(_) => {}
                        Err(error) => {
                            yield Err(std::io::Error::other(error));
                            break 'outer;
                        }
                    }

                    if latest.completed {
                        finished = true;
                        break 'outer;
                    }
                }
            }

            if !finished {
                match decoder.finish() {
                    Ok(frames) => {
                        for part in frames {
                            let update = parse_frame_update(&part);
                            if let Some(error_code) = update.error_code {
                                yield Err(std::io::Error::other(format!(
                                    "Gemini StreamGenerate returned error code {error_code}"
                                )));
                                return;
                            }
                            merge_update(&mut latest, update);
                        }
                    }
                    Err(error) => {
                        yield Err(std::io::Error::other(error));
                        return;
                    }
                }
            }

            match emitter.observe(&latest.text, true) {
                Ok(Some(delta)) if !delta.is_empty() => {
                    saw_text = true;
                    let event = json!({
                        "id": completion_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "delta": {"content": delta},
                            "finish_reason": Value::Null
                        }]
                    });
                    yield Ok(Bytes::from(format!("data: {event}\n\n")));
                }
                Ok(_) => {}
                Err(error) => {
                    yield Err(std::io::Error::other(error));
                    return;
                }
            }

            if !saw_text {
                yield Err(std::io::Error::other(
                    "Gemini browserless stream completed without assistant output"
                ));
                return;
            }

            if let Err(error) = Self::persist_conversation_state(
                thread_id.as_deref(),
                &provider_id,
                &account_id,
                &latest,
            ).await {
                yield Err(std::io::Error::other(error.to_string()));
                return;
            }

            let final_event = json!({
                "id": completion_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop"
                }]
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
impl BrowserProviderAdapter for GeminiWebHttpAdapter {
    fn kind(&self) -> &'static str {
        "browser-gemini-http"
    }

    fn adapter_id(&self) -> &'static str {
        "gemini-web-http"
    }

    async fn diagnose(
        &self,
        account_id: &str,
        _profile_dir: &str,
        binding: &BrowserAccountBinding,
    ) -> BrowserAdapterDiagnostics {
        let result = async {
            let material = self.auth_material(&binding.session, account_id)?;
            self.init_session(
                &material,
                account_id,
                Duration::from_millis(binding.probe_timeout_ms.unwrap_or(8_000).max(3_000)),
            )
            .await
        }
        .await;

        match result {
            Ok(_) => BrowserAdapterDiagnostics {
                account_id: account_id.to_string(),
                provider_kind: "browser-gemini".into(),
                adapter_id: Some(self.adapter_id().into()),
                adapter_version: Some(GEMINI_ADAPTER_VERSION.into()),
                contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
                expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                status: "ready".into(),
                message: "Gemini direct HTTP session is ready; Chromium is not required for chat"
                    .into(),
                page_signature: None,
                target_url_prefix: Some(GEMINI_INIT_URL.into()),
                configured_models: binding.models.clone(),
            },
            Err(BrowserProviderError::AdapterIncompatible { code, message, .. }) => {
                BrowserAdapterDiagnostics {
                    account_id: account_id.to_string(),
                    provider_kind: "browser-gemini".into(),
                    adapter_id: Some(self.adapter_id().into()),
                    adapter_version: Some(GEMINI_ADAPTER_VERSION.into()),
                    contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
                    expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                    status: if code == "login_required" {
                        "login_required".into()
                    } else {
                        "adapter_incompatible".into()
                    },
                    message,
                    page_signature: None,
                    target_url_prefix: Some(GEMINI_INIT_URL.into()),
                    configured_models: binding.models.clone(),
                }
            }
            Err(error) => BrowserAdapterDiagnostics {
                account_id: account_id.to_string(),
                provider_kind: "browser-gemini".into(),
                adapter_id: Some(self.adapter_id().into()),
                adapter_version: Some(GEMINI_ADAPTER_VERSION.into()),
                contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
                expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
                status: "unavailable".into(),
                message: error.to_string(),
                page_signature: None,
                target_url_prefix: Some(GEMINI_INIT_URL.into()),
                configured_models: binding.models.clone(),
            },
        }
    }

    async fn execute_chat(
        &self,
        request: BrowserAdapterRequest,
    ) -> Result<reqwest::Response, BrowserProviderError> {
        let material = self.auth_material(&request.session_id, &request.account.id)?;
        let session = self
            .init_session(
                &material,
                &request.account.id,
                Duration::from_millis(request.binding.probe_timeout_ms.unwrap_or(8_000).max(3_000)),
            )
            .await?;
        let metadata = self.conversation_metadata(&request).await?;
        let has_native_state = metadata != default_metadata();
        let prompt = serialize_prompt(&request.body, has_native_state)?;
        let response = self
            .submit_generation(&request, &session, &prompt, metadata)
            .await?;

        if request
            .body
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            self.streaming_response(&request, response)
        } else {
            self.buffered_response(&request, response).await
        }
    }
}

fn request_id() -> u64 {
    let raw = Uuid::new_v4().as_u128();
    10_000 + (raw % 90_000) as u64
}

fn default_metadata() -> Value {
    json!(["", "", "", null, null, null, null, null, null, ""])
}

fn build_inner_request(
    prompt: &str,
    language: &str,
    metadata: Value,
    request_uuid: &str,
    temporary: bool,
) -> Vec<Value> {
    let mut inner = vec![Value::Null; 81];
    inner[0] = json!([prompt, 0, null, null, null, null, 0]);
    inner[1] = json!([language]);
    inner[2] = metadata;
    inner[6] = json!([1]);
    inner[7] = json!(1);
    inner[10] = json!(1);
    inner[11] = json!(0);
    inner[17] = json!([[0]]);
    inner[18] = json!(0);
    inner[27] = json!(1);
    inner[30] = json!([4]);
    inner[41] = json!([1]);
    if temporary {
        inner[45] = json!(1);
    }
    inner[53] = json!(0);
    inner[59] = json!(request_uuid);
    inner[61] = json!([]);
    inner[68] = json!(1);
    inner[79] = json!(1);
    inner[80] = json!(1);
    inner
}

fn serialize_prompt(body: &Value, native_continuation: bool) -> Result<String, BrowserProviderError> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| BrowserProviderError::InvalidConfig(
            "Gemini browserless chat requires an OpenAI-style messages array".into(),
        ))?;

    if native_continuation {
        if let Some(message) = messages.iter().rev().find(|message| {
            message.get("role").and_then(Value::as_str) == Some("user")
        }) {
            let text = content_text(message.get("content").unwrap_or(&Value::Null));
            if !text.trim().is_empty() {
                return Ok(text);
            }
        }
    }

    let mut rendered = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let text = content_text(message.get("content").unwrap_or(&Value::Null));
        if text.trim().is_empty() {
            continue;
        }
        let label = match role {
            "system" => "System",
            "assistant" => "Assistant",
            "tool" => "Tool",
            _ => "User",
        };
        rendered.push(format!("{label}: {text}"));
    }

    let prompt = rendered.join("\n\n");
    if prompt.trim().is_empty() {
        return Err(BrowserProviderError::InvalidConfig(
            "Gemini browserless chat requires at least one text message".into(),
        ));
    }
    Ok(prompt)
}

fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(text.clone()),
                Value::Object(object) => object
                    .get("text")
                    .or_else(|| object.get("content"))
                    .map(content_text)
                    .filter(|text| !text.is_empty()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(object) => object
            .get("text")
            .or_else(|| object.get("content"))
            .map(content_text)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn extract_embedded_json_string(html: &str, key: &str) -> Option<String> {
    let marker = format!("\"{key}\"");
    let start = html.find(&marker)? + marker.len();
    let tail = html.get(start..)?;
    let colon = tail.find(':')?;
    let value = tail.get(colon + 1..)?.trim_start();
    if !value.starts_with('"') {
        return None;
    }

    let mut escaped = false;
    for (offset, character) in value.char_indices().skip(1) {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if character == '"' {
            return serde_json::from_str::<String>(&value[..=offset]).ok();
        }
    }
    None
}

impl GeminiFrameDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<Value>, String> {
        self.bytes.extend_from_slice(chunk);
        self.decode_available(false)
    }

    fn finish(&mut self) -> Result<Vec<Value>, String> {
        let frames = self.decode_available(true)?;
        if self.bytes.iter().any(|byte| !byte.is_ascii_whitespace()) {
            return Err("Gemini browserless stream ended with a truncated frame".into());
        }
        self.bytes.clear();
        Ok(frames)
    }

    fn decode_available(&mut self, final_input: bool) -> Result<Vec<Value>, String> {
        let valid_len = match std::str::from_utf8(&self.bytes) {
            Ok(_) => self.bytes.len(),
            Err(error) if error.error_len().is_none() && !final_input => error.valid_up_to(),
            Err(error) => {
                return Err(format!("Gemini browserless stream returned invalid UTF-8: {error}"))
            }
        };
        if valid_len == 0 {
            return Ok(Vec::new());
        }

        let text = std::str::from_utf8(&self.bytes[..valid_len])
            .map_err(|error| error.to_string())?;
        let (frames, consumed) = parse_length_prefixed_frames(text, &mut self.preamble_handled)?;
        if consumed > 0 {
            self.bytes.drain(..consumed);
        }
        Ok(frames)
    }
}

fn parse_length_prefixed_frames(
    text: &str,
    preamble_handled: &mut bool,
) -> Result<(Vec<Value>, usize), String> {
    let mut pos = 0usize;
    let mut frames = Vec::new();

    if !*preamble_handled {
        let trimmed = text.trim_start_matches(char::is_whitespace);
        let whitespace = text.len() - trimmed.len();
        if trimmed.len() < 4 && ")]}'".starts_with(trimmed) {
            return Ok((frames, 0));
        }
        if trimmed.starts_with(")]}'") {
            pos = whitespace + 4;
        }
        *preamble_handled = true;
    }

    loop {
        while let Some(character) = text[pos..].chars().next() {
            if !character.is_whitespace() {
                break;
            }
            pos += character.len_utf8();
            if pos >= text.len() {
                return Ok((frames, pos));
            }
        }
        if pos >= text.len() {
            return Ok((frames, pos));
        }

        let digit_start = pos;
        while pos < text.len() && text.as_bytes()[pos].is_ascii_digit() {
            pos += 1;
        }
        if digit_start == pos {
            return Err(format!(
                "Gemini browserless stream frame length marker was invalid near byte {pos}"
            ));
        }
        if pos >= text.len() {
            return Ok((frames, digit_start));
        }
        if text.as_bytes()[pos] != b'\n' {
            return Err("Gemini browserless stream frame length was not newline terminated".into());
        }
        let length: usize = text[digit_start..pos]
            .parse()
            .map_err(|error| format!("invalid Gemini frame length: {error}"))?;

        // Gemini's frame length starts at the newline following the decimal marker and
        // counts UTF-16 code units, matching the browser's JavaScript string semantics.
        let content_start = pos;
        let Some(content_end) = byte_index_after_utf16_units(text, content_start, length) else {
            return Ok((frames, digit_start));
        };
        let chunk = text[content_start..content_end].trim();
        pos = content_end;
        if chunk.is_empty() {
            continue;
        }

        let value: Value = serde_json::from_str(chunk)
            .map_err(|error| format!("invalid Gemini stream JSON frame: {error}"))?;
        if let Value::Array(items) = value {
            frames.extend(items);
        } else {
            frames.push(value);
        }
    }
}

fn byte_index_after_utf16_units(text: &str, start: usize, units: usize) -> Option<usize> {
    let mut consumed_units = 0usize;
    for (relative, character) in text.get(start..)?.char_indices() {
        if consumed_units == units {
            return Some(start + relative);
        }
        let width = character.len_utf16();
        if consumed_units + width > units {
            return None;
        }
        consumed_units += width;
    }
    (consumed_units == units).then_some(text.len())
}

fn parse_frame_update(part: &Value) -> GeminiFrameUpdate {
    let error_code = nested(part, &[5, 2, 0, 1, 0]).and_then(Value::as_i64);
    let Some(inner_raw) = nested(part, &[2]).and_then(Value::as_str) else {
        return GeminiFrameUpdate {
            error_code,
            ..Default::default()
        };
    };
    let Ok(inner) = serde_json::from_str::<Value>(inner_raw) else {
        return GeminiFrameUpdate {
            error_code,
            ..Default::default()
        };
    };

    let metadata = nested(&inner, &[1]).cloned();
    let conversation_id = metadata
        .as_ref()
        .and_then(|value| nested(value, &[0]))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let response_id = metadata
        .as_ref()
        .and_then(|value| nested(value, &[1]))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut candidate_id = String::new();
    let mut text = String::new();
    let mut completed = false;
    if let Some(candidates) = nested(&inner, &[4]).and_then(Value::as_array) {
        for candidate in candidates {
            if candidate_id.is_empty() {
                candidate_id = nested(candidate, &[0])
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            }
            if text.is_empty() {
                text = nested(candidate, &[1, 0])
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            }
            completed |= nested(candidate, &[8, 0]).and_then(Value::as_i64) == Some(2);
        }
    }

    GeminiFrameUpdate {
        metadata,
        conversation_id,
        response_id,
        candidate_id,
        text,
        completed,
        error_code,
    }
}

fn nested<'a>(value: &'a Value, path: &[usize]) -> Option<&'a Value> {
    let mut current = value;
    for index in path {
        current = current.as_array()?.get(*index)?;
    }
    Some(current)
}

fn merge_update(target: &mut GeminiFrameUpdate, next: GeminiFrameUpdate) {
    if next.metadata.is_some() {
        target.metadata = next.metadata;
    }
    if !next.conversation_id.is_empty() {
        target.conversation_id = next.conversation_id;
    }
    if !next.response_id.is_empty() {
        target.response_id = next.response_id;
    }
    if !next.candidate_id.is_empty() {
        target.candidate_id = next.candidate_id;
    }
    if !next.text.is_empty() {
        target.text = next.text;
    }
    target.completed |= next.completed;
    if next.error_code.is_some() {
        target.error_code = next.error_code;
    }
}

impl StableTextEmitter {
    fn observe(&mut self, snapshot: &str, completed: bool) -> Result<Option<String>, String> {
        if snapshot.is_empty() {
            return Ok(None);
        }
        if !snapshot.starts_with(&self.emitted) {
            return Err(
                "stream_rewrite_detected: Gemini rewrote text outside the browserless stability window"
                    .into(),
            );
        }
        self.latest.clear();
        self.latest.push_str(snapshot);

        let commit_end = if completed {
            snapshot.len()
        } else {
            prefix_byte_len_excluding_tail(snapshot, STREAM_REWRITE_HOLD_CHARS)
        };
        if commit_end <= self.emitted.len() {
            return Ok(None);
        }
        let delta = snapshot[self.emitted.len()..commit_end].to_string();
        self.emitted.push_str(&delta);
        Ok(Some(delta))
    }
}

fn prefix_byte_len_excluding_tail(text: &str, tail_chars: usize) -> usize {
    let total = text.chars().count();
    if total <= tail_chars {
        return 0;
    }
    let keep = total - tail_chars;
    text.char_indices()
        .nth(keep)
        .map(|(index, _)| index)
        .unwrap_or(text.len())
}

fn decode_complete_response(raw: &[u8]) -> Result<Vec<GeminiFrameUpdate>, BrowserProviderError> {
    let mut decoder = GeminiFrameDecoder::default();
    let mut parts = decoder
        .push(raw)
        .map_err(BrowserProviderError::Transport)?;
    parts.extend(
        decoder
            .finish()
            .map_err(BrowserProviderError::Transport)?,
    );
    Ok(parts.iter().map(parse_frame_update).collect())
}

fn synthetic_json_response(body: Value) -> Result<reqwest::Response, BrowserProviderError> {
    let encoded = serde_json::to_vec(&body)
        .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
    let response = HttpResponse::builder()
        .status(200)
        .header(CONTENT_TYPE, "application/json")
        .body(reqwest::Body::from(encoded))
        .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
    Ok(reqwest::Response::from(response))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_embedded_init_fields() {
        let html = r#"<script>{"SNlM0e":"token-123","cfb2h":"boq_test","FdrFJe":"sid","TuX5cc":"en"}</script>"#;
        assert_eq!(
            extract_embedded_json_string(html, "SNlM0e").as_deref(),
            Some("token-123")
        );
        assert_eq!(
            extract_embedded_json_string(html, "FdrFJe").as_deref(),
            Some("sid")
        );
    }

    #[test]
    fn serializes_only_latest_user_turn_for_native_continuation() {
        let body = json!({
            "messages": [
                {"role":"user","content":"first"},
                {"role":"assistant","content":"answer"},
                {"role":"user","content":"second"}
            ]
        });
        assert_eq!(serialize_prompt(&body, true).unwrap(), "second");
        assert!(serialize_prompt(&body, false).unwrap().contains("User: first"));
    }

    #[test]
    fn stable_emitter_holds_a_rewrite_window() {
        let mut emitter = StableTextEmitter::default();
        let prefix = "a".repeat(STREAM_REWRITE_HOLD_CHARS + 10);
        let first = emitter.observe(&prefix, false).unwrap().unwrap();
        assert_eq!(first.len(), 10);

        let mut rewritten = prefix.clone();
        rewritten.replace_range(20..21, "b");
        assert!(emitter.observe(&rewritten, false).is_ok());

        let mut committed_rewrite = prefix;
        committed_rewrite.replace_range(0..1, "b");
        assert!(emitter.observe(&committed_rewrite, false).is_err());
    }

    #[test]
    fn parses_gemini_length_prefixed_frame() {
        let inner = json!([
            null,
            ["c_test", "r_test"],
            null,
            null,
            [["rc_test", ["hello"], null, null, null, null, null, null, [2]]]
        ]);
        let part = json!([["wrb.fr"], null, inner.to_string()]);
        let json_frame = serde_json::to_string(&vec![part]).unwrap();
        let payload = format!("\n{}\n{}\n", json_frame.encode_utf16().count() + 1, json_frame);
        let mut handled = true;
        let (parts, _) = parse_length_prefixed_frames(&payload, &mut handled).unwrap();
        assert_eq!(parts.len(), 1);
        let update = parse_frame_update(&parts[0]);
        assert_eq!(update.conversation_id, "c_test");
        assert_eq!(update.response_id, "r_test");
        assert_eq!(update.text, "hello");
        assert!(update.completed);
    }
}
