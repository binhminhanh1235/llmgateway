use crate::{
    browser_auth::{BrowserAuthMaterial, BrowserAuthVault},
    browser_auth_runtime, conversation_runtime,
    browser_provider::{
        BrowserAccountBinding, BrowserAdapterDiagnostics, BrowserAdapterRequest,
        BrowserDiscoveredModel, BrowserProviderAdapter, BrowserProviderError,
        BrowserlessCapabilities,
        BrowserTransportMode, BROWSER_ADAPTER_CONTRACT_VERSION,
    },
};
use async_trait::async_trait;
use axum::http::Response as HttpResponse;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{
    header::{
        ACCEPT, ACCEPT_LANGUAGE, AUTHORIZATION, CONTENT_TYPE, COOKIE, ORIGIN, REFERER, USER_AGENT,
    },
    Client, Response, Url,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use uuid::Uuid;

const CHATGPT_HOST: &str = "chatgpt.com";
const CHATGPT_BASE: &str = "https://chatgpt.com";
const CHATGPT_SESSION_URL: &str = "https://chatgpt.com/api/auth/session";
const CHATGPT_SENTINEL_PREPARE_URL: &str =
    "https://chatgpt.com/backend-api/sentinel/chat-requirements/prepare";
const CHATGPT_SENTINEL_FINALIZE_URL: &str =
    "https://chatgpt.com/backend-api/sentinel/chat-requirements/finalize";
const CHATGPT_CONVERSATION_PREPARE_URL: &str =
    "https://chatgpt.com/backend-api/f/conversation/prepare";
const CHATGPT_CONVERSATION_URL: &str = "https://chatgpt.com/backend-api/f/conversation";
const CHATGPT_CONVERSATION_RESUME_URL: &str =
    "https://chatgpt.com/backend-api/f/conversation/resume";
const CHATGPT_MODELS_URL: &str =
    "https://chatgpt.com/backend-api/models?iim=false&is_gizmo=false&supports_model_picker_upgrade_presets=true";
const CHATGPT_ADAPTER_VERSION: &str = "experimental-2";
const CHATGPT_DIRECT_MODEL: &str = "chatgpt-web-default";
const CHATGPT_WEB_MODEL: &str = "auto";
const POW_MAX_ATTEMPTS: u32 = 500_000;
const PREFLIGHT_CACHE_TTL: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct ChatGptWebHttpAdapter {
    client: Client,
    preflight_cache: Arc<Mutex<HashMap<String, CachedPreflight>>>,
}

#[derive(Clone, Debug)]
struct ChatGptSession {
    access_token: String,
    cookie_header: String,
    user_agent: String,
    device_id: String,
    session_id: String,
    language: String,
    client_version: String,
}

#[derive(Clone, Debug)]
struct SentinelRequirements {
    token: String,
    proof_token: Option<String>,
}

#[derive(Clone, Debug)]
struct CachedPreflight {
    created_at: Instant,
    session: ChatGptSession,
    requirements: SentinelRequirements,
}

#[derive(Clone, Debug, Default)]
struct NativeConversation {
    conversation_id: Option<String>,
    parent_message_id: Option<String>,
    conduit_token: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct ChatGptStreamState {
    conversation_id: Option<String>,
    assistant_message_id: Option<String>,
    assistant_text: String,
    assistant_status: Option<String>,
    assistant_end_turn: Option<bool>,
    assistant_recipient: Option<String>,
    last_path: Option<String>,
    last_operation: Option<String>,
    resume_token: Option<String>,
    resume_topic: Option<String>,
    handoff: bool,
    stream_complete: bool,
    saw_done: bool,
}

#[derive(Debug, Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

#[derive(Clone, Debug)]
struct SseEvent {
    data: String,
}

impl ChatGptWebHttpAdapter {
    pub fn new() -> Result<Self, BrowserProviderError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        Ok(Self {
            client,
            preflight_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    async fn build_preflight(
        &self,
        material: &BrowserAuthMaterial,
        account_id: &str,
        timeout_duration: Duration,
    ) -> Result<CachedPreflight, BrowserProviderError> {
        let session = self
            .init_session(material, account_id, timeout_duration)
            .await?;
        let requirements = self
            .requirements(&session, account_id, timeout_duration)
            .await?;
        Ok(CachedPreflight {
            created_at: Instant::now(),
            session,
            requirements,
        })
    }

    async fn warm_preflight(
        &self,
        cache_key: &str,
        material: &BrowserAuthMaterial,
        account_id: &str,
        timeout_duration: Duration,
    ) -> Result<(), BrowserProviderError> {
        let preflight = self
            .build_preflight(material, account_id, timeout_duration)
            .await?;
        self.preflight_cache
            .lock()
            .await
            .insert(cache_key.to_string(), preflight);
        Ok(())
    }

    async fn take_or_build_preflight(
        &self,
        cache_key: &str,
        material: &BrowserAuthMaterial,
        account_id: &str,
        timeout_duration: Duration,
    ) -> Result<CachedPreflight, BrowserProviderError> {
        if let Some(cached) = self.preflight_cache.lock().await.remove(cache_key) {
            if cached.created_at.elapsed() <= PREFLIGHT_CACHE_TTL {
                return Ok(cached);
            }
        }
        self.build_preflight(material, account_id, timeout_duration)
            .await
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
                    "ChatGPT browserless auth material is unavailable; login with browser again: {error}"
                ),
            })?;
        if material.cookie_header_for_host(CHATGPT_HOST).is_empty() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "login_required".into(),
                message: "ChatGPT auth snapshot has no cookies valid for chatgpt.com".into(),
            });
        }
        Ok(material)
    }

    async fn init_session(
        &self,
        material: &BrowserAuthMaterial,
        account_id: &str,
        timeout_duration: Duration,
    ) -> Result<ChatGptSession, BrowserProviderError> {
        let cookie_header = material.cookie_header_for_host(CHATGPT_HOST);
        let user_agent = if material.user_agent.trim().is_empty() {
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36".to_string()
        } else {
            material.user_agent.clone()
        };
        let device_id = material
            .cookie_value_for_host(CHATGPT_HOST, "oai-did")
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let session_id = Uuid::new_v4().to_string();

        let response = self
            .client
            .get(CHATGPT_SESSION_URL)
            .header(COOKIE, &cookie_header)
            .header(USER_AGENT, &user_agent)
            .header(REFERER, format!("{CHATGPT_BASE}/"))
            .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .timeout(timeout_duration)
            .send()
            .await
            .map_err(|error| preflight_error(account_id, "session_bootstrap_failed", error))?;
        let status = response.status();
        if !status.is_success() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: if matches!(status.as_u16(), 401 | 403) {
                    "login_required".into()
                } else {
                    "session_bootstrap_failed".into()
                },
                message: format!("ChatGPT session bootstrap returned HTTP {status}"),
            });
        }

        let body: Value = response
            .json()
            .await
            .map_err(|error| preflight_error(account_id, "session_bootstrap_invalid", error))?;
        let access_token = body
            .get("accessToken")
            .or_else(|| body.get("access_token"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "login_required".into(),
                message: "ChatGPT session bootstrap returned no access token".into(),
            })?
            .to_string();

        Ok(ChatGptSession {
            access_token,
            cookie_header,
            user_agent,
            device_id,
            session_id,
            language: "en-US".into(),
            client_version: String::new(),
        })
    }

    async fn discover_models_impl(
        &self,
        account_id: &str,
        binding: &BrowserAccountBinding,
    ) -> Result<Vec<BrowserDiscoveredModel>, BrowserProviderError> {
        let material = self.auth_material(&binding.session, account_id)?;
        let timeout_duration =
            Duration::from_millis(binding.probe_timeout_ms.unwrap_or(8_000).max(3_000));
        let session = self
            .init_session(&material, account_id, timeout_duration)
            .await?;
        let response = self
            .base_request(
                &session,
                reqwest::Method::GET,
                CHATGPT_MODELS_URL,
                "/backend-api/models",
            )
            .header(ACCEPT, "application/json")
            .header(REFERER, format!("{CHATGPT_BASE}/"))
            .timeout(timeout_duration)
            .send()
            .await
            .map_err(|error| preflight_error(account_id, "model_catalog_failed", error))?;
        let status = response.status();
        if !status.is_success() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: if matches!(status.as_u16(), 401 | 403) {
                    "login_required".into()
                } else {
                    "model_catalog_failed".into()
                },
                message: format!("ChatGPT model catalog returned HTTP {status}"),
            });
        }
        let payload: Value = response
            .json()
            .await
            .map_err(|error| preflight_error(account_id, "model_catalog_invalid", error))?;
        let models = parse_chatgpt_model_catalog(&payload);
        if models.is_empty() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "model_catalog_empty".into(),
                message: "ChatGPT model catalog contained no selectable models".into(),
            });
        }
        Ok(models)
    }

    fn base_request(
        &self,
        session: &ChatGptSession,
        method: reqwest::Method,
        url: &str,
        target_path: &str,
    ) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .header(AUTHORIZATION, format!("Bearer {}", session.access_token))
            .header(COOKIE, &session.cookie_header)
            .header(USER_AGENT, &session.user_agent)
            .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .header("oai-language", &session.language)
            .header("oai-device-id", &session.device_id)
            .header("oai-session-id", &session.session_id)
            .header("x-openai-target-path", target_path)
            .header("x-openai-target-route", target_path)
            .header("sec-fetch-site", "same-origin")
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-dest", "empty")
            .header(ORIGIN, CHATGPT_BASE)
    }

    async fn requirements(
        &self,
        session: &ChatGptSession,
        account_id: &str,
        timeout_duration: Duration,
    ) -> Result<SentinelRequirements, BrowserProviderError> {
        let config = sentinel_config(session);
        let prepare_key = prepare_token(&config)?;
        let prepare = self
            .base_request(
                session,
                reqwest::Method::POST,
                CHATGPT_SENTINEL_PREPARE_URL,
                "/backend-api/sentinel/chat-requirements/prepare",
            )
            .header(ACCEPT, "*/*")
            .header(CONTENT_TYPE, "application/json")
            .header(REFERER, format!("{CHATGPT_BASE}/"))
            .timeout(timeout_duration)
            .json(&json!({"p": prepare_key}))
            .send()
            .await
            .map_err(|error| preflight_error(account_id, "sentinel_prepare_failed", error))?;
        let status = prepare.status();
        if !status.is_success() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: if matches!(status.as_u16(), 401 | 403) {
                    "login_required".into()
                } else {
                    "sentinel_prepare_failed".into()
                },
                message: format!("ChatGPT Sentinel prepare returned HTTP {status}"),
            });
        }
        let prepared: Value = prepare
            .json()
            .await
            .map_err(|error| preflight_error(account_id, "sentinel_prepare_invalid", error))?;

        for challenge in ["turnstile", "arkose", "so"] {
            if challenge_required(prepared.get(challenge)) {
                return Err(BrowserProviderError::AdapterIncompatible {
                    account_id: account_id.to_string(),
                    code: "browser_challenge_required".into(),
                    message: format!(
                        "ChatGPT Sentinel requires {challenge}; direct transport will not bypass the challenge"
                    ),
                });
            }
        }

        let prepare_token = prepared
            .get("prepare_token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "sentinel_prepare_invalid".into(),
                message: "ChatGPT Sentinel prepare returned no prepare_token".into(),
            })?
            .to_string();

        let proof_token = if prepared
            .get("proofofwork")
            .and_then(|value| value.get("required"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let seed = prepared
                .get("proofofwork")
                .and_then(|value| value.get("seed"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let difficulty = prepared
                .get("proofofwork")
                .and_then(|value| value.get("difficulty"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if seed.is_empty() || difficulty.is_empty() {
                return Err(BrowserProviderError::AdapterIncompatible {
                    account_id: account_id.to_string(),
                    code: "sentinel_pow_invalid".into(),
                    message: "ChatGPT Sentinel returned an incomplete proof-of-work challenge"
                        .into(),
                });
            }
            let config_for_pow = config.clone();
            Some(
                tokio::task::spawn_blocking(move || {
                    solve_proof(&seed, &difficulty, &config_for_pow, POW_MAX_ATTEMPTS)
                })
                .await
                .map_err(|error| BrowserProviderError::AdapterIncompatible {
                    account_id: account_id.to_string(),
                    code: "sentinel_pow_failed".into(),
                    message: format!("ChatGPT proof-of-work worker failed: {error}"),
                })?
                .map_err(|message| BrowserProviderError::AdapterIncompatible {
                    account_id: account_id.to_string(),
                    code: "sentinel_pow_failed".into(),
                    message,
                })?,
            )
        } else {
            None
        };

        let mut finalize_body = serde_json::Map::new();
        finalize_body.insert("prepare_token".into(), Value::String(prepare_token));
        if let Some(proof) = proof_token.as_ref() {
            finalize_body.insert("proofofwork".into(), Value::String(proof.clone()));
        }

        let finalized = self
            .base_request(
                session,
                reqwest::Method::POST,
                CHATGPT_SENTINEL_FINALIZE_URL,
                "/backend-api/sentinel/chat-requirements/finalize",
            )
            .header(ACCEPT, "*/*")
            .header(CONTENT_TYPE, "application/json")
            .header(REFERER, format!("{CHATGPT_BASE}/"))
            .timeout(timeout_duration)
            .json(&Value::Object(finalize_body))
            .send()
            .await
            .map_err(|error| preflight_error(account_id, "sentinel_finalize_failed", error))?;
        let status = finalized.status();
        if !status.is_success() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: if matches!(status.as_u16(), 401 | 403) {
                    "browser_challenge_required".into()
                } else {
                    "sentinel_finalize_failed".into()
                },
                message: format!("ChatGPT Sentinel finalize returned HTTP {status}"),
            });
        }
        let finalized: Value = finalized
            .json()
            .await
            .map_err(|error| preflight_error(account_id, "sentinel_finalize_invalid", error))?;
        let token = finalized
            .get("token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| BrowserProviderError::AdapterIncompatible {
                account_id: account_id.to_string(),
                code: "sentinel_finalize_invalid".into(),
                message: "ChatGPT Sentinel finalize returned no chat requirements token".into(),
            })?
            .to_string();

        Ok(SentinelRequirements { token, proof_token })
    }

    async fn native_conversation(
        &self,
        request: &BrowserAdapterRequest,
        session: &ChatGptSession,
    ) -> Result<NativeConversation, BrowserProviderError> {
        let Some(thread_id) = request.thread_id.as_deref() else {
            return Ok(NativeConversation::default());
        };
        let Some(store) = conversation_runtime::get() else {
            return Ok(NativeConversation::default());
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

        if let Some(state) = state.as_ref() {
            let conversation_id = state
                .get("conversation_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let parent_message_id = state
                .get("parent_message_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            if !needs_resync && conversation_id.is_some() && parent_message_id.is_some() {
                return Ok(NativeConversation {
                    conversation_id,
                    parent_message_id,
                    conduit_token: state
                        .get("conduit_token")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string),
                });
            }
        }

        let provider_conversation = store
            .provider_conversation(thread_id, &request.provider.id, &request.account.id)
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let Some(provider_conversation) = provider_conversation else {
            return Ok(NativeConversation::default());
        };
        let conversation_id = chatgpt_conversation_id(&provider_conversation.conversation_url)
            .ok_or_else(|| BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "direct_state_unsynced".into(),
                message: "Stored ChatGPT conversation URL cannot be resynchronized by the direct transport".into(),
            })?;

        let target_path = format!("/backend-api/conversation/{conversation_id}");
        let response = self
            .base_request(
                session,
                reqwest::Method::GET,
                &format!("{CHATGPT_BASE}{target_path}"),
                &target_path,
            )
            .header(ACCEPT, "application/json")
            .header(REFERER, format!("{CHATGPT_BASE}/c/{conversation_id}"))
            .timeout(Duration::from_millis(
                request.binding.probe_timeout_ms.unwrap_or(8_000).max(3_000),
            ))
            .send()
            .await
            .map_err(|error| {
                preflight_error(&request.account.id, "direct_state_unsynced", error)
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: if matches!(status.as_u16(), 401 | 403) {
                    "login_required".into()
                } else {
                    "direct_state_unsynced".into()
                },
                message: format!(
                    "ChatGPT conversation state resync returned HTTP {status}"
                ),
            });
        }
        let detail: Value = response
            .json()
            .await
            .map_err(|error| {
                preflight_error(&request.account.id, "direct_state_unsynced", error)
            })?;
        let parent_message_id = detail
            .get("current_node")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| BrowserProviderError::AdapterIncompatible {
                account_id: request.account.id.clone(),
                code: "direct_state_unsynced".into(),
                message: "ChatGPT conversation detail returned no current_node for direct continuation".into(),
            })?
            .to_string();

        Ok(NativeConversation {
            conversation_id: Some(conversation_id),
            parent_message_id: Some(parent_message_id),
            conduit_token: None,
        })
    }

    async fn prepare_conversation(
        &self,
        request: &BrowserAdapterRequest,
        session: &ChatGptSession,
        native: &NativeConversation,
        partial_query: &Value,
        model: &str,
        trace_id: &str,
        referer: &str,
    ) -> Result<String, BrowserProviderError> {
        let mut conduit = native
            .conduit_token
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "no-token".into());

        for prepare_state in ["none", "sent", "success"] {
            let mut payload = json!({
                "action": "next",
                "fork_from_shared_post": false,
                "parent_message_id": native.parent_message_id.as_deref().unwrap_or("client-created-root"),
                "model": model,
                "client_prepare_state": prepare_state,
                "timezone_offset_min": -420,
                "timezone": "Asia/Bangkok",
                "conversation_mode": {"kind": "primary_assistant"},
                "system_hints": [],
                "supports_buffering": true,
                "supported_encodings": ["v1"],
                "client_contextual_info": {"app_name": "chatgpt.com"},
                "history_and_training_disabled": request.thread_id.is_none()
            });
            if let Some(conversation_id) = native.conversation_id.as_ref() {
                payload["conversation_id"] = Value::String(conversation_id.clone());
            }
            if prepare_state != "none" {
                payload["partial_query"] = partial_query.clone();
            }

            let response = self
                .base_request(
                    session,
                    reqwest::Method::POST,
                    CHATGPT_CONVERSATION_PREPARE_URL,
                    "/backend-api/f/conversation/prepare",
                )
                .header(ACCEPT, "*/*")
                .header(CONTENT_TYPE, "application/json")
                .header(REFERER, referer)
                .header("x-conduit-token", &conduit)
                .header("x-oai-turn-trace-id", trace_id)
                .timeout(Duration::from_millis(
                    request.binding.probe_timeout_ms.unwrap_or(8_000).max(3_000),
                ))
                .json(&payload)
                .send()
                .await
                .map_err(|error| {
                    preflight_error(&request.account.id, "conversation_prepare_failed", error)
                })?;
            let status = response.status();
            if !status.is_success() {
                return Err(BrowserProviderError::AdapterIncompatible {
                    account_id: request.account.id.clone(),
                    code: "conversation_prepare_failed".into(),
                    message: format!(
                        "ChatGPT conversation prepare ({prepare_state}) returned HTTP {status}"
                    ),
                });
            }
            if let Some(value) = response
                .headers()
                .get("x-conduit-token")
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
            {
                conduit = value.to_string();
            }
            let body: Value = response
                .json()
                .await
                .map_err(|error| {
                    preflight_error(
                        &request.account.id,
                        "conversation_prepare_invalid",
                        error,
                    )
                })?;
            if let Some(value) = body
                .get("conduit_token")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                conduit = value.to_string();
            }
            if conduit == "no-token" {
                return Err(BrowserProviderError::AdapterIncompatible {
                    account_id: request.account.id.clone(),
                    code: "conversation_prepare_invalid".into(),
                    message: format!(
                        "ChatGPT conversation prepare ({prepare_state}) returned no conduit token"
                    ),
                });
            }
        }

        Ok(conduit)
    }

    async fn submit_conversation(
        &self,
        request: &BrowserAdapterRequest,
        session: &ChatGptSession,
        requirements: &SentinelRequirements,
        native: &NativeConversation,
        user_message: Value,
        conduit: &str,
        model: &str,
        trace_id: &str,
        referer: &str,
    ) -> Result<Response, BrowserProviderError> {
        let mut payload = json!({
            "action": "next",
            "messages": [user_message],
            "model": model,
            "parent_message_id": native.parent_message_id.as_deref().unwrap_or("client-created-root"),
            "client_prepare_state": "success",
            "timezone_offset_min": -420,
            "timezone": "Asia/Bangkok",
            "conversation_mode": {"kind": "primary_assistant"},
            "enable_message_followups": true,
            "system_hints": [],
            "supports_buffering": true,
            "supported_encodings": ["v1"],
            "client_contextual_info": {"app_name": "chatgpt.com"},
            "paragen_cot_summary_display_override": "allow",
            "force_parallel_switch": "auto",
            "history_and_training_disabled": request.thread_id.is_none(),
            "websocket_request_id": Uuid::new_v4().to_string()
        });
        if let Some(conversation_id) = native.conversation_id.as_ref() {
            payload["conversation_id"] = Value::String(conversation_id.clone());
        }

        let mut upstream = self
            .base_request(
                session,
                reqwest::Method::POST,
                CHATGPT_CONVERSATION_URL,
                "/backend-api/f/conversation",
            )
            .header(ACCEPT, "text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .header(REFERER, referer)
            .header("x-conduit-token", conduit)
            .header("x-oai-turn-trace-id", trace_id)
            .header(
                "openai-sentinel-chat-requirements-token",
                &requirements.token,
            )
            .timeout(Duration::from_millis(
                request.binding.response_timeout_ms.unwrap_or(180_000),
            ));
        if let Some(proof) = requirements.proof_token.as_ref() {
            upstream = upstream.header("openai-sentinel-proof-token", proof);
        }

        let response = upstream
            .json(&payload)
            .send()
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(BrowserProviderError::Transport(format!(
                "ChatGPT conversation returned HTTP {status} after the turn was submitted"
            )));
        }
        Ok(response)
    }

    async fn resume_response(
        &self,
        session: &ChatGptSession,
        state: &ChatGptStreamState,
        referer: &str,
        timeout_duration: Duration,
    ) -> Result<Response, BrowserProviderError> {
        let conversation_id = state.conversation_id.as_deref().ok_or_else(|| {
            BrowserProviderError::Transport(
                "ChatGPT stream_handoff did not include conversation_id".into(),
            )
        })?;
        let resume_token = state.resume_token.as_deref().ok_or_else(|| {
            BrowserProviderError::Transport(
                "ChatGPT stream_handoff did not include resume token".into(),
            )
        })?;
        if state.resume_topic.is_none() {
            return Err(BrowserProviderError::Transport(
                "ChatGPT stream_handoff did not include resume_sse_endpoint".into(),
            ));
        }

        for offset in 0..3 {
            let response = self
                .base_request(
                    session,
                    reqwest::Method::POST,
                    CHATGPT_CONVERSATION_RESUME_URL,
                    "/backend-api/f/conversation/resume",
                )
                .header(ACCEPT, "text/event-stream")
                .header(CONTENT_TYPE, "application/json")
                .header(REFERER, referer)
                .header("x-conduit-token", resume_token)
                .timeout(timeout_duration)
                .json(&json!({"conversation_id": conversation_id, "offset": offset}))
                .send()
                .await
                .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
            if response.status().as_u16() == 404 {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
            if !response.status().is_success() {
                return Err(BrowserProviderError::Transport(format!(
                    "ChatGPT conversation resume returned HTTP {}",
                    response.status()
                )));
            }
            return Ok(response);
        }

        Err(BrowserProviderError::Transport(
            "ChatGPT conversation resume was not ready after 3 offsets".into(),
        ))
    }

    async fn persist_state(
        request: &BrowserAdapterRequest,
        state: &ChatGptStreamState,
        conduit_token: &str,
    ) -> Result<(), BrowserProviderError> {
        let Some(thread_id) = request.thread_id.as_deref() else {
            return Ok(());
        };
        let Some(store) = conversation_runtime::get() else {
            return Ok(());
        };
        let conversation_id = state.conversation_id.as_deref().ok_or_else(|| {
            BrowserProviderError::Transport(
                "ChatGPT completed without a native conversation id".into(),
            )
        })?;
        let parent_message_id = state.assistant_message_id.as_deref().ok_or_else(|| {
            BrowserProviderError::Transport(
                "ChatGPT completed without an assistant message id".into(),
            )
        })?;
        let stored = json!({
            "transport": "chatgpt-http",
            "conversation_id": conversation_id,
            "parent_message_id": parent_message_id,
            "conduit_token": conduit_token
        });
        store
            .upsert_provider_conversation_state(
                thread_id,
                &request.provider.id,
                &request.account.id,
                &stored,
            )
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        store
            .upsert_provider_conversation(
                thread_id,
                &request.provider.id,
                &request.account.id,
                &format!("https://chatgpt.com/c/{conversation_id}"),
            )
            .await
            .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        Ok(())
    }

    async fn collect_response(
        &self,
        request: &BrowserAdapterRequest,
        session: &ChatGptSession,
        response: Response,
        conduit: &str,
        referer: &str,
    ) -> Result<ChatGptStreamState, BrowserProviderError> {
        let mut state = ChatGptStreamState::default();
        consume_response_bytes(response, &mut state).await?;
        if state.handoff && !stream_success(&state) {
            let resumed = self
                .resume_response(
                    session,
                    &state,
                    referer,
                    Duration::from_millis(
                        request.binding.response_timeout_ms.unwrap_or(180_000),
                    ),
                )
                .await?;
            state.saw_done = false;
            state.stream_complete = false;
            consume_response_bytes(resumed, &mut state).await?;
        }
        validate_stream_success(&state)?;
        Self::persist_state(request, &state, conduit).await?;
        Ok(state)
    }

    fn streaming_response(
        &self,
        request: &BrowserAdapterRequest,
        session: ChatGptSession,
        response: Response,
        conduit: String,
        referer: String,
    ) -> Result<reqwest::Response, BrowserProviderError> {
        let adapter = self.clone();
        let native_request = request.clone();
        let model = request.route.model.clone();
        let completion_id = format!("chatcmpl_chatgpt_{}", Uuid::new_v4().simple());
        let created = chrono::Utc::now().timestamp();

        let stream = async_stream::stream! {
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

            let mut state = ChatGptStreamState::default();
            let mut decoder = SseDecoder::default();
            let mut upstream = response.bytes_stream();

            while let Some(chunk) = upstream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        yield Err(std::io::Error::other(format!(
                            "ChatGPT browserless stream body failed: {error}"
                        )));
                        return;
                    }
                };
                let events = match decoder.push(&chunk) {
                    Ok(events) => events,
                    Err(error) => {
                        yield Err(std::io::Error::other(error));
                        return;
                    }
                };
                for event in events {
                    let deltas = match state.ingest_event(&event.data) {
                        Ok(deltas) => deltas,
                        Err(error) => {
                            yield Err(std::io::Error::other(error));
                            return;
                        }
                    };
                    for delta in deltas {
                        let event = openai_delta_event(
                            &completion_id,
                            created,
                            &model,
                            &delta,
                        );
                        yield Ok(Bytes::from(format!("data: {event}\n\n")));
                    }
                }
            }

            let tail_events = match decoder.finish() {
                Ok(events) => events,
                Err(error) => {
                    yield Err(std::io::Error::other(error));
                    return;
                }
            };
            for event in tail_events {
                let deltas = match state.ingest_event(&event.data) {
                    Ok(deltas) => deltas,
                    Err(error) => {
                        yield Err(std::io::Error::other(error));
                        return;
                    }
                };
                for delta in deltas {
                    let event = openai_delta_event(
                        &completion_id,
                        created,
                        &model,
                        &delta,
                    );
                    yield Ok(Bytes::from(format!("data: {event}\n\n")));
                }
            }

            if state.handoff && !stream_success(&state) {
                // The primary SSE can legitimately finish with [DONE] only to hand
                // the turn to /resume. Final success must therefore observe a fresh
                // [DONE] on the resumed stream rather than borrowing the primary one.
                state.saw_done = false;
                state.stream_complete = false;

                let resumed = adapter
                    .resume_response(
                        &session,
                        &state,
                        &referer,
                        Duration::from_millis(
                            native_request.binding.response_timeout_ms.unwrap_or(180_000),
                        ),
                    )
                    .await;
                let resumed = match resumed {
                    Ok(response) => response,
                    Err(error) => {
                        yield Err(std::io::Error::other(error.to_string()));
                        return;
                    }
                };

                let mut resume_decoder = SseDecoder::default();
                let mut resume_upstream = resumed.bytes_stream();
                while let Some(chunk) = resume_upstream.next().await {
                    let chunk = match chunk {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            yield Err(std::io::Error::other(format!(
                                "ChatGPT browserless resume body failed: {error}"
                            )));
                            return;
                        }
                    };
                    let events = match resume_decoder.push(&chunk) {
                        Ok(events) => events,
                        Err(error) => {
                            yield Err(std::io::Error::other(error));
                            return;
                        }
                    };
                    for event in events {
                        let deltas = match state.ingest_event(&event.data) {
                            Ok(deltas) => deltas,
                            Err(error) => {
                                yield Err(std::io::Error::other(error));
                                return;
                            }
                        };
                        for delta in deltas {
                            let event = openai_delta_event(
                                &completion_id,
                                created,
                                &model,
                                &delta,
                            );
                            yield Ok(Bytes::from(format!("data: {event}\n\n")));
                        }
                    }
                }

                let tail_events = match resume_decoder.finish() {
                    Ok(events) => events,
                    Err(error) => {
                        yield Err(std::io::Error::other(error));
                        return;
                    }
                };
                for event in tail_events {
                    let deltas = match state.ingest_event(&event.data) {
                        Ok(deltas) => deltas,
                        Err(error) => {
                            yield Err(std::io::Error::other(error));
                            return;
                        }
                    };
                    for delta in deltas {
                        let event = openai_delta_event(
                            &completion_id,
                            created,
                            &model,
                            &delta,
                        );
                        yield Ok(Bytes::from(format!("data: {event}\n\n")));
                    }
                }
            }

            if let Err(error) = validate_stream_success(&state) {
                yield Err(std::io::Error::other(error.to_string()));
                return;
            }
            if let Err(error) = ChatGptWebHttpAdapter::persist_state(
                &native_request,
                &state,
                &conduit,
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
impl BrowserProviderAdapter for ChatGptWebHttpAdapter {
    fn kind(&self) -> &'static str {
        "browser-chatgpt-http"
    }

    fn adapter_id(&self) -> &'static str {
        "chatgpt-web-http"
    }

    fn browserless_capabilities(&self) -> BrowserlessCapabilities {
        BrowserlessCapabilities::preferred(BrowserTransportMode::HttpPreferred, true, true, true)
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
        let result = async {
            let material = self.auth_material(&binding.session, account_id)?;
            self.warm_preflight(
                &binding.session,
                &material,
                account_id,
                Duration::from_millis(binding.probe_timeout_ms.unwrap_or(8_000).max(3_000)),
            )
            .await
        }
        .await;

        match result {
            Ok(()) => diagnostics(
                account_id,
                binding,
                "ready",
                "ChatGPT direct HTTP preflight is ready; authenticated web models can run without Chromium",
            ),
            Err(BrowserProviderError::AdapterIncompatible { code, message, .. }) => {
                let status = match code.as_str() {
                    "login_required" => "login_required",
                    "browser_challenge_required" => "requires_attention",
                    _ => "adapter_incompatible",
                };
                diagnostics(account_id, binding, status, &message)
            }
            Err(error) => diagnostics(account_id, binding, "unavailable", &error.to_string()),
        }
    }

    async fn execute_chat(
        &self,
        request: BrowserAdapterRequest,
    ) -> Result<reqwest::Response, BrowserProviderError> {
        let web_model = chatgpt_wire_model(&request.route.model);
        let material = self.auth_material(&request.session_id, &request.account.id)?;
        let preflight = self
            .take_or_build_preflight(
                &request.session_id,
                &material,
                &request.account.id,
                Duration::from_millis(request.binding.probe_timeout_ms.unwrap_or(8_000).max(3_000)),
            )
            .await?;
        let session = preflight.session;
        let requirements = preflight.requirements;
        let native = self.native_conversation(&request, &session).await?;
        let prompt = serialize_prompt(&request.body, native.conversation_id.is_some())?;
        let user_message = chatgpt_user_message(&prompt);
        let partial_query = partial_query(&user_message);
        let trace_id = Uuid::new_v4().to_string();
        let referer = native
            .conversation_id
            .as_ref()
            .map(|id| format!("{CHATGPT_BASE}/c/{id}"))
            .unwrap_or_else(|| {
                if request.thread_id.is_none() {
                    format!("{CHATGPT_BASE}/?temporary-chat=true")
                } else {
                    format!("{CHATGPT_BASE}/c/WEB:{}", Uuid::new_v4())
                }
            });
        let conduit = self
            .prepare_conversation(
                &request,
                &session,
                &native,
                &partial_query,
                &web_model,
                &trace_id,
                &referer,
            )
            .await?;
        let response = self
            .submit_conversation(
                &request,
                &session,
                &requirements,
                &native,
                user_message,
                &conduit,
                &web_model,
                &trace_id,
                &referer,
            )
            .await?;

        if request
            .body
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            self.streaming_response(&request, session, response, conduit, referer)
        } else {
            let state = self
                .collect_response(&request, &session, response, &conduit, &referer)
                .await?;
            synthetic_json_response(&request.route.model, &state.assistant_text)
        }
    }
}

