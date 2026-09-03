use crate::{
    browser_provider::BrowserProviderRegistry,
    browser_provider_runtime,
    catalog::ModelCatalog,
    config::{AppConfig, RouteConfig},
    quota_usage_runtime,
};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::RwLock;
use tracing::warn;

#[derive(Clone, Debug, Default, Serialize)]
pub struct RouteHealth {
    pub consecutive_failures: u32,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct Router {
    config: Arc<AppConfig>,
    catalog: Arc<ModelCatalog>,
    health: Arc<RwLock<HashMap<String, RouteHealth>>>,
}

impl Router {
    pub fn new(config: Arc<AppConfig>, catalog: Arc<ModelCatalog>) -> Self {
        Self {
            config,
            catalog,
            health: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn plan(&self, requested_model: &str) -> Vec<RouteConfig> {
        let resolved = self.config.resolve_model_alias(requested_model);
        let mut candidates = if let Some(vm) = self.config.virtual_models.get(resolved) {
            vm.routes
                .iter()
                .filter_map(|id| self.config.route(id).cloned())
                .collect::<Vec<_>>()
        } else if let Some(route) = self.config.route(resolved) {
            vec![route.clone()]
        } else {
            self.routes_for_physical_model(resolved).await
        };

        candidates.retain(|route| route.enabled);

        let now = Utc::now();
        {
            let health = self.health.read().await;
            candidates.retain(|route| {
                health
                    .get(&route.id)
                    .and_then(|state| state.cooldown_until.as_ref())
                    .map(|until| until <= &now)
                    .unwrap_or(true)
            });
        }

        candidates = self.filter_runtime_available(candidates).await;

        if let Some(usage) = quota_usage_runtime::get() {
            let mut scored = Vec::with_capacity(candidates.len());
            for route in candidates {
                match usage.route_penalty(&route.account).await {
                    Ok(Some(penalty)) => scored.push((route, penalty)),
                    Ok(None) => {
                        tracing::debug!(
                            account = %route.account,
                            route = %route.id,
                            "quota engine excluded route"
                        );
                    }
                    Err(error) => {
                        warn!(%error, account = %route.account, "quota scoring failed; keeping route");
                        scored.push((route, 0));
                    }
                }
            }
            scored.sort_by_key(|(route, penalty)| route.priority.saturating_add(*penalty));
            return scored.into_iter().map(|(route, _)| route).collect();
        }

        candidates.sort_by_key(|route| route.priority);
        candidates
    }

    async fn filter_runtime_available(&self, candidates: Vec<RouteConfig>) -> Vec<RouteConfig> {
        let mut available = Vec::with_capacity(candidates.len());
        for route in candidates {
            let Some(account) = self.config.account(&route.account) else {
                continue;
            };
            let Some(provider) = self.config.provider(&account.provider) else {
                continue;
            };
            if BrowserProviderRegistry::is_browser_kind(&provider.kind) {
                let Some(registry) = browser_provider_runtime::get() else {
                    tracing::debug!(route = %route.id, "browser provider runtime unavailable; skipping route");
                    continue;
                };
                if !registry.route_available(&provider.kind, &account.id).await {
                    tracing::debug!(
                        route = %route.id,
                        account = %account.id,
                        provider = %provider.id,
                        "browser route skipped because its session is not ready"
                    );
                    continue;
                }
            }
            available.push(route);
        }
        available
    }

    async fn routes_for_physical_model(&self, requested: &str) -> Vec<RouteConfig> {
        let (provider_filter, external_id) = requested
            .split_once('/')
            .filter(|(provider, _)| self.config.provider(provider).is_some())
            .map(|(provider, external)| (Some(provider), external))
            .unwrap_or((None, requested));

        let models = match self.catalog.models().await {
            Ok(models) => models,
            Err(error) => {
                warn!(%error, "failed to read model catalog while planning route");
                return self.config_routes_for_model(provider_filter, external_id);
            }
        };

        let mut routes = Vec::new();
        let mut seen = HashSet::new();
        for model in models.into_iter().filter(|model| {
            model.external_id == external_id
                && provider_filter.is_none_or(|provider| model.provider == provider)
        }) {
            for binding in model.accounts.into_iter().filter(|binding| {
                binding.enabled
                    && matches!(binding.availability.as_str(), "available" | "unknown")
            }) {
                let Some(account) = self.config.account(&binding.account_id) else {
                    continue;
                };
                if !account.enabled || account.provider != model.provider {
                    continue;
                }
                let key = format!("{}\0{}", account.id, external_id);
                if !seen.insert(key) {
                    continue;
                }

                if let Some(configured) = self.config.routes.iter().find(|route| {
                    route.enabled && route.account == account.id && route.model == external_id
                }) {
                    routes.push(configured.clone());
                    continue;
                }

                let account_order = self
                    .config
                    .accounts
                    .iter()
                    .position(|candidate| candidate.id == account.id)
                    .unwrap_or(self.config.accounts.len()) as i32;
                routes.push(RouteConfig {
                    id: format!("discovered:{}:{}", account.id, external_id),
                    account: account.id.clone(),
                    model: external_id.to_string(),
                    priority: 1000 + account_order,
                    enabled: true,
                    capabilities: model.capabilities.clone(),
                });
            }
        }

        if routes.is_empty() {
            self.config_routes_for_model(provider_filter, external_id)
        } else {
            routes
        }
    }

    fn config_routes_for_model(
        &self,
        provider_filter: Option<&str>,
        external_id: &str,
    ) -> Vec<RouteConfig> {
        self.config
            .routes
            .iter()
            .filter(|route| {
                route.enabled
                    && route.model == external_id
                    && provider_filter.is_none_or(|provider| {
                        self.config
                            .account(&route.account)
                            .is_some_and(|account| account.provider == provider)
                    })
            })
            .cloned()
            .collect()
    }

    pub async fn mark_success(&self, route_id: &str) {
        self.health
            .write()
            .await
            .insert(route_id.to_string(), RouteHealth::default());
    }

    pub async fn mark_failure(&self, route_id: &str, error: String, cooldown_secs: i64) {
        let mut health = self.health.write().await;
        let entry = health.entry(route_id.to_string()).or_default();
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.last_error = Some(error);
        entry.cooldown_until = Some(Utc::now() + Duration::seconds(cooldown_secs));
    }

    pub async fn snapshot(&self) -> HashMap<String, RouteHealth> {
        self.health.read().await.clone()
    }
}
