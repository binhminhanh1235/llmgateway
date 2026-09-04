use crate::{
    api::{authorize, json_error, json_response, AppState},
    browser_provider::{BrowserAdapterDiagnostics, BrowserProviderConfig},
    browser_provider_runtime, browser_session_runtime,
    routing::AccountReadiness,
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
    readiness: AccountReadiness,
    browser_session: Option<BrowserSessionIntelligence>,
    browser_adapter: Option<BrowserAdapterDiagnostics>,
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

    let config = state.gateway.config_snapshot();
    let mut data = Vec::with_capacity(accounts.len());
    for summary in accounts {
        let Some(account) = config.account(&summary.id) else {
            continue;
        };
        let Some(provider) = config.provider(&account.provider) else {
            continue;
        };

        let route_ids = config
            .routes
            .iter()
            .filter(|route| route.enabled && route.account == account.id)
            .map(|route| route.id.clone())
            .collect::<Vec<_>>();

        let readiness = state.gateway.router.account_readiness(&account.id).await;
        let credential_required = account.credential_required(provider);
        let credential_configured = readiness.credential_configured;

        let mut browser_session = None;
        let mut browser_adapter = None;
        if provider.is_browser() {
            if let Some(binding) = browser_config.bindings.get(&account.id) {
                if let Some(store) = browser_session_runtime::get() {
                    if let Ok(session) = store.session(&binding.session).await {
                        browser_session = Some(BrowserSessionIntelligence {
                            id: session.id,
                            label: session.label,
                            status: session.status,
                            enabled: session.enabled,
                            last_verified_at: session.last_verified_at,
                            last_error: session.last_error,
                        });
                    }
                }
            }
            if let Some(registry) = browser_provider_runtime::get() {
                browser_adapter = Some(
                    registry
                        .adapter_diagnostics(&provider.kind, &account.id)
                        .await,
                );
            }
        }

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
            routing_state: readiness.effective_status.clone(),
            readiness,
            browser_session,
            browser_adapter,
        });
    }

    json_response(StatusCode::OK, json!({"data":data}), None)
}