fn diagnostics(
    account_id: &str,
    binding: &BrowserAccountBinding,
    status: &str,
    message: &str,
) -> BrowserAdapterDiagnostics {
    BrowserAdapterDiagnostics {
        account_id: account_id.to_string(),
        provider_kind: "browser-chatgpt".into(),
        adapter_id: Some("chatgpt-web-http".into()),
        adapter_version: Some(CHATGPT_ADAPTER_VERSION.into()),
        contract_version: Some(BROWSER_ADAPTER_CONTRACT_VERSION),
        expected_contract_version: BROWSER_ADAPTER_CONTRACT_VERSION,
        status: status.into(),
        message: message.into(),
        page_signature: None,
        target_url_prefix: Some(format!("{CHATGPT_BASE}/")),
        configured_models: binding.models.clone(),
    }
}

fn chatgpt_conversation_id(raw: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    if url.host_str() != Some(CHATGPT_HOST) {
        return None;
    }
    let segments = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let index = segments.iter().position(|segment| *segment == "c")?;
    segments
        .get(index + 1)
        .filter(|value| !value.is_empty())
        .map(|value| (*value).to_string())
}

fn preflight_error(
    account_id: &str,
    code: &str,
    error: impl std::fmt::Display,
) -> BrowserProviderError {
    BrowserProviderError::AdapterIncompatible {
        account_id: account_id.to_string(),
        code: code.into(),
        message: error.to_string(),
    }
}

