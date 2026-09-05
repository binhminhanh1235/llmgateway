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
use futures_util::StreamExt;
use reqwest::{
    header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, COOKIE, ORIGIN, REFERER, USER_AGENT},
    Client, Response,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use uuid::Uuid;

const GEMINI_HOST: &str = "gemini.google.com";
const GEMINI_INIT_URL: &str = "https://gemini.google.com/app";
const GEMINI_GENERATE_URL: &str =
    "https://gemini.google.com/_/BardChatUi/data/assistant.lamda.BardFrontendService/StreamGenerate";
const GEMINI_BATCH_EXEC_URL: &str = "https://gemini.google.com/_/BardChatUi/data/batchexecute";
const GEMINI_GET_USER_STATUS_RPC: &str = "otAQ7b";
const GEMINI_MODEL_HEADER: &str = "x-goog-ext-525001261-jspb";
const GEMINI_MODEL_AUX_HEADER_1: &str = "x-goog-ext-73010989-jspb";
const GEMINI_MODEL_AUX_HEADER_2: &str = "x-goog-ext-73010990-jspb";
const GEMINI_REFERER: &str = "https://gemini.google.com/";
const GEMINI_DEFAULT_LANGUAGE: &str = "en";
const GEMINI_DEFAULT_MODEL: &str = "gemini-web-default";
const GEMINI_ADAPTER_VERSION: &str = "experimental-2";
const USAGE_LIMIT_EXCEEDED: i64 = 1037;
const MODEL_HEADER_INVALID: i64 = 1052;
const STREAM_REWRITE_HOLD_CHARS: usize = 192;
const MODEL_CATALOG_TTL: Duration = Duration::from_secs(600);

#[derive(Clone)]
pub struct GeminiWebHttpAdapter {
    client: Client,
    model_catalogs: Arc<RwLock<BTreeMap<String, GeminiModelCatalogSnapshot>>>,
}

#[derive(Clone, Debug)]
struct GeminiModelRecipe {
    external_id: String,
    display_name: String,
    description: String,
    model_id: String,
    capacity: i64,
    capacity_field: usize,
    model_number: i64,
    aliases: BTreeSet<String>,
    capabilities: Vec<String>,
}

#[derive(Clone, Debug)]
struct GeminiModelCatalogSnapshot {
    discovered_at: Instant,
    wire_session_id: String,
    models: Vec<GeminiModelRecipe>,
}

#[derive(Clone, Debug)]
struct GeminiModelSelection {
    recipe: GeminiModelRecipe,
    wire_session_id: String,
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
        Ok(Self {
            client,
            model_catalogs: Arc::new(RwLock::new(BTreeMap::new())),
        })
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

    async fn model_catalog(
        &self,
        binding: &BrowserAccountBinding,
        account_id: &str,
        force: bool,
    ) -> Result<GeminiModelCatalogSnapshot, BrowserProviderError> {
        if !force {
            if let Some(cached) = self.model_catalogs.read().await.get(account_id).cloned() {
                if cached.discovered_at.elapsed() <= MODEL_CATALOG_TTL {
                    return Ok(cached);
                }
            }
        }

        let material = self.auth_material(&binding.session, account_id)?;
        let timeout_duration =
            Duration::from_millis(binding.probe_timeout_ms.unwrap_or(8_000).max(3_000));
        let session = self
            .init_session(&material, account_id, timeout_duration)
            .await?;
        let snapshot = self
            .fetch_model_catalog(&session, account_id, timeout_duration)
            .await?;
        self.model_catalogs
            .write()
            .await
            .insert(account_id.to_string(), snapshot.clone());
        if let Some(registry) = browser_provider_runtime::get() {
            registry.clear_model_catalog_refresh_required(account_id);
        }
        Ok(snapshot)
    }

