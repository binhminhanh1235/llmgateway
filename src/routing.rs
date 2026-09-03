use crate::config::{AppConfig, RouteConfig};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

#[derive(Clone, Debug, Default, Serialize)]
pub struct RouteHealth {
    pub consecutive_failures: u32,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct Router {
    config: Arc<AppConfig>,
    health: Arc<RwLock<HashMap<String, RouteHealth>>>,
}

impl Router {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            config,
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
            let mut exact = self
                .config
                .routes
                .iter()
                .filter(|route| route.model == resolved)
                .cloned()
                .collect::<Vec<_>>();
            exact.sort_by_key(|route| route.priority);
            exact
        };

        candidates.retain(|route| route.enabled);

        let now = Utc::now();
        let health = self.health.read().await;
        candidates.retain(|route| {
            health
                .get(&route.id)
                .and_then(|state| state.cooldown_until.as_ref())
                .map(|until| until <= &now)
                .unwrap_or(true)
        });

        candidates
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