fn challenge_required(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    value
        .get("required")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value
            .get("dx")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        || value
            .get("collector_dx")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        || value
            .get("snapshot_dx")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
}

fn sentinel_config(session: &ChatGptSession) -> Vec<Value> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    vec![
        json!(2134),
        json!(chrono::Utc::now().format("%a %b %d %Y %H:%M:%S GMT+0000 (UTC)").to_string()),
        json!(4_294_967_296u64),
        json!(1),
        json!(session.user_agent),
        Value::Null,
        json!(session.client_version),
        json!("en"),
        json!("en-US,en"),
        json!(0),
        json!("vendor−Google Inc."),
        json!(format!("_reactListening{}", &Uuid::new_v4().simple().to_string()[..10])),
        json!("onmessage"),
        json!(0),
        json!(session.session_id),
        json!(""),
        json!(8),
        json!(now_ms),
        json!(0),
        json!(0),
        json!(0),
        json!(0),
        json!(0),
        json!(0),
        json!(0),
    ]
}

fn prepare_token(config: &[Value]) -> Result<String, BrowserProviderError> {
    if config.len() != 25 {
        return Err(BrowserProviderError::InvalidConfig(format!(
            "ChatGPT Sentinel configuration must have 25 slots, got {}",
            config.len()
        )));
    }
    let mut prepared = config.to_vec();
    prepared[3] = json!(1);
    prepared[9] = json!(0);
    let encoded = serde_json::to_vec(&prepared)
        .map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
    Ok(format!("gAAAAAC{}", STANDARD.encode(encoded)))
}

