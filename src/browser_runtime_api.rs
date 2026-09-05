use crate::{
    api::{authorize, json_error, json_response, AppState},
    browser_auth_runtime, browser_provider_runtime, browser_session_runtime, chromium_driver_runtime,
};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Response, StatusCode},
};
use serde_json::{json, Value};

pub async fn browser_account_runtime_diagnostics(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    let config = state.gateway.config_snapshot();
    let Some(account) = config.account(&account_id) else {
        return json_error(
            StatusCode::NOT_FOUND,
            "browser_runtime_diagnostics_error",
            &format!("account '{account_id}' was not found"),
        );
    };
    let Some(provider) = config.provider(&account.provider) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "browser_runtime_diagnostics_error",
            &format!(
                "account '{account_id}' references unknown provider '{}'",
                account.provider
            ),
        );
    };
    if !provider.is_browser() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "browser_runtime_diagnostics_error",
            &format!("account '{account_id}' is not a browser account"),
        );
    }

    let Some(registry) = browser_provider_runtime::get() else {
        return unavailable("browser provider runtime is not initialized");
    };
    let Some(session_id) = registry.session_id_for_account(&account_id) else {
        return json_error(
            StatusCode::NOT_FOUND,
            "browser_runtime_diagnostics_error",
            &format!("browser account '{account_id}' has no configured session binding"),
        );
    };
    let Some(session_store) = browser_session_runtime::get() else {
        return unavailable("browser session runtime is not initialized");
    };
    let session = match session_store.session(&session_id).await {
        Ok(session) => session,
        Err(error) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "browser_runtime_diagnostics_error",
                &error.to_string(),
            )
        }
    };

    let adapter = registry
        .adapter_diagnostics(&provider.kind, &account_id)
        .await;
    let last_execution = registry.last_transport_execution(&account_id).await;
    let refresh_required = registry.model_catalog_refresh_required(&account_id);
    let model_catalog = match state.catalog.models().await {
        Ok(models) => {
            let mut count = 0usize;
            let mut discovered_at: Option<String> = None;
            for model in models {
                let Some(binding) = model.accounts.into_iter().find(|binding| {
                    binding.account_id == account_id
                        && binding.enabled
                        && binding.discovered
                        && binding.availability != "unavailable"
                }) else {
                    continue;
                };
                count += 1;
                if let Some(timestamp) = binding.last_verified_at.or(binding.last_seen_at) {
                    if discovered_at
                        .as_ref()
                        .is_none_or(|current| timestamp.as_str() > current.as_str())
                    {
                        discovered_at = Some(timestamp);
                    }
                }
            }
            json!({
                "count": count,
                "discovered_at": discovered_at,
                "refresh_required": refresh_required,
            })
        }
        Err(_) => json!({
            "count": 0,
            "discovered_at": Value::Null,
            "refresh_required": true,
        }),
    };
    let auth_snapshot_available = browser_auth_runtime::get()
        .is_some_and(|vault| vault.contains(&session_id));
    let browser = match chromium_driver_runtime::get() {
        Some(driver) => driver.status(&session_id).await.ok(),
        None => None,
    };
    let browser_running = browser.as_ref().is_some_and(|status| status.running);
    let direct_ready = adapter.status == "ready"
        && adapter
            .adapter_id
            .as_deref()
            .is_some_and(|adapter_id| registry.is_direct_http_adapter_id(adapter_id));
    let effective_transport = if direct_ready {
        "direct-http"
    } else if browser_running {
        "browser-cdp"
    } else if adapter.status == "browser_fallback_required" {
        "browser-fallback-required"
    } else {
        "unavailable"
    };

    json_response(
        StatusCode::OK,
        json!({
            "account_id": account_id,
            "provider_id": provider.id,
            "provider_kind": provider.kind,
            "session_id": session_id,
            "session": {
                "status": session.status,
                "enabled": session.enabled,
                "routable": session.routable,
                "last_ready_at": session.last_ready_at,
                "last_verified_at": session.last_verified_at,
                "last_error": session.last_error,
            },
            "auth_snapshot_available": auth_snapshot_available,
            "adapter": adapter,
            "model_catalog": model_catalog,
            "last_execution": last_execution,
            "browser": browser,
            "effective_transport": effective_transport,
            "direct_ready": direct_ready,
            "browser_running": browser_running,
        }),
        None,
    )
}

pub async fn browser_thread_affinity_diagnostics(
    State(state): State<AppState>,
    Path((thread_id, account_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    let config = state.gateway.config_snapshot();
    let Some(account) = config.account(&account_id) else {
        return json_error(
            StatusCode::NOT_FOUND,
            "browser_affinity_diagnostics_error",
            &format!("account '{account_id}' was not found"),
        );
    };
    let Some(provider) = config.provider(&account.provider) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "browser_affinity_diagnostics_error",
            &format!(
                "account '{account_id}' references unknown provider '{}'",
                account.provider
            ),
        );
    };
    if !provider.is_browser() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "browser_affinity_diagnostics_error",
            &format!("account '{account_id}' is not a browser account"),
        );
    }

    if let Err(error) = state.conversations.thread(&thread_id).await {
        return json_error(
            StatusCode::NOT_FOUND,
            "browser_affinity_diagnostics_error",
            &error.to_string(),
        );
    }

    let mapping = match state
        .conversations
        .provider_conversation(&thread_id, &provider.id, &account_id)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "browser_affinity_diagnostics_error",
                &error.to_string(),
            )
        }
    };
    let raw_state = match state
        .conversations
        .provider_conversation_state(&thread_id, &provider.id, &account_id)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "browser_affinity_diagnostics_error",
                &error.to_string(),
            )
        }
    };

    let safe_state = raw_state
        .as_ref()
        .map(summarize_provider_state)
        .unwrap_or_else(|| json!({
            "present": false,
            "transport": Value::Null,
            "needs_resync": false,
            "conversation_id_present": false,
            "parent_message_id_present": false,
            "metadata_present": false,
            "response_id_present": false,
            "candidate_id_present": false,
            "model_external_id_present": false,
            "native_chain": Value::Null
        }));

    json_response(
        StatusCode::OK,
        json!({
            "thread_id": thread_id,
            "account_id": account_id,
            "provider_id": provider.id,
            "provider_kind": provider.kind,
            "mapping": mapping,
            "state": safe_state
        }),
        None,
    )
}

