mod account_readiness;
mod adaptive_scoring;
mod task_aware;

pub use account_readiness::AccountReadiness;
pub use adaptive_scoring::AdaptiveRouteSnapshot;
pub use task_aware::{TaskFitSnapshot, TaskProfile};

use crate::{
    browser_provider_runtime,
    catalog::ModelCatalog,
    config::{AppConfig, RouteConfig},
    live_config::LiveConfig,
    quota_usage_runtime,
};
use account_readiness::evaluate_base;
use adaptive_scoring::AdaptiveRouteState;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value;
use task_aware::{classify as classify_task, route_fit as evaluate_task_fit};
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

#[derive(Clone, Debug, Serialize)]
pub struct RouteCandidateDecision {
    pub route_id: String,
    pub account: String,
    pub model: String,
    pub transport: String,
    pub transport_preference: i32,
    pub base_priority: i32,
    pub eligible: bool,
    pub exclusion_reasons: Vec<String>,
    pub warnings: Vec<String>,
    pub readiness: AccountReadiness,
    pub route_health: RouteHealth,
    pub quota_penalty: Option<i32>,
    pub adaptive_penalty: i32,
    pub adaptive: AdaptiveRouteSnapshot,
    pub task_adjustment: i32,
    pub task_fit: TaskFitSnapshot,
    pub final_score: Option<i32>,
    pub rank: Option<usize>,
    pub selected: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct RouteDecisionTrace {
    pub requested_model: String,
    pub resolved_model: String,
    pub execution_preference: String,
    pub api_fallback: bool,
    pub task: TaskProfile,
    pub selected_route: Option<String>,
    pub candidates: Vec<RouteCandidateDecision>,
}

#[derive(Clone)]
struct EvaluatedRoute {
    route: RouteConfig,
    decision: RouteCandidateDecision,
    original_index: usize,
}

#[derive(Clone)]
enum QuotaEvaluation {
    Included(i32),
    Excluded,
    Unavailable(String),
}

#[derive(Clone)]
pub struct Router {
    config: Arc<AppConfig>,
    live_config: LiveConfig,
    catalog: Arc<ModelCatalog>,
    health: Arc<RwLock<HashMap<String, RouteHealth>>>,
    adaptive: Arc<RwLock<HashMap<String, AdaptiveRouteState>>>,
}

impl Router {
    pub fn new(config: Arc<AppConfig>, live_config: LiveConfig, catalog: Arc<ModelCatalog>) -> Self {
        Self {
            config,
            live_config,
            catalog,
            health: Arc::new(RwLock::new(HashMap::new())),
            adaptive: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn plan(&self, requested_model: &str) -> Vec<RouteConfig> {
        self.plan_for_body(requested_model, None).await
    }

    pub async fn plan_for_body(
        &self,
        requested_model: &str,
        body: Option<&Value>,
    ) -> Vec<RouteConfig> {
        let evaluation = self.evaluate(requested_model, body).await;
        let mut eligible = evaluation
            .candidates
            .into_iter()
            .filter(|candidate| candidate.decision.eligible)
            .collect::<Vec<_>>();
        eligible.sort_by_key(|candidate| candidate.decision.rank.unwrap_or(usize::MAX));
        eligible.into_iter().map(|candidate| candidate.route).collect()
    }

    pub async fn explain(&self, requested_model: &str) -> RouteDecisionTrace {
        self.explain_for_body(requested_model, None).await
    }

    pub fn sticky_route_matches_best_task_fit(
        &self,
        requested_model: &str,
        body: &Value,
        preferred: &RouteConfig,
        best: &RouteConfig,
    ) -> bool {
        let resolved_model = self.config.resolve_model_alias(requested_model);
        if self.config.virtual_models.contains_key(resolved_model)
            && self.transport_preference_rank(preferred) != self.transport_preference_rank(best)
        {
            return false;
        }
        if !self.config.routing.task_aware_enabled {
            return true;
        }
        let task = classify_task(Some(body), &self.config.routing);
        let preferred_fit = evaluate_task_fit(&task, preferred, &self.config.routing);
        let best_fit = evaluate_task_fit(&task, best, &self.config.routing);
        preferred_fit.exclusion_reason.is_none()
            && preferred_fit.snapshot.adjustment == best_fit.snapshot.adjustment
    }

    pub async fn explain_for_body(
        &self,
        requested_model: &str,
        body: Option<&Value>,
    ) -> RouteDecisionTrace {
        let evaluation = self.evaluate(requested_model, body).await;
        let selected_route = evaluation
            .candidates
            .iter()
            .find(|candidate| candidate.decision.selected)
            .map(|candidate| candidate.route.id.clone());
        RouteDecisionTrace {
            requested_model: requested_model.to_string(),
            resolved_model: evaluation.resolved_model,
            execution_preference: self.config.routing.execution_preference.clone(),
            api_fallback: self.config.routing.api_fallback,
            task: evaluation.task,
            selected_route,
            candidates: evaluation
                .candidates
                .into_iter()
                .map(|candidate| candidate.decision)
                .collect(),
        }
    }

    async fn evaluate(&self, requested_model: &str, body: Option<&Value>) -> RouteEvaluation {
        let resolved_model = self.config.resolve_model_alias(requested_model).to_string();
        let task = classify_task(body, &self.config.routing);
        let candidates = self.candidate_routes(&resolved_model).await;
        let apply_execution_policy = self.config.virtual_models.contains_key(&resolved_model);
        let now = Utc::now();
        let health_snapshot = self.health.read().await.clone();
        let adaptive_snapshot = self.adaptive.read().await.clone();
        let mut readiness_by_account: HashMap<String, AccountReadiness> = HashMap::new();
        let mut quota_by_account: HashMap<String, QuotaEvaluation> = HashMap::new();
        let mut evaluated = Vec::with_capacity(candidates.len());

        for (original_index, route) in candidates.into_iter().enumerate() {
            let readiness = if let Some(readiness) = readiness_by_account.get(&route.account) {
                readiness.clone()
            } else {
                let readiness = self.account_readiness(&route.account).await;
                readiness_by_account.insert(route.account.clone(), readiness.clone());
                readiness
            };
            let transport = self.route_transport(&route);
            let transport_preference = if apply_execution_policy {
                self.transport_preference_rank(&route)
            } else {
                0
            };
            let route_health = health_snapshot.get(&route.id).cloned().unwrap_or_default();
            let adaptive = adaptive_snapshot
                .get(&route.id)
                .cloned()
                .unwrap_or_default()
                .snapshot(&self.config.routing);
            let adaptive_penalty = adaptive.penalty;
            let task_fit = evaluate_task_fit(&task, &route, &self.config.routing);
            let task_adjustment = task_fit.snapshot.adjustment;
            let mut exclusion_reasons = Vec::new();
            let mut warnings = Vec::new();

            if !route.enabled {
                push_unique(&mut exclusion_reasons, "route_disabled");
            }
            if transport == "browser"
                && browser_provider_runtime::get()
                    .is_some_and(|registry| !registry.model_allowed(&route.account, &route.model))
            {
                push_unique(&mut exclusion_reasons, "browser_model_unavailable");
            }
            if apply_execution_policy
                && self.config.routing.execution_preference == "browser-first"
                && !self.config.routing.api_fallback
                && transport == "api"
            {
                push_unique(&mut exclusion_reasons, "api_fallback_disabled");
            }
            if let Some(reason) = task_fit.exclusion_reason {
                push_unique(&mut exclusion_reasons, reason);
            }
            if route_health
                .cooldown_until
                .as_ref()
                .is_some_and(|until| until > &now)
            {
                push_unique(&mut exclusion_reasons, "route_cooldown");
            }

            if readiness.routable {
                for reason in &readiness.reasons {
                    push_unique(&mut warnings, reason);
                }
            } else if readiness.reasons.is_empty() {
                push_unique(&mut exclusion_reasons, "account_unavailable");
            } else {
                for reason in &readiness.reasons {
                    push_unique(&mut exclusion_reasons, reason);
                }
            }

            let mut quota_penalty = None;
            let mut final_score = None;
            if exclusion_reasons.is_empty() {
                let quota = if let Some(cached) = quota_by_account.get(&route.account) {
                    cached.clone()
                } else {
                    let evaluated = match quota_usage_runtime::get() {
                        Some(usage) => match usage.route_penalty(&route.account).await {
                            Ok(Some(penalty)) => QuotaEvaluation::Included(penalty),
                            Ok(None) => QuotaEvaluation::Excluded,
                            Err(error) => QuotaEvaluation::Unavailable(error.to_string()),
                        },
                        None => QuotaEvaluation::Included(0),
                    };
                    quota_by_account.insert(route.account.clone(), evaluated.clone());
                    evaluated
                };

                match quota {
                    QuotaEvaluation::Included(penalty) => {
                        quota_penalty = Some(penalty);
                        final_score = Some(
                            route
                                .priority
                                .saturating_add(penalty)
                                .saturating_add(adaptive_penalty)
                                .saturating_add(task_adjustment),
                        );
                        if adaptive.active && adaptive_penalty > 0 {
                            push_unique(&mut warnings, "adaptive_degraded");
                        }
                        if task_adjustment < 0 {
                            push_unique(&mut warnings, "task_preferred");
                        } else if task_adjustment > 0 {
                            push_unique(&mut warnings, "task_mismatch");
                        }
                        if apply_execution_policy {
                            match self.config.routing.execution_preference.as_str() {
                                "browser-first" if transport == "browser" => {
                                    push_unique(&mut warnings, "browser_preferred");
                                }
                                "browser-first" if transport == "api" => {
                                    push_unique(&mut warnings, "api_fallback");
                                }
                                "api-first" if transport == "api" => {
                                    push_unique(&mut warnings, "api_preferred");
                                }
                                "api-first" if transport == "browser" => {
                                    push_unique(&mut warnings, "browser_fallback");
                                }
                                _ => {}
                            }
                        }
                    }
                    QuotaEvaluation::Excluded => {
                        push_unique(&mut exclusion_reasons, "quota_blocked");
                    }
                    QuotaEvaluation::Unavailable(error) => {
                        warn!(%error, account = %route.account, "quota scoring failed; keeping route");
                        push_unique(&mut warnings, "quota_scoring_unavailable");
                        quota_penalty = Some(0);
                        final_score = Some(
                            route
                                .priority
                                .saturating_add(adaptive_penalty)
                                .saturating_add(task_adjustment),
                        );
                        if adaptive.active && adaptive_penalty > 0 {
                            push_unique(&mut warnings, "adaptive_degraded");
                        }
                        if task_adjustment < 0 {
                            push_unique(&mut warnings, "task_preferred");
                        } else if task_adjustment > 0 {
                            push_unique(&mut warnings, "task_mismatch");
                        }
                        if apply_execution_policy {
                            match self.config.routing.execution_preference.as_str() {
                                "browser-first" if transport == "browser" => {
                                    push_unique(&mut warnings, "browser_preferred");
                                }
                                "browser-first" if transport == "api" => {
                                    push_unique(&mut warnings, "api_fallback");
                                }
                                "api-first" if transport == "api" => {
                                    push_unique(&mut warnings, "api_preferred");
                                }
                                "api-first" if transport == "browser" => {
                                    push_unique(&mut warnings, "browser_fallback");
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }

            let eligible = exclusion_reasons.is_empty() && final_score.is_some();
            evaluated.push(EvaluatedRoute {
                decision: RouteCandidateDecision {
                    route_id: route.id.clone(),
                    account: route.account.clone(),
                    model: route.model.clone(),
                    transport: transport.to_string(),
                    transport_preference,
                    base_priority: route.priority,
                    eligible,
                    exclusion_reasons,
                    warnings,
                    readiness,
                    route_health,
                    quota_penalty,
                    adaptive_penalty,
                    adaptive,
                    task_adjustment,
                    task_fit: task_fit.snapshot,
                    final_score,
                    rank: None,
                    selected: false,
                },
                route,
                original_index,
            });
        }

        let mut ranked_indices = evaluated
            .iter()
            .enumerate()
            .filter(|(_, candidate)| candidate.decision.eligible)
            .map(|(index, candidate)| {
                (
                    index,
                    candidate.decision.transport_preference,
                    candidate.decision.final_score.unwrap_or(i32::MAX),
                    candidate.original_index,
                )
            })
            .collect::<Vec<_>>();
        ranked_indices.sort_by_key(|(_, transport_preference, score, original_index)| {
            (*transport_preference, *score, *original_index)
        });
        for (rank_index, (candidate_index, _, _, _)) in ranked_indices.into_iter().enumerate() {
            let candidate = &mut evaluated[candidate_index];
            candidate.decision.rank = Some(rank_index + 1);
            candidate.decision.selected = rank_index == 0;
        }

        RouteEvaluation {
            resolved_model,
            task,
            candidates: evaluated,
        }
    }

    fn route_transport(&self, route: &RouteConfig) -> &'static str {
        self.config
            .account(&route.account)
            .and_then(|account| self.config.provider(&account.provider))
            .map(|provider| provider.transport())
            .unwrap_or("unknown")
    }

    fn transport_preference_rank(&self, route: &RouteConfig) -> i32 {
        match (self.config.routing.execution_preference.as_str(), self.route_transport(route)) {
            ("browser-first", "browser") | ("api-first", "api") => 0,
            ("browser-first", _) | ("api-first", _) => 1,
            _ => 0,
        }
    }

    async fn candidate_routes(&self, resolved: &str) -> Vec<RouteConfig> {
        if let Some(vm) = self.config.virtual_models.get(resolved) {
            vm.routes
                .iter()
                .filter_map(|id| self.config.route(id).cloned())
                .collect()
        } else if let Some(route) = self.config.route(resolved) {
            vec![route.clone()]
        } else {
            self.routes_for_physical_model(resolved).await
        }
    }

    pub async fn account_readiness(&self, account_id: &str) -> AccountReadiness {
        let mut readiness = evaluate_base(self.config.as_ref(), account_id).await;
        let routes = self
            .config
            .routes
            .iter()
            .filter(|route| route.enabled && route.account == account_id)
            .collect::<Vec<_>>();
        readiness.route_count = routes.len();

        let now = Utc::now();
        let health = self.health.read().await;
        for route in routes {
            let cooling = health
                .get(&route.id)
                .and_then(|state| state.cooldown_until.as_ref())
                .is_some_and(|until| until > &now);
            if cooling {
                readiness.cooling_route_count += 1;
            } else {
                readiness.healthy_route_count += 1;
            }
        }
        drop(health);

        if readiness.cooling_route_count > 0 {
            readiness.add_soft_reason("route_cooldown");
        }
        readiness
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
                    let mut configured = configured.clone();
                    if configured.context_window.is_none() {
                        configured.context_window = model.context_window;
                    }
                    for capability in &model.capabilities {
                        if !configured.capabilities.iter().any(|value| value == capability) {
                            configured.capabilities.push(capability.clone());
                        }
                    }
                    routes.push(configured);
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
                    context_window: model.context_window,
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

    pub async fn restore_adaptive_samples<I>(&self, samples: I) -> usize
    where
        I: IntoIterator<Item = (String, bool, u64, i64)>,
    {
        let mut adaptive = self.adaptive.write().await;
        let mut restored = 0usize;
        for (route_id, success, latency_ms, observed_at_ms) in samples {
            let entry = adaptive.entry(route_id).or_default();
            if success {
                entry.observe_success_at(latency_ms, observed_at_ms, &self.config.routing);
            } else {
                entry.observe_failure_at(latency_ms, observed_at_ms, &self.config.routing);
            }
            restored = restored.saturating_add(1);
        }
        restored
    }

    pub async fn mark_success(&self, route_id: &str, latency_ms: u64) {
        {
            let mut health = self.health.write().await;
            let entry = health.entry(route_id.to_string()).or_default();
            entry.consecutive_failures = 0;
            entry.last_error = None;
            entry.cooldown_until = None;
        }
        self.adaptive
            .write()
            .await
            .entry(route_id.to_string())
            .or_default()
            .observe_success(latency_ms, &self.config.routing);
    }

    pub async fn mark_failure(
        &self,
        route_id: &str,
        error: String,
        cooldown_secs: i64,
        latency_ms: u64,
        count_for_adaptive: bool,
    ) {
        {
            let mut health = self.health.write().await;
            let entry = health.entry(route_id.to_string()).or_default();
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            entry.last_error = Some(error);
            entry.cooldown_until = Some(Utc::now() + Duration::seconds(cooldown_secs));
        }
        if count_for_adaptive {
            self.adaptive
                .write()
                .await
                .entry(route_id.to_string())
                .or_default()
                .observe_failure(latency_ms, &self.config.routing);
        }
    }

    pub async fn snapshot(&self) -> HashMap<String, RouteHealth> {
        self.health.read().await.clone()
    }
}

struct RouteEvaluation {
    resolved_model: String,
    task: TaskProfile,
    candidates: Vec<EvaluatedRoute>,
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|item| item == value) {
        values.push(value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::push_unique;

    #[test]
    fn push_unique_deduplicates_trace_reasons() {
        let mut values = vec!["route_cooldown".to_string()];
        push_unique(&mut values, "route_cooldown");
        push_unique(&mut values, "quota_pressure");
        assert_eq!(values, vec!["route_cooldown", "quota_pressure"]);
    }
}