fn solve_proof(
    seed: &str,
    difficulty: &str,
    config: &[Value],
    max_attempts: u32,
) -> Result<String, String> {
    if config.len() != 25 {
        return Err(format!(
            "ChatGPT PoW configuration must have 25 slots, got {}",
            config.len()
        ));
    }
    if difficulty.is_empty()
        || difficulty.len() > 8
        || !difficulty.chars().all(|character| character.is_ascii_hexdigit())
    {
        return Err(format!(
            "ChatGPT PoW difficulty is invalid: {difficulty:?}"
        ));
    }

    let started = std::time::Instant::now();
    for attempt in 0..max_attempts {
        let mut candidate = config.to_vec();
        candidate[3] = json!(attempt);
        candidate[9] = json!(started.elapsed().as_millis() as u64);
        let raw = serde_json::to_vec(&candidate).map_err(|error| error.to_string())?;
        let encoded = STANDARD.encode(raw);
        let digest = fnv_avalanche_hex(&format!("{seed}{encoded}"));
        if digest[..difficulty.len()].to_ascii_lowercase() <= difficulty.to_ascii_lowercase() {
            return Ok(format!("gAAAAAB{encoded}~S"));
        }
    }
    Err(format!(
        "ChatGPT Sentinel proof-of-work did not solve within {max_attempts} attempts"
    ))
}