    async fn fetch_model_catalog(
        &self,
        session: &GeminiInitSession,
        account_id: &str,
        timeout_duration: Duration,
    ) -> Result<GeminiModelCatalogSnapshot, BrowserProviderError> {
        let wire_session_id = Uuid::new_v4().to_string().to_uppercase();
        let batch_header = serde_json::to_string(&json!([
            1,
            null,
            null,
            null,
            null,
            null,
            null,
            null,
            [4, 5, 6, 8],
            null,
            null,
            null,
            null,
            null,
            null,
            null,
            wire_session_id
        ]))
        .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let f_req = serde_json::to_string(&json!([[[
            GEMINI_GET_USER_STATUS_RPC,
            "[]",
            null,
            "generic"
        ]]]))
        .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;

        let mut query: Vec<(&str, String)> = vec![
            ("rpcids", GEMINI_GET_USER_STATUS_RPC.into()),
            ("hl", session.language.clone()),
            ("_reqid", request_id().to_string()),
            ("rt", "c".into()),
            ("source-path", "/app".into()),
        ];
        if !session.build_label.is_empty() {
            query.push(("bl", session.build_label.clone()));
        }
        if !session.frontend_session_id.is_empty() {
            query.push(("f.sid", session.frontend_session_id.clone()));
        }

        let mut upstream = self
            .client
            .post(GEMINI_BATCH_EXEC_URL)
            .query(&query)
            .header(
                CONTENT_TYPE,
                "application/x-www-form-urlencoded;charset=utf-8",
            )
            .header(ORIGIN, "https://gemini.google.com")
            .header(REFERER, GEMINI_REFERER)
            .header("x-same-domain", "1")
            .header(GEMINI_MODEL_HEADER, batch_header)
            .header(GEMINI_MODEL_AUX_HEADER_1, "[0]")
            .header(COOKIE, &session.cookie_header)
            .form(&[("at", session.access_token.clone()), ("f.req", f_req)])
            .timeout(timeout_duration);
        if !session.user_agent.trim().is_empty() {
            upstream = upstream.header(USER_AGENT, session.user_agent.trim());
        }

        let response = upstream
            .send()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403) {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "login_required".into(),
                message: format!("Gemini model discovery rejected saved auth with HTTP {status}"),
            });
        }
        if !status.is_success() {
            return Err(BrowserProviderError::Transport(format!(
                "Gemini model discovery returned HTTP {status}"
            )));
        }
        let raw = response
            .bytes()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let models = parse_model_catalog_response(&raw)?;
        if models.is_empty() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "model_discovery_empty".into(),
                message: "Gemini GET_USER_STATUS returned no selectable models".into(),
            });
        }

        Ok(GeminiModelCatalogSnapshot {
            discovered_at: Instant::now(),
            wire_session_id,
            models,
        })
    }

    async fn resolve_model_selection(
        &self,
        request: &BrowserAdapterRequest,
    ) -> Result<Option<GeminiModelSelection>, BrowserProviderError> {
        if request.route.model == GEMINI_DEFAULT_MODEL {
            return Ok(None);
        }
        let snapshot = self
            .model_catalog(&request.binding, &request.account.id, false)
            .await?;
        let recipe = find_model_recipe(&snapshot, &request.route.model)
            .ok_or_else(|| BrowserProviderError::ModelUnavailable {
                account_id: request.account.id.clone(),
                model: request.route.model.clone(),
            })?;
        Ok(Some(GeminiModelSelection {
            recipe,
            wire_session_id: snapshot.wire_session_id,
        }))
    }

    async fn invalidate_model_catalog(&self, account_id: &str) {
        self.model_catalogs.write().await.remove(account_id);
        if let Some(registry) = browser_provider_runtime::get() {
            registry.mark_model_catalog_refresh_required(account_id);
        }
    }

    async fn submit_generation(
        &self,
        request: &BrowserAdapterRequest,
        session: &GeminiInitSession,
        prompt: &str,
        metadata: Value,
        selection: Option<&GeminiModelSelection>,
    ) -> Result<Response, BrowserProviderError> {
        let request_uuid = Uuid::new_v4().to_string().to_uppercase();
        let inner = build_inner_request(
            prompt,
            &session.language,
            metadata,
            &request_uuid,
            request.binding.ephemeral_chat.unwrap_or(false) && request.thread_id.is_none(),
            selection.map_or(1, |selected| selected.recipe.model_number),
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
        if let Some(selected) = selection {
            upstream = upstream
                .header(
                    GEMINI_MODEL_HEADER,
                    model_header_value(&selected.recipe, &selected.wire_session_id)?,
                )
                .header(GEMINI_MODEL_AUX_HEADER_1, "[0]")
                .header(GEMINI_MODEL_AUX_HEADER_2, "[0,0,0]");
        }
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
        requested_model: &str,
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
        let needs_resync = state
            .as_ref()
            .and_then(|value| value.get("needs_resync"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let browser_conversation_exists = if state.is_none() {
            store
                .provider_conversation(thread_id, &request.provider.id, &request.account.id)
                .await
                .map_err(|error| BrowserProviderError::Transport(error.to_string()))?
                .is_some()
        } else {
            false
        };
        if needs_resync || browser_conversation_exists {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "direct_state_unsynced".into(),
                message: "Gemini native conversation state was advanced by the browser adapter; keep this thread on CDP to preserve exact conversation affinity".into(),
            });
        }
        validate_thread_model_affinity(state.as_ref(), requested_model).map_err(|message| {
            // This is intentionally a Transport error even though it is detected before submit:
            // BrowserProviderRegistry only falls back to CDP for AdapterIncompatible /
            // ModelUnavailable. A model switch on an existing native Gemini conversation must
            // never be "recovered" by sending the turn through the browser.
            BrowserProviderError::Transport(message)
        })?;
        Ok(state
            .and_then(|value| value.get("metadata").cloned())
            .unwrap_or_else(default_metadata))
    }

    async fn persist_conversation_state(
        thread_id: Option<&str>,
        provider_id: &str,
        account_id: &str,
        model_external_id: &str,
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
            "model_external_id": model_external_id,
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
                if error_code != 0 {
                    if error_code == MODEL_HEADER_INVALID {
                        self.invalidate_model_catalog(&request.account.id).await;
                    }
                    return Err(generation_error(
                        error_code,
                        &request.account.id,
                        &request.route.model,
                    ));
                }
            }
            merge_update(&mut latest, update);
        }

        if !latest.completed {
            return Err(BrowserProviderError::Transport(
                "Gemini StreamGenerate ended before the provider completion marker".into(),
            ));
        }
        if latest.text.is_empty() {
            return Err(BrowserProviderError::Transport(
                "Gemini StreamGenerate completed without assistant text after the request was accepted"
                    .into(),
            ));
        }

        Self::persist_conversation_state(
            request.thread_id.as_deref(),
            &request.provider.id,
            &request.account.id,
            &request.route.model,
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
        let adapter = self.clone();
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

            // Emit a role-only chunk immediately so gateway first-byte timers do not
            // punish the stability window used to absorb cumulative Gemini rewrites.
            let role_event = json!({
                "id": completion_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant"},
                    "finish_reason": Value::Null
                }]
            });
            yield Ok(Bytes::from(format!("data: {role_event}\n\n")));

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
                        if error_code == MODEL_HEADER_INVALID {
                            adapter.invalidate_model_catalog(&account_id).await;
                        }
                        yield Err(std::io::Error::other(
                            generation_error(error_code, &account_id, &model).to_string(),
                        ));
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
                                if error_code == MODEL_HEADER_INVALID {
                                    adapter.invalidate_model_catalog(&account_id).await;
                                }
                                yield Err(std::io::Error::other(
                                    generation_error(error_code, &account_id, &model).to_string(),
                                ));
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

            if !latest.completed {
                yield Err(std::io::Error::other(
                    "Gemini browserless stream ended before the provider completion marker"
                ));
                return;
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
                &model,
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

    fn supports_model_discovery(&self) -> bool {
        true
    }

    async fn discover_models(
        &self,
        account_id: &str,
        binding: &BrowserAccountBinding,
        force: bool,
    ) -> Result<Vec<BrowserDiscoveredModel>, BrowserProviderError> {
        let snapshot = self.model_catalog(binding, account_id, force).await?;
        Ok(snapshot
            .models
            .iter()
            .map(|model| BrowserDiscoveredModel {
                external_id: model.external_id.clone(),
                display_name: model.display_name.clone(),
                owned_by: "Google".into(),
                context_window: None,
                capabilities: model.capabilities.clone(),
            })
            .collect())
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
        let selection = self.resolve_model_selection(&request).await?;
        let metadata = self
            .conversation_metadata(&request, &request.route.model)
            .await?;
        let has_native_state = metadata != default_metadata();
        let prompt = serialize_prompt(&request.body, has_native_state)?;
        let response = self
            .submit_generation(&request, &session, &prompt, metadata, selection.as_ref())
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

fn validate_thread_model_affinity(
    state: Option<&Value>,
    requested_model: &str,
) -> Result<(), String> {
    let Some(existing_model) = state
        .and_then(|value| value.get("model_external_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    else {
        // Legacy direct state predating model affinity is allowed to continue. Once the
        // next successful turn is persisted, the thread becomes explicitly model-bound.
        return Ok(());
    };
    if existing_model == requested_model {
        return Ok(());
    }
    Err(format!(
        "Gemini native conversation is already bound to model '{existing_model}'; start a new llmgateway thread to use model '{requested_model}'"
    ))
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
    model_number: i64,
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
    inner[79] = json!(model_number);
    inner[80] = json!(1);
    inner
}

fn find_model_recipe(
    snapshot: &GeminiModelCatalogSnapshot,
    requested_model: &str,
) -> Option<GeminiModelRecipe> {
    let requested = normalize_model_lookup(requested_model);
    snapshot
        .models
        .iter()
        .find(|model| {
            model.external_id == requested_model
                || model.aliases.contains(&requested)
                || model.model_id.eq_ignore_ascii_case(requested_model)
        })
        .cloned()
}

fn generation_error(
    error_code: i64,
    account_id: &str,
    model: &str,
) -> BrowserProviderError {
    match error_code {
        USAGE_LIMIT_EXCEEDED => {
            BrowserProviderError::Transport("Gemini web usage limit exceeded".into())
        }
        MODEL_HEADER_INVALID => BrowserProviderError::ModelRecipeStale {
            account_id: account_id.to_string(),
            model: model.to_string(),
        },
        _ => BrowserProviderError::Transport(format!(
            "Gemini StreamGenerate returned error code {error_code}"
        )),
    }
}

fn model_header_value(
    recipe: &GeminiModelRecipe,
    wire_session_id: &str,
) -> Result<String, BrowserProviderError> {
    let mut header = vec![
        json!(1),
        Value::Null,
        Value::Null,
        Value::Null,
        json!(recipe.model_id),
        Value::Null,
        Value::Null,
        json!(0),
        json!([4, 5, 6, 8]),
        Value::Null,
        Value::Null,
    ];
    if recipe.capacity_field == 13 {
        header.push(Value::Null);
        header.push(json!(recipe.capacity));
    } else {
        header.push(json!(recipe.capacity));
    }
    header.push(Value::Null);
    header.push(Value::Null);
    header.push(json!(recipe.model_number));
    header.push(json!(1));
    header.push(json!(wire_session_id));
    serde_json::to_string(&header)
        .map_err(|error| BrowserProviderError::Transport(error.to_string()))
}

fn parse_model_catalog_response(raw: &[u8]) -> Result<Vec<GeminiModelRecipe>, BrowserProviderError> {
    let values = decode_response_values(raw)?;
    let mut bodies = Vec::new();
    for value in &values {
        collect_rpc_bodies(value, GEMINI_GET_USER_STATUS_RPC, &mut bodies);
    }
    let mut models = Vec::new();
    let mut used_ids = BTreeSet::new();
    for body in bodies {
        let Some(model_items) = nested(&body, &[15]).and_then(Value::as_array) else {
            continue;
        };
        let tier_flags = nested(&body, &[16])
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let capability_flags = nested(&body, &[17])
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (capacity, capacity_field) =
            compute_model_capacity(&tier_flags, &capability_flags);
        for item in model_items {
            let Some(mut recipe) =
                parse_model_recipe(item, capacity, capacity_field)
            else {
                continue;
            };
            if !used_ids.insert(recipe.external_id.clone()) {
                recipe.external_id = format!(
                    "{}-{}",
                    recipe.external_id,
                    &recipe.model_id[..recipe.model_id.len().min(8)]
                );
            }
            recipe
                .aliases
                .insert(normalize_model_lookup(&recipe.external_id));
            models.push(recipe);
        }
        if !models.is_empty() {
            break;
        }
    }
    Ok(models)
}

fn decode_response_values(raw: &[u8]) -> Result<Vec<Value>, BrowserProviderError> {
    let mut decoder = GeminiFrameDecoder::default();
    let mut values = decoder
        .push(raw)
        .map_err(BrowserProviderError::Transport)?;
    values.extend(
        decoder
            .finish()
            .map_err(BrowserProviderError::Transport)?,
    );
    Ok(values)
}

fn collect_rpc_bodies(value: &Value, target: &str, bodies: &mut Vec<Value>) {
    let Some(items) = value.as_array() else {
        return;
    };
    if items.get(1).and_then(Value::as_str) == Some(target) {
        if let Some(body) = items
            .get(2)
            .and_then(Value::as_str)
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        {
            bodies.push(body);
        }
    }
    for item in items {
        if item.is_array() {
            collect_rpc_bodies(item, target, bodies);
        }
    }
}

fn compute_model_capacity(tier_flags: &[Value], capability_flags: &[Value]) -> (i64, usize) {
    if contains_int(tier_flags, 21) {
        return (1, 13);
    }
    if contains_int(tier_flags, 22) {
        return (2, 13);
    }
    if contains_int(capability_flags, 115) {
        return (4, 12);
    }
    if contains_int(tier_flags, 16) || contains_int(capability_flags, 106) {
        return (3, 12);
    }
    if contains_int(tier_flags, 8) || contains_int(capability_flags, 19) {
        return (2, 12);
    }
    (1, 12)
}

fn contains_int(values: &[Value], expected: i64) -> bool {
    values.iter().any(|value| value.as_i64() == Some(expected))
}

fn parse_model_recipe(
    model_data: &Value,
    capacity: i64,
    capacity_field: usize,
) -> Option<GeminiModelRecipe> {
    let model_id = nested(model_data, &[0])?.as_str()?.trim().to_string();
    if model_id.is_empty() {
        return None;
    }
    let category_name = nested(model_data, &[1])
        .or_else(|| nested(model_data, &[10]))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let display_name = nested(model_data, &[11])
        .or_else(|| nested(model_data, &[19]))
        .or_else(|| nested(model_data, &[1]))
        .and_then(Value::as_str)
        .unwrap_or(&model_id)
        .trim()
        .to_string();
    let description = nested(model_data, &[12])
        .or_else(|| nested(model_data, &[2]))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let model_number = nested(model_data, &[17])
        .and_then(Value::as_i64)
        .or_else(|| nested(model_data, &[9]).and_then(Value::as_i64))
        .unwrap_or(1);

    let base_name = if !category_name.is_empty() {
        category_name.as_str()
    } else if !display_name.is_empty() {
        display_name.as_str()
    } else {
        model_id.as_str()
    };
    let external_id = format!("gemini-web-{}", slugify(base_name));
    let mut aliases = BTreeSet::new();
    for alias in [
        model_id.as_str(),
        category_name.as_str(),
        display_name.as_str(),
        external_id.as_str(),
    ] {
        if !alias.trim().is_empty() {
            aliases.insert(normalize_model_lookup(alias));
        }
    }
    if !category_name.is_empty() {
        aliases.insert(normalize_model_lookup(&format!("gemini-{category_name}")));
    }
    let capabilities = infer_model_capabilities(&category_name, &display_name, &description);

    Some(GeminiModelRecipe {
        external_id,
        display_name: if display_name.is_empty() {
            category_name
        } else {
            display_name
        },
        description,
        model_id,
        capacity,
        capacity_field,
        model_number,
        aliases,
        capabilities,
    })
}

fn infer_model_capabilities(category: &str, display: &str, description: &str) -> Vec<String> {
    let haystack = format!("{category} {display} {description}").to_ascii_lowercase();
    let mut capabilities = BTreeSet::from(["chat".to_string(), "long-context".to_string()]);
    if haystack.contains("pro") || haystack.contains("think") || haystack.contains("reason") {
        capabilities.insert("reasoning".into());
        capabilities.insert("coding".into());
        capabilities.insert("premium".into());
    }
    if haystack.contains("flash") {
        capabilities.insert("fast".into());
        capabilities.insert("simple-chat".into());
    }
    if haystack.contains("lite") {
        capabilities.insert("cheap".into());
    }
    capabilities.into_iter().collect()
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character.to_ascii_lowercase());
            separator = false;
        } else {
            separator = true;
        }
    }
    if slug.is_empty() {
        "model".into()
    } else {
        slug
    }
}