fn summarize_provider_state(state: &Value) -> Value {
    let transport = state.get("transport").and_then(Value::as_str);
    let conversation_id =
        first_non_empty_string(state, &["conversation_id", "chat_id", "chat_session_id"]);
    let parent_id = first_non_empty_string(
        state,
        &[
            "parent_message_id",
            "parent_id",
            "next_parent_id",
            "response_message_id",
        ],
    );
    let request_parent_id = first_non_empty_string(state, &["request_parent_id"]);
    let response_id = first_non_empty_string(state, &["response_id", "response_message_id"]);
    let candidate_id = first_non_empty_string(state, &["candidate_id"]);
    let model_external_id = first_non_empty_string(state, &["model_external_id"]);
    let native_chain_present = [
        conversation_id,
        parent_id,
        request_parent_id,
        response_id,
        candidate_id,
        model_external_id,
    ]
    .iter()
    .any(Option::is_some);

    json!({
        "present": true,
        "transport": transport,
        "needs_resync": state
            .get("needs_resync")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "conversation_id_present": conversation_id.is_some(),
        "parent_message_id_present": parent_id.is_some(),
        "metadata_present": state.get("metadata").is_some_and(|value| !value.is_null()),
        "response_id_present": response_id.is_some(),
        "candidate_id_present": candidate_id.is_some(),
        "model_external_id_present": model_external_id.is_some(),
        "native_chain": native_chain_present.then(|| json!({
            "conversation_id": conversation_id,
            "parent_id": parent_id,
            "request_parent_id": request_parent_id,
            "response_id": response_id,
            "candidate_id": candidate_id,
            "model_external_id": model_external_id
        }))
    })
}

fn first_non_empty_string<'a>(state: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        state
            .get(*key)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
    })
}

fn unavailable(message: &str) -> Response<Body> {
    json_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "browser_runtime_diagnostics_error",
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen_state_summary_uses_generic_native_chain() {
        let state = json!({
            "transport": "qwen-http",
            "chat_id": "chat-1",
            "parent_id": "parent-1",
            "request_parent_id": "response-0",
            "response_id": "response-1",
            "token": "secret-token",
            "metadata": {"secret": "private"}
        });
        let summary = summarize_provider_state(&state);
        assert_eq!(summary["native_chain"]["conversation_id"], "chat-1");
        assert_eq!(summary["native_chain"]["parent_id"], "parent-1");
        assert_eq!(summary["native_chain"]["request_parent_id"], "response-0");
        assert_eq!(summary["native_chain"]["response_id"], "response-1");
        assert!(summary.get("qwen_native").is_none());
        let rendered = summary.to_string();
        assert!(!rendered.contains("secret-token"));
        assert!(!rendered.contains("private"));
    }

    #[test]
    fn deepseek_state_summary_uses_same_generic_native_chain() {
        let state = json!({
            "transport": "deepseek-http",
            "chat_session_id": "session-1",
            "conversation_id": "session-1",
            "request_parent_id": "message-0",
            "response_message_id": "message-1",
            "response_id": "message-1",
            "next_parent_id": "message-1",
            "model_external_id": "deepseek-web-reasoning",
            "access_token": "secret-token"
        });
        let summary = summarize_provider_state(&state);
        assert_eq!(summary["native_chain"]["conversation_id"], "session-1");
        assert_eq!(summary["native_chain"]["parent_id"], "message-1");
        assert_eq!(summary["native_chain"]["request_parent_id"], "message-0");
        assert_eq!(summary["native_chain"]["response_id"], "message-1");
        assert_eq!(
            summary["native_chain"]["model_external_id"],
            "deepseek-web-reasoning"
        );
        assert!(!summary.to_string().contains("secret-token"));
    }

    #[test]
    fn provider_state_summary_does_not_expose_tokens_or_metadata_contents() {
        let state = json!({
            "transport": "chatgpt-http",
            "conversation_id": "conversation-1",
            "parent_message_id": "message-1",
            "conduit_token": "secret-conduit",
            "metadata": {"secret": "provider-private"},
            "needs_resync": false
        });

        let summary = summarize_provider_state(&state);
        assert_eq!(summary["present"], true);
        assert_eq!(summary["transport"], "chatgpt-http");
        assert_eq!(summary["conversation_id_present"], true);
        assert_eq!(summary["parent_message_id_present"], true);
        assert_eq!(summary["metadata_present"], true);
        assert!(summary.get("conduit_token").is_none());
        assert!(summary.get("metadata").is_none());
    }

    #[test]
    fn provider_state_summary_marks_missing_native_state() {
        let summary = summarize_provider_state(&json!({
            "transport": "browser-cdp",
            "needs_resync": true
        }));
        assert_eq!(summary["needs_resync"], true);
        assert_eq!(summary["conversation_id_present"], false);
        assert_eq!(summary["parent_message_id_present"], false);
    }
}