fn fnv_avalanche_hex(input: &str) -> String {
    let mut current: u32 = 2_166_136_261;
    for byte in input.as_bytes() {
        current ^= u32::from(*byte);
        current = current.wrapping_mul(16_777_619);
    }
    current ^= current >> 16;
    current = current.wrapping_mul(2_246_822_507);
    current ^= current >> 13;
    current = current.wrapping_mul(3_266_489_909);
    current ^= current >> 16;
    format!("{current:08x}")
}

fn chatgpt_wire_model(requested: &str) -> String {
    let trimmed = requested.trim().trim_start_matches("chatgpt-web/");
    if trimmed.is_empty()
        || trimmed == CHATGPT_DIRECT_MODEL
        || trimmed.eq_ignore_ascii_case(CHATGPT_WEB_MODEL)
    {
        CHATGPT_WEB_MODEL.to_string()
    } else {
        trimmed.to_string()
    }
}

fn parse_chatgpt_model_catalog(payload: &Value) -> Vec<BrowserDiscoveredModel> {
    let items = payload
        .get("models")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array());
    let Some(items) = items else {
        return Vec::new();
    };

    let mut seen = std::collections::BTreeSet::new();
    let mut models = Vec::new();
    for item in items {
        if item.get("enabled").and_then(Value::as_bool) == Some(false)
            || item.get("hidden").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let slug = item
            .get("slug")
            .or_else(|| item.get("model_slug"))
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(slug) = slug else {
            continue;
        };
        let external_id = if slug.eq_ignore_ascii_case(CHATGPT_WEB_MODEL) {
            CHATGPT_DIRECT_MODEL.to_string()
        } else {
            slug.to_string()
        };
        if !seen.insert(external_id.clone()) {
            continue;
        }

        let display_name = item
            .get("title")
            .or_else(|| item.get("display_name"))
            .or_else(|| item.get("displayName"))
            .or_else(|| item.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(slug)
            .to_string();
        let description = item
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let context_window = item
            .get("max_tokens")
            .or_else(|| item.get("context_window"))
            .or_else(|| item.get("contextWindow"))
            .or_else(|| item.get("max_context_tokens"))
            .and_then(Value::as_i64);

        let haystack = format!("{slug} {display_name} {description}").to_ascii_lowercase();
        let mut capabilities = vec!["chat".to_string(), "streaming".to_string()];
        if haystack.contains("think")
            || haystack.contains("reason")
            || haystack.contains("pro")
            || haystack.contains("gpt-5")
        {
            capabilities.push("reasoning".into());
        }
        if haystack.contains("code") || haystack.contains("codex") || haystack.contains("gpt-5") {
            capabilities.push("coding".into());
        }

        models.push(BrowserDiscoveredModel {
            external_id,
            display_name,
            owned_by: "OpenAI".into(),
            context_window,
            capabilities,
        });
    }

    if !seen.contains(CHATGPT_DIRECT_MODEL) {
        models.insert(
            0,
            BrowserDiscoveredModel {
                external_id: CHATGPT_DIRECT_MODEL.into(),
                display_name: "ChatGPT Auto".into(),
                owned_by: "OpenAI".into(),
                context_window: None,
                capabilities: vec!["chat".into(), "streaming".into()],
            },
        );
    }
    models
}