fn normalize_model_lookup(value: &str) -> String {
    slugify(value.trim().trim_start_matches("gemini-web/"))
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
    fn parses_dynamic_model_catalog_and_builds_private_header() {
        let model_data = json!([
            "model-hex-pro",
            "Pro",
            "Reasoning model",
            null,
            null,
            null,
            null,
            null,
            null,
            null,
            null,
            "Gemini 3.1 Pro",
            "Reasoning and coding",
            null,
            null,
            null,
            null,
            3
        ]);
        let mut body = vec![Value::Null; 18];
        body[15] = json!([model_data]);
        body[16] = json!([8]);
        body[17] = json!([19]);
        let part = json!([["wrb.fr"], GEMINI_GET_USER_STATUS_RPC, Value::Array(body).to_string()]);
        let json_frame = serde_json::to_string(&vec![part]).unwrap();
        let payload = format!("\n{}\n{}\n", json_frame.encode_utf16().count() + 1, json_frame);
        let models = parse_model_catalog_response(payload.as_bytes()).unwrap();
        assert_eq!(models.len(), 1);
        let model = &models[0];
        assert_eq!(model.external_id, "gemini-web-pro");
        assert_eq!(model.display_name, "Gemini 3.1 Pro");
        assert_eq!(model.capacity, 2);
        assert_eq!(model.model_number, 3);
        assert!(model.capabilities.contains(&"reasoning".to_string()));
        let header = model_header_value(model, "SESSION").unwrap();
        assert!(header.contains("model-hex-pro"));
        assert!(header.contains("SESSION"));
    }

    #[test]
    fn selected_models_build_distinct_private_headers() {
        let pro = GeminiModelRecipe {
            external_id: "gemini-web-pro".into(),
            display_name: "Pro".into(),
            description: String::new(),
            model_id: "opaque-pro".into(),
            capacity: 2,
            capacity_field: 12,
            model_number: 3,
            aliases: BTreeSet::new(),
            capabilities: vec!["reasoning".into()],
        };
        let flash = GeminiModelRecipe {
            external_id: "gemini-web-flash".into(),
            display_name: "Flash".into(),
            description: String::new(),
            model_id: "opaque-flash".into(),
            capacity: 1,
            capacity_field: 12,
            model_number: 1,
            aliases: BTreeSet::new(),
            capabilities: vec!["fast".into()],
        };
        assert_ne!(
            model_header_value(&pro, "SESSION").unwrap(),
            model_header_value(&flash, "SESSION").unwrap()
        );
    }

    #[test]
    fn unknown_model_is_rejected_by_recipe_resolver() {
        let snapshot = GeminiModelCatalogSnapshot {
            discovered_at: Instant::now(),
            wire_session_id: "SESSION".into(),
            models: vec![GeminiModelRecipe {
                external_id: "gemini-web-pro".into(),
                display_name: "Pro".into(),
                description: String::new(),
                model_id: "opaque-pro".into(),
                capacity: 2,
                capacity_field: 12,
                model_number: 3,
                aliases: BTreeSet::new(),
                capabilities: vec![],
            }],
        };
        assert!(find_model_recipe(&snapshot, "gemini-web-does-not-exist").is_none());
    }

    #[test]
    fn invalid_model_header_is_classified_as_stale_recipe() {
        let error = generation_error(MODEL_HEADER_INVALID, "account-a", "gemini-web-pro");
        assert!(matches!(
            error,
            BrowserProviderError::ModelRecipeStale {
                account_id,
                model
            } if account_id == "account-a" && model == "gemini-web-pro"
        ));
    }

    #[tokio::test]
    async fn invalidating_recipe_cache_is_account_scoped() {
        let adapter = GeminiWebHttpAdapter::new().unwrap();
        let snapshot = GeminiModelCatalogSnapshot {
            discovered_at: Instant::now(),
            wire_session_id: "SESSION".into(),
            models: vec![],
        };
        {
            let mut catalogs = adapter.model_catalogs.write().await;
            catalogs.insert("account-a".into(), snapshot.clone());
            catalogs.insert("account-b".into(), snapshot);
        }
        adapter.invalidate_model_catalog("account-a").await;
        let catalogs = adapter.model_catalogs.read().await;
        assert!(!catalogs.contains_key("account-a"));
        assert!(catalogs.contains_key("account-b"));
    }

    #[test]
    fn native_thread_model_affinity_rejects_model_switch_before_submit() {
        let state = json!({
            "transport": "gemini-http",
            "model_external_id": "gemini-web-pro",
            "metadata": default_metadata()
        });
        assert!(validate_thread_model_affinity(Some(&state), "gemini-web-pro").is_ok());
        let error =
            validate_thread_model_affinity(Some(&state), "gemini-web-flash").unwrap_err();
        assert!(error.contains("start a new llmgateway thread"));
        assert!(error.contains("gemini-web-pro"));
        assert!(error.contains("gemini-web-flash"));
    }

    #[test]
    fn legacy_native_thread_without_model_affinity_can_continue() {
        let state = json!({
            "transport": "gemini-http",
            "metadata": default_metadata()
        });
        assert!(validate_thread_model_affinity(Some(&state), "gemini-web-pro").is_ok());
        assert!(validate_thread_model_affinity(None, "gemini-web-pro").is_ok());
    }

    #[test]
    fn model_number_is_written_into_inner_request() {
        let inner = build_inner_request(
            "hello",
            "en",
            default_metadata(),
            "REQUEST",
            false,
            6,
        );
        assert_eq!(inner[79], json!(6));
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
