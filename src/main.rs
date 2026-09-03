mod admin;
mod admin_api;
mod api;
mod catalog;
mod compat;
mod config;
mod context_engine;
mod context_runtime;
mod conversation;
mod conversation_api;
mod gateway;
mod memory_api;
mod response_state;
mod routing;
mod structured_memory;
mod ui;

use admin_api::set_account_model;
use api::{
    admin_account_models, admin_accounts, admin_models, admin_refresh_account_models,
    anthropic_messages, health, models, openai_chat, openai_responses, AppState,
};
use axum::{
    routing::{get, post},
    Router,
};
use catalog::ModelCatalog;
use config::AppConfig;
use context_engine::ContextEngine;
use conversation::ConversationStore;
use conversation_api::{
    compact_thread_context, create_thread, delete_thread, get_thread, get_thread_context,
    list_threads, send_thread_message,
};
use gateway::Gateway;
use memory_api::get_thread_memory;
use std::{env, net::SocketAddr, sync::Arc};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::info;
use tracing_subscriber::EnvFilter;
use ui::{app_css, app_js, index as ui_index};

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
    let config = Arc::new(AppConfig::load(&config_path)?);
    let gateway_api_key = Arc::new(config.gateway_api_key()?);

    let catalog = Arc::new(ModelCatalog::connect(config.clone()).await?);
    catalog.seed_from_config().await?;
    let conversations = Arc::new(ConversationStore::connect(config.clone()).await?);
    let gateway = Arc::new(Gateway::new(config.clone(), catalog.clone())?);
    let context_engine = Arc::new(
        ContextEngine::connect(
            config.clone(),
            conversations.clone(),
            catalog.clone(),
            gateway.clone(),
        )
        .await?,
    );
    context_runtime::install(context_engine)
        .map_err(|_| "context engine was already initialized")?;

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
        .route("/v1/chat/completions", post(openai_chat))
        .route("/v1/responses", post(openai_responses))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/models", get(models))
        .route("/v1/threads", get(list_threads).post(create_thread))
        .route("/v1/threads/{thread_id}", get(get_thread).delete(delete_thread))
        .route(
            "/v1/threads/{thread_id}/messages",
            post(send_thread_message),
        )
        .route(
            "/v1/threads/{thread_id}/context",
            get(get_thread_context),
        )
        .route(
            "/v1/threads/{thread_id}/memory",
            get(get_thread_memory),
        )
        .route(
            "/v1/threads/{thread_id}/compact",
            post(compact_thread_context),
        )
        .route("/_llmgateway/health", get(health))
        .route("/_llmgateway/models", get(admin_models))
        .route("/_llmgateway/accounts", get(admin_accounts))
        .route(
            "/_llmgateway/accounts/{account_id}/models",
            get(admin_account_models).patch(set_account_model),
        )
        .route(
            "/_llmgateway/accounts/{account_id}/models/refresh",
            post(admin_refresh_account_models),
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