fn serialize_prompt(body: &Value, native_continuation: bool) -> Result<String, BrowserProviderError> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| BrowserProviderError::InvalidConfig(
            "ChatGPT browserless chat requires an OpenAI-style messages array".into(),
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
            "ChatGPT browserless chat requires at least one text message".into(),
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

fn chatgpt_user_message(prompt: &str) -> Value {
    json!({
        "id": Uuid::new_v4().to_string(),
        "author": {"role": "user"},
        "create_time": chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
        "content": {"content_type": "text", "parts": [prompt]},
        "metadata": {
            "selected_github_repos": [],
            "selected_all_github_repos": false,
            "serialization_metadata": {"custom_symbol_offsets": []}
        }
    })
}

fn partial_query(message: &Value) -> Value {
    json!({
        "id": message.get("id").cloned().unwrap_or(Value::Null),
        "author": message.get("author").cloned().unwrap_or(json!({"role":"user"})),
        "content": message.get("content").cloned().unwrap_or(json!({"content_type":"text","parts":[""]}))
    })
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, String> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        loop {
            let Some((index, separator_len)) = find_sse_separator(&self.buffer) else {
                break;
            };
            let block = self.buffer.drain(..index + separator_len).collect::<Vec<_>>();
            let payload = &block[..index];
            if payload.is_empty() {
                continue;
            }
            let text = std::str::from_utf8(payload)
                .map_err(|error| format!("ChatGPT SSE returned invalid UTF-8: {error}"))?;
            let mut data_lines = Vec::new();
            for raw_line in text.lines() {
                let line = raw_line.trim_end_matches('\r');
                if let Some(data) = line.strip_prefix("data:") {
                    data_lines.push(data.trim_start().to_string());
                }
            }
            if !data_lines.is_empty() {
                events.push(SseEvent {
                    data: data_lines.join("\n"),
                });
            }
        }
        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<SseEvent>, String> {
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

fn find_sse_separator(bytes: &[u8]) -> Option<(usize, usize)> {
    if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
        return Some((index, 4));
    }
    bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2))
}

