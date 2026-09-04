mod account_intelligence_api;
mod admin;
mod admin_api;
mod api;
mod browser_account_setup;
mod browser_provider;
mod browser_provider_runtime;
mod browser_session;
mod browser_session_api;
mod browser_session_runtime;
mod catalog;
mod chromium_driver;
mod chromium_driver_api;
mod chromium_driver_runtime;
mod compat;
mod config;
mod context_engine;
mod context_runtime;
mod conversation;
mod conversation_api;
mod embedding_retrieval;
mod embedding_runtime;
mod execution_trace;
mod execution_trace_api;
mod gateway;
mod live_config;
mod memory_api;
mod memory_backfill;
mod memory_provenance;
mod memory_provenance_runtime;
mod quota_usage;
mod quota_usage_runtime;
mod response_state;
mod retrieval_api;
mod routing;
mod routing_api;
mod semantic_retrieval;
mod structured_memory;
mod ui;
mod usage_api;

use account_intelligence_api::account_intelligence;
use admin_api::set_account_model;
use api::{
    admin_account_models, admin_accounts, admin_models, admin_refresh_account_models,
    anthropic_messages, health, models, openai_chat, openai_responses, AppState,
};
use axum::{
    routing::{get, post},
    Router,
};
use browser_account_setup::{browser_account_setup_presets, create_browser_account_setup};
use browser_provider::{BrowserProviderConfig, BrowserProviderRegistry};
use browser_session::{BrowserConfig, BrowserSessionStore};
use browser_session_api::{
    begin_browser_login, complete_browser_login, get_browser_session, list_browser_sessions,
    require_browser_attention, reset_browser_session, verify_browser_session,
};
use catalog::ModelCatalog;
use chromium_driver::{ChromiumConfig, ChromiumDriver};
use chromium_driver_api::{
    chromium_status, launch_chromium_login, stop_chromium, verify_chromium_login,
};
use config::AppConfig;
use context_engine::ContextEngine;
use conversation::ConversationStore;
use conversation_api::{
    compact_thread_context, create_thread, delete_thread, get_thread, get_thread_context,
    list_threads, send_thread_message,
};
use embedding_retrieval::EmbeddingRetriever;
use execution_trace::ExecutionTraceStore;
use execution_trace_api::{get_execution, list_executions};
use gateway::Gateway;
use live_config::LiveConfig;
use memory_api::{add_thread_memory_pin, get_thread_memory, update_thread_memory_item};
use memory_backfill::backfill_legacy_memories;
use memory_provenance::MemoryProvenanceStore;
use quota_usage::{QuotaUsageStore, UsageConfig};
use retrieval_api::inspect_thread_retrieval;
use routing_api::explain_routes;
use std::{env, net::SocketAddr, sync::Arc};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use ui::{
    account_control_css, account_control_js, account_intelligence_css, account_intelligence_js,
    app_css, app_js, browser_control_css, browser_control_js, index as ui_index,
    trace_console_css, trace_console_js,
};
use usage_api::{get_account_usage, get_usage, reset_account_quota};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("llmgateway=info,tower_http=info")),
        )
        .init();

    let config_path =
        env::var("LLMGATEWAY_CONFIG").unwrap_or_else(|_| "config/llmgateway.toml".into());
    let usage_config = UsageConfig::load_from_gateway_config(&config_path)?;
    let browser_config = BrowserConfig::load_from_gateway_config(&config_path)?;
    let browser_provider_config = BrowserProviderConfig::load_from_gateway_config(&config_path)?;
    let chromium_config = ChromiumConfig::load_from_gateway_config(&config_path)?;
    let config = Arc::new(AppConfig::load(&config_path)?);
    let live_config = LiveConfig::new(config.clone());
    let gateway_api_key = Arc::new(config.gateway_api_key()?);

    let catalog = Arc::new(ModelCatalog::connect(config.clone()).await?);
    catalog.seed_from_config().await?;
    let conversations = Arc::new(ConversationStore::connect(config.clone()).await?);
    let execution_traces = Arc::new(ExecutionTraceStore::connect(config.clone()).await?);

    let quota_usage = Arc::new(QuotaUsageStore::connect(config.clone(), usage_config).await?);
    quota_usage_runtime::install(quota_usage)
        .map_err(|_| "quota usage store was already initialized")?;

    let browser_sessions = Arc::new(BrowserSessionStore::connect(config.clone(), browser_config).await?);
    let browser_session_count = browser_sessions.summary().await?.sessions.len();
    let chromium_driver = Arc::new(ChromiumDriver::new(chromium_config, browser_sessions.clone())?);
    let chromium_driver_enabled = chromium_driver.enabled();
    let startup_browser_reconcile = chromium_driver.reconcile_all().await;
    chromium_driver_runtime::install(chromium_driver.clone())
        .map_err(|_| "Chromium driver was already initialized")?;
    browser_session_runtime::install(browser_sessions)
        .map_err(|_| "browser session store was already initialized")?;

    let browser_providers = Arc::new(BrowserProviderRegistry::new(browser_provider_config)?);
    let browser_provider_bindings = browser_providers.binding_count();
    browser_provider_runtime::install(browser_providers)
        .map_err(|_| "browser provider registry was already initialized")?;

    if browser_session_count > 0 {
        info!(browser_session_count, "browser session registry enabled");
    }
    if chromium_driver_enabled {
        info!(
            checked = startup_browser_reconcile.checked,
            ready = startup_browser_reconcile.ready,
            recovered = startup_browser_reconcile.recovered,
            attention = startup_browser_reconcile.attention,
            "Chromium browser driver enabled and startup reconciliation completed"
        );

        let reconcile_driver = chromium_driver.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(
                    reconcile_driver.reconcile_interval_seconds(),
                ))
                .await;
                let summary = reconcile_driver.reconcile_all().await;
                if summary.recovered > 0 {
                    info!(
                        recovered = summary.recovered,
                        ready = summary.ready,
                        "browser sessions recovered automatically"
                    );
                }
                for session in summary.sessions.iter().filter(|session| session.error.is_some()) {
                    warn!(
                        session_id = %session.session_id,
                        action = %session.action,
                        error = %session.error.as_deref().unwrap_or("unknown"),
                        "browser session reconciliation needs attention"
                    );
                }
            }
        });
    }
    if browser_provider_bindings > 0 {
        info!(browser_provider_bindings, "browser provider account bindings enabled");
    }

    let gateway = Arc::new(Gateway::new(
        config.clone(),
        live_config,
        catalog.clone(),
        execution_traces,
    )?);
    match gateway.restore_adaptive_from_traces().await {
        Ok(restored) if restored > 0 => {
            info!(restored, "restored adaptive route samples from execution trace");
        }
        Ok(_) => {}
        Err(error) => {
            warn!(%error, "failed to restore adaptive route samples; starting with cold telemetry");
        }
    }
    let context_engine = Arc::new(
        ContextEngine::connect(
            config.clone(),
            conversations.clone(),
            catalog.clone(),
            gateway.clone(),
        )
        .await?,
    );
    let legacy_memories = backfill_legacy_memories(config.as_ref()).await?;
    if legacy_memories > 0 {
        info!(legacy_memories, "backfilled legacy context checkpoints into structured memory");
    }
    context_runtime::install(context_engine).map_err(|_| "context engine was already initialized")?;

    let memory_provenance = Arc::new(MemoryProvenanceStore::connect(config.clone()).await?);
    memory_provenance_runtime::install(memory_provenance)
        .map_err(|_| "memory provenance store was already initialized")?;

    if let Some(retriever) = EmbeddingRetriever::connect(config.clone()).await? {
        embedding_runtime::install(Arc::new(retriever))
            .map_err(|_| "embedding retriever was already initialized")?;
        info!("hybrid embedding retrieval enabled");
    }

    let state = AppState {
        gateway,
        catalog,
        conversations,
        gateway_api_key,
    };
    let app = Router::new()
        .route("/", get(ui_index))
        .route("/ui", get(ui_index))
        .route("/ui/app.css", get(app_css))
        .route("/ui/app.js", get(app_js))
        .route("/ui/account-control.css", get(account_control_css))
        .route("/ui/account-control.js", get(account_control_js))
        .route("/ui/account-intelligence.css", get(account_intelligence_css))
        .route("/ui/account-intelligence.js", get(account_intelligence_js))
        .route("/ui/browser-control.css", get(browser_control_css))
        .route("/ui/browser-control.js", get(browser_control_js))
        .route("/ui/trace-console.css", get(trace_console_css))
        .route("/ui/trace-console.js", get(trace_console_js))
        .route("/v1/chat/completions", post(openai_chat))
        .route("/v1/responses", post(openai_responses))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/models", get(models))
        .route("/v1/threads", get(list_threads).post(create_thread))
        .route("/v1/threads/{thread_id}", get(get_thread).delete(delete_thread))
        .route("/v1/threads/{thread_id}/messages", post(send_thread_message))
        .route("/v1/threads/{thread_id}/context", get(get_thread_context))
        .route("/v1/threads/{thread_id}/memory", get(get_thread_memory))
        .route("/v1/threads/{thread_id}/memory/pins", post(add_thread_memory_pin))
        .route(
            "/v1/threads/{thread_id}/memory/items/{item_key}",
            axum::routing::patch(update_thread_memory_item),
        )
        .route("/v1/threads/{thread_id}/retrieve", post(inspect_thread_retrieval))
        .route("/v1/threads/{thread_id}/compact", post(compact_thread_context))
        .route("/_llmgateway/health", get(health))
        .route("/_llmgateway/models", get(admin_models))
        .route("/_llmgateway/accounts", get(admin_accounts))
        .route("/_llmgateway/account-intelligence", get(account_intelligence))
        .route("/_llmgateway/routes/explain", post(explain_routes))
        .route("/_llmgateway/executions", get(list_executions))
        .route("/_llmgateway/executions/{request_id}", get(get_execution))
        .route(
            "/_llmgateway/accounts/{account_id}/models",
            get(admin_account_models).patch(set_account_model),
        )
        .route(
            "/_llmgateway/accounts/{account_id}/models/refresh",
            post(admin_refresh_account_models),
        )
        .route("/_llmgateway/usage", get(get_usage))
        .route(
            "/_llmgateway/accounts/{account_id}/usage",
            get(get_account_usage),
        )
        .route(
            "/_llmgateway/accounts/{account_id}/quota/reset",
            post(reset_account_quota),
        )
        .route(
            "/_llmgateway/browser-account-setup/providers",
            get(browser_account_setup_presets),
        )
        .route(
            "/_llmgateway/browser-account-setup",
            post(create_browser_account_setup),
        )
        .route("/_llmgateway/browser-sessions", get(list_browser_sessions))
        .route(
            "/_llmgateway/browser-sessions/{session_id}",
            get(get_browser_session),
        )
        .route(
            "/_llmgateway/browser-sessions/{session_id}/login/start",
            post(begin_browser_login),
        )
        .route(
            "/_llmgateway/browser-sessions/{session_id}/login/complete",
            post(complete_browser_login),
        )
        .route(
            "/_llmgateway/browser-sessions/{session_id}/verify",
            post(verify_browser_session),
        )
        .route(
            "/_llmgateway/browser-sessions/{session_id}/attention",
            post(require_browser_attention),
        )
        .route(
            "/_llmgateway/browser-sessions/{session_id}/reset",
            post(reset_browser_session),
        )
        .route(
            "/_llmgateway/browser-sessions/{session_id}/driver/launch",
            post(launch_chromium_login),
        )
        .route(
            "/_llmgateway/browser-sessions/{session_id}/driver/status",
            get(chromium_status),
        )
        .route(
            "/_llmgateway/browser-sessions/{session_id}/driver/verify",
            post(verify_chromium_login),
        )
        .route(
            "/_llmgateway/browser-sessions/{session_id}/driver/stop",
            post(stop_chromium),
        )
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive());

    let address = SocketAddr::new(config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(address).await?;
    info!(%address, "llmgateway listening");
    axum::serve(listener, app).await?;
    Ok(())
}
