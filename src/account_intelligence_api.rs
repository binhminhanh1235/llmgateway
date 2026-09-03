use crate::{
    api::{authorize, json_error, json_response, AppState},
    browser_provider::BrowserProviderConfig,
    browser_session_runtime,
};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Response, StatusCode},
};
use serde::Serialize;
use serde_json::json;
use std::env;

#[derive(Debug, Serialize)]
struct BrowserSessionIntelligence {
    id: String,
    label: String,
    status: String,
    enabled: bool,
    last_verified_at: Option<String>,
    last_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct AccountIntelligence {
    id: String,
    provider: String,
    provider_kind: String,
    transport: String,
    enabled: bool,
    credential_required: bool,
    credential_configured: Option<bool>,
    discover_models: bool,
    model_count: i64,
    available_model_count: i64,
    route_ids: Vec<String>,
    routing_state: String,
    browser_session: Option<BrowserSessionIntelligence>,
}

pub async fn account_intelligence(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    let accounts = match state.catalog.accounts().await {
        Ok(accounts) => accounts,
        Err(error) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "account_intelligence_error",
                &error.to_string(),
            )
        }
    };

    let config_path =
        env::var("LLMGATEWAY_CONFIG").unwrap_or_else(|_| "config/llmgateway.toml".into());
    let browser_config = match BrowserProviderConfig::load_from_gateway_config(&config_path) {
        Ok(config) => config,
        Err(error) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "account_intelligence_config_error",
                &error.to_string(),
            )
        }
    };

    let mut data = Vec::with_capacity(accounts.len());
    for summary in accounts {
        let Some(account) = state.gateway.config.account(&summary.id) else {
            continue;
        };
        let Some(provider) = state.gateway.config.provider(&account.provider) else {
            continue;
        };

        let route_ids = state
            .gateway
            .config
            .routes
            .iter()
            .filter(|route| route.enabled && route.account == account.id)
            .map(|route| route.id.clone())
            .collect::<Vec<_>>();

        let credential_required = account.credential_required(provider);
        let credential_configured = credential_required.then(|| {
            env::var(&account.api_key_env)
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
        });

        let mut browser_session = None;
        let routing_state = if !account.enabled {
            "disabled".to_string()
        } else if provider.is_browser() {
            match browser_config.bindings.get(&account.id) {
                None => "unbound".to_string(),
                Some(binding) => match browser_session_runtime::get() {
                    None => "browser_runtime_unavailable".to_string(),
                    Some(store) => match store.session(&binding.session).await {
                        Ok(session) => {
                            let status = session.status.clone();
                            browser_session = Some(BrowserSessionIntelligence {
                                id: session.id,
                                label: session.label,
                                status: session.status,
                                enabled: session.enabled,
                                last_verified_at: session.last_verified_at,
                                last_error: session.last_error,
                            });
                            status
                        }
                        Err(_) => "session_unavailable".to_string(),
                    },
                },
            }
        } else if credential_configured == Some(false) {
            "credential_missing".to_string()
        } else {
            "ready".to_string()
        };

        data.push(AccountIntelligence {
            id: summary.id,
            provider: summary.provider,
            provider_kind: provider.kind.clone(),
            transport: provider.transport().to_string(),
            enabled: summary.enabled,
            credential_required,
            credential_configured,
            discover_models: summary.discover_models,
            model_count: summary.model_count,
            available_model_count: summary.available_model_count,
            route_ids,
            routing_state,
            browser_session,
        });
    }

    json_response(StatusCode::OK, json!({"data":data}), None)
}