impl ChatGptStreamState {
    fn ingest_event(&mut self, data: &str) -> Result<Vec<String>, String> {
        if data == "[DONE]" {
            self.saw_done = true;
            return Ok(Vec::new());
        }
        let payload: Value = match serde_json::from_str(data) {
            Ok(Value::String(inner))
                if inner.trim_start().starts_with('{') || inner.trim_start().starts_with('[') =>
            {
                serde_json::from_str(&inner).unwrap_or(Value::String(inner))
            }
            Ok(value) => value,
            Err(_) => return Ok(Vec::new()),
        };
        self.ingest_value(&payload)
    }

    fn ingest_value(&mut self, payload: &Value) -> Result<Vec<String>, String> {
        let mut deltas = Vec::new();
        let Some(object) = payload.as_object() else {
            return Ok(deltas);
        };

        if let Some(conversation_id) = object
            .get("conversation_id")
            .or_else(|| object.get("conversationId"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            self.conversation_id = Some(conversation_id.to_string());
        }

        match object.get("type").and_then(Value::as_str) {
            Some("resume_conversation_token") => {
                if let Some(token) = object.get("token").and_then(Value::as_str) {
                    self.resume_token = Some(token.to_string());
                }
            }
            Some("stream_handoff") => {
                self.handoff = true;
                if let Some(options) = object.get("options").and_then(Value::as_array) {
                    for option in options {
                        if option.get("type").and_then(Value::as_str)
                            == Some("resume_sse_endpoint")
                        {
                            if let Some(topic) = option.get("topic_id").and_then(Value::as_str) {
                                self.resume_topic = Some(topic.to_string());
                            }
                        }
                    }
                }
            }
            Some("message_stream_complete") => self.stream_complete = true,
            _ => {}
        }

        if let Some(message) = object.get("message") {
            deltas.extend(self.ingest_message(message)?);
        }

        let value = object.get("v");
        if let Some(value_object) = value.and_then(Value::as_object) {
            if let Some(conversation_id) = value_object
                .get("conversation_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                self.conversation_id = Some(conversation_id.to_string());
            }
            if let Some(message) = value_object.get("message") {
                deltas.extend(self.ingest_message(message)?);
            }
        }

        let explicit_operation = object.get("o").and_then(Value::as_str);
        let operation = explicit_operation
            .map(str::to_string)
            .or_else(|| self.last_operation.clone());
        if explicit_operation.is_some() {
            self.last_operation = operation.clone();
        }
        let explicit_path = object.get("p").and_then(Value::as_str);
        let path = explicit_path
            .map(str::to_string)
            .or_else(|| self.last_path.clone());
        if explicit_path.is_some() {
            self.last_path = path.clone();
        }

        if operation.as_deref() == Some("patch") {
            if let Some(items) = value.and_then(Value::as_array) {
                for item in items {
                    deltas.extend(self.ingest_value(item)?);
                }
            }
            return Ok(deltas);
        }

        if let (Some(operation), Some(path), Some(value)) =
            (operation.as_deref(), path.as_deref(), value)
        {
            match (operation, path) {
                ("append", "/message/content/parts/0") => {
                    if self.visible_assistant() {
                        if let Some(text) = value.as_str() {
                            self.assistant_text.push_str(text);
                            deltas.push(text.to_string());
                        }
                    }
                }
                ("replace", "/message/status") => {
                    self.assistant_status = value.as_str().map(str::to_string)
                }
                ("replace", "/message/end_turn") => {
                    self.assistant_end_turn = value.as_bool()
                }
                ("replace", "/message/recipient") => {
                    self.assistant_recipient = value.as_str().map(str::to_string)
                }
                _ => {}
            }
        }

        Ok(deltas)
    }

    fn ingest_message(&mut self, message: &Value) -> Result<Vec<String>, String> {
        let Some(message) = message.as_object() else {
            return Ok(Vec::new());
        };
        if message
            .get("author")
            .and_then(|value| value.get("role"))
            .and_then(Value::as_str)
            != Some("assistant")
        {
            return Ok(Vec::new());
        }
        let recipient = message.get("recipient").and_then(Value::as_str);
        let channel = message.get("channel").and_then(Value::as_str);
        if recipient.is_some_and(|value| value != "all") || channel == Some("commentary") {
            return Ok(Vec::new());
        }

        self.assistant_recipient = recipient.map(str::to_string);
        if let Some(id) = message
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            self.assistant_message_id = Some(id.to_string());
        }
        if let Some(status) = message.get("status").and_then(Value::as_str) {
            self.assistant_status = Some(status.to_string());
        }
        if let Some(end_turn) = message.get("end_turn").and_then(Value::as_bool) {
            self.assistant_end_turn = Some(end_turn);
        }

        let text = message
            .get("content")
            .and_then(|value| value.get("parts"))
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        if text.is_empty() {
            return Ok(Vec::new());
        }
        if text == self.assistant_text || self.assistant_text.starts_with(&text) {
            return Ok(Vec::new());
        }
        if text.starts_with(&self.assistant_text) {
            let delta = text[self.assistant_text.len()..].to_string();
            self.assistant_text = text;
            return Ok((!delta.is_empty()).then_some(delta).into_iter().collect());
        }
        Err("stream_rewrite_detected: ChatGPT rewrote text that was already emitted".into())
    }

