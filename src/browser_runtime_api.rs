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
    let auth_snapshot_available = browser_auth_runtime::get()
        .is_some_and(|vault| vault.contains(&session_id));
    let browser = match chromium_driver_runtime::get() {
        Some(driver) => driver.status(&session_id).await.ok(),
        None => None,
    };
    let browser_running = browser.as_ref().is_some_and(|status| status.running);
    let direct_ready = adapter.status == "ready"
        && matches!(
            adapter.adapter_id.as_deref(),
            Some("gemini-web-http" | "chatgpt-web-http")
        );
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
            "candidate_id_present": false
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
    json!({
        "present": true,
        "transport": state.get("transport").and_then(Value::as_str),
        "needs_resync": state
            .get("needs_resync")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "conversation_id_present": non_empty_string(state.get("conversation_id")),
        "parent_message_id_present": non_empty_string(state.get("parent_message_id")),
        "metadata_present": state.get("metadata").is_some_and(|value| !value.is_null()),
        "response_id_present": non_empty_string(state.get("response_id")),
        "candidate_id_present": non_empty_string(state.get("candidate_id")),
    })
}

fn non_empty_string(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
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