    fn visible_assistant(&self) -> bool {
        self.assistant_recipient
            .as_deref()
            .is_none_or(|recipient| recipient == "all")
    }
}

fn stream_success(state: &ChatGptStreamState) -> bool {
    state.saw_done
        && (state.stream_complete
            || (state.assistant_status.as_deref() == Some("finished_successfully")
                && state.assistant_end_turn == Some(true)))
        && !state.assistant_text.is_empty()
}

fn validate_stream_success(state: &ChatGptStreamState) -> Result<(), BrowserProviderError> {
    if !state.saw_done {
        return Err(BrowserProviderError::Transport(
            "ChatGPT stream ended before data: [DONE]".into(),
        ));
    }
    if !(state.stream_complete
        || (state.assistant_status.as_deref() == Some("finished_successfully")
            && state.assistant_end_turn == Some(true)))
    {
        return Err(BrowserProviderError::Transport(
            "ChatGPT stream ended without a successful assistant completion marker".into(),
        ));
    }
    if state.assistant_text.is_empty() {
        return Err(BrowserProviderError::Transport(
            "ChatGPT stream completed without visible assistant text".into(),
        ));
    }
    Ok(())
}

async fn consume_response_bytes(
    response: Response,
    state: &mut ChatGptStreamState,
) -> Result<(), BrowserProviderError> {
    let mut decoder = SseDecoder::default();
    let mut bytes = response.bytes_stream();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk.map_err(|error| BrowserProviderError::Transport(error.to_string()))?;
        for event in decoder
            .push(&chunk)
            .map_err(BrowserProviderError::Transport)?
        {
            let _ = state
                .ingest_event(&event.data)
                .map_err(BrowserProviderError::Transport)?;
        }
    }
    for event in decoder
        .finish()
        .map_err(BrowserProviderError::Transport)?
    {
        let _ = state
            .ingest_event(&event.data)
            .map_err(BrowserProviderError::Transport)?;
    }
    Ok(())
}

fn openai_delta_event(
    completion_id: &str,
    created: i64,
    model: &str,
    delta: &str,
) -> Value {
    json!({
        "id": completion_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {"content": delta},
            "finish_reason": Value::Null
        }]
    })
}

fn synthetic_json_response(
    model: &str,
    text: &str,
) -> Result<reqwest::Response, BrowserProviderError> {
    let body = json!({
        "id": format!("chatcmpl_chatgpt_{}", Uuid::new_v4().simple()),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }]
    });
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

    fn test_session() -> ChatGptSession {
        ChatGptSession {
            access_token: "token".into(),
            cookie_header: "oai-did=device".into(),
            user_agent: "Mozilla/5.0 Test Browser".into(),
            device_id: "device".into(),
            session_id: "session".into(),
            language: "en-US".into(),
            client_version: String::new(),
        }
    }

    #[test]
    fn sentinel_config_has_current_wire_shape() {
        let config = sentinel_config(&test_session());
        assert_eq!(config.len(), 25);
        assert_eq!(config[4], json!("Mozilla/5.0 Test Browser"));
        assert!(prepare_token(&config).unwrap().starts_with("gAAAAAC"));
    }

    #[test]
    fn fnv_hash_is_stable() {
        assert_eq!(fnv_avalanche_hex("hello"), fnv_avalanche_hex("hello"));
        assert_ne!(fnv_avalanche_hex("hello"), fnv_avalanche_hex("world"));
    }

    #[test]
    fn sse_parser_requires_done_and_completion() {
        let mut state = ChatGptStreamState::default();
        state
            .ingest_event(
                &json!({
                    "v": {
                        "message": {
                            "id": "assistant-1",
                            "author": {"role":"assistant"},
                            "recipient":"all",
                            "content":{"content_type":"text","parts":["hello"]},
                            "status":"finished_successfully",
                            "end_turn":true
                        },
                        "conversation_id":"conv-1"
                    }
                })
                .to_string(),
            )
            .unwrap();
        assert!(!stream_success(&state));
        state.ingest_event("[DONE]").unwrap();
        assert!(stream_success(&state));
        assert_eq!(state.conversation_id.as_deref(), Some("conv-1"));
        assert_eq!(state.assistant_message_id.as_deref(), Some("assistant-1"));
    }

    #[test]
    fn patch_stream_appends_visible_assistant_text() {
        let mut state = ChatGptStreamState::default();
        state.assistant_recipient = Some("all".into());
        let first = state
            .ingest_event(
                &json!({"p":"/message/content/parts/0","o":"append","v":"Hello"})
                    .to_string(),
            )
            .unwrap();
        let second = state
            .ingest_event(&json!({"v":" world"}).to_string())
            .unwrap();
        assert_eq!(first, vec!["Hello"]);
        assert_eq!(second, vec![" world"]);
        assert_eq!(state.assistant_text, "Hello world");
    }

    #[test]
    fn serializes_latest_user_for_native_continuation() {
        let body = json!({
            "messages":[
                {"role":"user","content":"first"},
                {"role":"assistant","content":"answer"},
                {"role":"user","content":"second"}
            ]
        });
        assert_eq!(serialize_prompt(&body, true).unwrap(), "second");
        assert!(serialize_prompt(&body, false).unwrap().contains("User: first"));
    }

    #[test]
    fn browser_challenges_are_detected_without_solving_them() {
        assert!(challenge_required(Some(&json!({"required":true,"dx":"abc"}))));
        assert!(!challenge_required(Some(&json!({"required":false}))));
    }
}
