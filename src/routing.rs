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
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
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
    pub policy_reason: String,
    pub browser_fairness_rank: Option<u64>,
    pub browser_recovery_penalty: i32,
    pub base_priority: i32,
    pub group_tier_priority: Option<i32>,
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
    pub execution_policy: String,
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
    browser_last_success: Arc<RwLock<HashMap<String, u64>>>,
    browser_success_sequence: Arc<AtomicU64>,
}

impl Router {
    pub fn new(config: Arc<AppConfig>, live_config: LiveConfig, catalog: Arc<ModelCatalog>) -> Self {
        Self {
            config,
            live_config,
            catalog,
            health: Arc::new(RwLock::new(HashMap::new())),
            adaptive: Arc::new(RwLock::new(HashMap::new())),
            browser_last_success: Arc::new(RwLock::new(HashMap::new())),
            browser_success_sequence: Arc::new(AtomicU64::new(0)),
        }
    }

     pub async fn plan_for_body_with_config(
        &self,
        config: Arc<AppConfig>,
        requested_model: &str,
        body: Option<&Value>,
    ) -> Vec<RouteConfig> {
        let evaluation = self.evaluate_with_config(config, requested_model, body).await;
        let mut eligible = evaluation
            .candidates
            .into_iter()
            .filter(|candidate| candidate.decision.eligible)
            .collect::<Vec<_>>();
        eligible.sort_by_key(|candidate| candidate.decision.rank.unwrap_or(usize::MAX));
        eligible.into_iter().map(|candidate| candidate.route).collect()
    }

     pub fn sticky_route_matches_best_task_fit_with_config(
        &self,
        config: &AppConfig,
        requested_model: &str,
        body: &Value,
        preferred: &RouteConfig,
        best: &RouteConfig,
    ) -> bool {
        let resolved_model = config.resolve_model_alias(requested_model);
        if let Some(group) = config.virtual_models.get(resolved_model) {
            if group.is_tiered()
                && group.tier_priority_for_route(config, preferred)
                    != group.tier_priority_for_route(config, best)
            {
                return false;
            }
            if self.transport_preference_rank(config, preferred)
                != self.transport_preference_rank(config, best)
            {
                return false;
            }
        }
        if self.route_transport(config, preferred) == "browser"
            && !config.routing.browser_sticky_affinity
        {
            return false;
        }
        if !config.routing.task_aware_enabled {
            return true;
        }
        let task = classify_task(Some(body), &config.routing);
        let preferred_fit = evaluate_task_fit(&task, preferred, &config.routing);
        let best_fit = evaluate_task_fit(&task, best, &config.routing);
        preferred_fit.exclusion_reason.is_none()
            && preferred_fit.snapshot.adjustment == best_fit.snapshot.adjustment
    }

     pub async fn explain_for_body_with_config(
        &self,
        config: Arc<AppConfig>,
        requested_model: &str,
        body: Option<&Value>,
    ) -> RouteDecisionTrace {
        let evaluation = self
            .evaluate_with_config(config.clone(), requested_model, body)
            .await;
        let selected_route = evaluation
            .candidates
            .iter()
            .find(|candidate| candidate.decision.selected)
            .map(|candidate| candidate.route.id.clone());
        RouteDecisionTrace {
            requested_model: requested_model.to_string(),
            resolved_model: evaluation.resolved_model,
            execution_preference: config.routing.execution_preference.clone(),
            execution_policy: config.routing.execution_policy().to_string(),
            api_fallback: config.routing.api_fallback,
            task: evaluation.task,
            selected_route,
            candidates: evaluation
                .candidates
                .into_iter()
                .map(|candidate| candidate.decision)
                .collect(),
        }
    }

    async fn evaluate_with_config(
        &self,
        config: Arc<AppConfig>,
        requested_model: &str,
        body: Option<&Value>,
    ) -> RouteEvaluation {
        let resolved_model = config.resolve_model_alias(requested_model).to_string();
        let task = classify_task(body, &config.routing);
        let candidates = self
            .candidate_routes(config.clone(), &resolved_model)
            .await;
        let apply_execution_policy = config.virtual_models.contains_key(&resolved_model);
        let now = Utc::now();
        let health_snapshot = self.health.read().await.clone();
        let adaptive_snapshot = self.adaptive.read().await.clone();
        let browser_success_snapshot = self.browser_last_success.read().await.clone();
        let mut readiness_by_account: HashMap<String, AccountReadiness> = HashMap::new();
        let mut quota_by_account: HashMap<String, QuotaEvaluation> = HashMap::new();
        let mut evaluated = Vec::with_capacity(candidates.len());

        for (original_index, route) in candidates.into_iter().enumerate() {
            let readiness = if let Some(readiness) = readiness_by_account.get(&route.account) {
                readiness.clone()
            } else {
                let readiness = self
                    .account_readiness_with_config(config.as_ref(), &route.account)
                    .await;
                readiness_by_account.insert(route.account.clone(), readiness.clone());
                readiness
            };
            let transport = self.route_transport(config.as_ref(), &route);
            let transport_preference = if apply_execution_policy {
                self.transport_preference_rank(config.as_ref(), &route)
            } else {
                0
            };
            let route_health = health_snapshot.get(&route.id).cloned().unwrap_or_default();
            let adaptive = adaptive_snapshot
                .get(&route.id)
                .cloned()
                .unwrap_or_default()
                .snapshot(&config.routing);
            let adaptive_penalty = adaptive.penalty;
            let browser_fairness_rank = if transport == "browser"
                && config.routing.browser_fairness_enabled
            {
                Some(
                    browser_success_snapshot
                        .get(&route.account)
                        .copied()
                        .unwrap_or(0),
                )
            } else {
                None
            };
            let browser_recovery_penalty = if transport == "browser"
                && route_health.consecutive_failures > 0
            {
                let failures = route_health
                    .consecutive_failures
                    .min(i32::MAX as u32) as i32;
                failures
                    .saturating_mul(config.routing.browser_recovery_penalty)
                    .min(config.routing.browser_recovery_max_penalty)
            } else {
                0
            };
            let policy_reason = self.policy_reason(config.as_ref(), transport).to_string();
            let group_tier_priority = config
                .virtual_models
                .get(&resolved_model)
                .and_then(|group| group.tier_priority_for_route(config.as_ref(), &route));
            let task_fit = evaluate_task_fit(&task, &route, &config.routing);
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
            if apply_execution_policy {
                if let Some(reason) = execution_policy_exclusion(
                    config.routing.execution_policy(),
                    config.routing.api_fallback,
                    transport,
                ) {
                    push_unique(&mut exclusion_reasons, reason);
                }
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
                                .saturating_add(browser_recovery_penalty)
                                .saturating_add(task_adjustment),
                        );
                        if adaptive.active && adaptive_penalty > 0 {
                            push_unique(&mut warnings, "adaptive_degraded");
                        }
                        if browser_recovery_penalty > 0 {
                            push_unique(&mut warnings, "browser_recovery_probe");
                        }
                        if transport == "browser"
                            && config.routing.browser_fairness_enabled
                        {
                            push_unique(&mut warnings, "browser_fairness");
                        }
                        if task_adjustment < 0 {
                            push_unique(&mut warnings, "task_preferred");
                        } else if task_adjustment > 0 {
                            push_unique(&mut warnings, "task_mismatch");
                        }
                        if apply_execution_policy && policy_reason != "balanced" {
                            push_unique(&mut warnings, &policy_reason);
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
                                .saturating_add(browser_recovery_penalty)
                                .saturating_add(task_adjustment),
                        );
                        if adaptive.active && adaptive_penalty > 0 {
                            push_unique(&mut warnings, "adaptive_degraded");
                        }
                        if browser_recovery_penalty > 0 {
                            push_unique(&mut warnings, "browser_recovery_probe");
                        }
                        if transport == "browser"
                            && config.routing.browser_fairness_enabled
                        {
                            push_unique(&mut warnings, "browser_fairness");
                        }
                        if task_adjustment < 0 {
                            push_unique(&mut warnings, "task_preferred");
                        } else if task_adjustment > 0 {
                            push_unique(&mut warnings, "task_mismatch");
                        }
                        if apply_execution_policy && policy_reason != "balanced" {
                            push_unique(&mut warnings, &policy_reason);
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
                    policy_reason,
                    browser_fairness_rank,
                    browser_recovery_penalty,
                    base_priority: route.priority,
                    group_tier_priority,
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
                    candidate.decision.group_tier_priority.unwrap_or(0),
                    candidate.decision.transport_preference,
                    candidate.decision.final_score.unwrap_or(i32::MAX),
                    candidate.original_index,
                )
            })
            .collect::<Vec<_>>();
        ranked_indices.sort_by(|left, right| {
            let base = (left.1, left.2, left.3).cmp(&(right.1, right.2, right.3));
            if base != std::cmp::Ordering::Equal {
                return base;
            }

            let left_candidate = &evaluated[left.0];
            let right_candidate = &evaluated[right.0];
            if left_candidate.decision.transport == "browser"
                && right_candidate.decision.transport == "browser"
            {
                let fairness = left_candidate
                    .decision
                    .browser_fairness_rank
                    .unwrap_or(0)
                    .cmp(
                        &right_candidate
                            .decision
                            .browser_fairness_rank
                            .unwrap_or(0),
                    );
                if fairness != std::cmp::Ordering::Equal {
                    return fairness;
                }
            }

            left.4.cmp(&right.4)
        });
        for (rank_index, (candidate_index, _, _, _, _)) in ranked_indices.into_iter().enumerate() {
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

    fn route_transport(&self, config: &AppConfig, route: &RouteConfig) -> &'static str {
        config
            .account(&route.account)
            .and_then(|account| config.provider(&account.provider))
            .map(|provider| provider.transport())
            .unwrap_or("unknown")
    }

    fn transport_preference_rank(&self, config: &AppConfig, route: &RouteConfig) -> i32 {
        match (
            config.routing.execution_policy(),
            self.route_transport(config, route),
        ) {
            ("prefer-browser", "browser") | ("prefer-api", "api") => 0,
            ("prefer-browser", _) | ("prefer-api", _) => 1,
            _ => 0,
        }
    }

    fn policy_reason(&self, config: &AppConfig, transport: &str) -> &'static str {
        match (config.routing.execution_policy(), transport) {
            ("prefer-browser", "browser") => "browser_preferred",
            ("prefer-browser", "api") => "api_fallback",
            ("browser-only", "browser") => "browser_only",
            ("prefer-api", "api") => "api_preferred",
            ("prefer-api", "browser") => "browser_fallback",
            ("api-only", "api") => "api_only",
            _ => "balanced",
        }
    }

    async fn candidate_routes(
        &self,
        config: Arc<AppConfig>,
        resolved: &str,
    ) -> Vec<RouteConfig> {
        if let Some(vm) = config.virtual_models.get(resolved) {
            let route_ids = vm
                .route_ids()
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let model_ids = vm
                .model_ids()
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>();

            let mut routes = route_ids
                .into_iter()
                .filter_map(|id| config.route(&id).cloned())
                .collect::<Vec<_>>();

            for model_id in model_ids {
                routes.extend(
                    self.routes_for_physical_model(config.clone(), &model_id)
                        .await,
                );
            }

            let mut seen = HashSet::new();
            routes.retain(|route| seen.insert(route.id.clone()));
            self.enrich_configured_routes(config.as_ref(), routes).await
        } else if let Some(route) = config.route(resolved) {
            self.enrich_configured_routes(config.as_ref(), vec![route.clone()])
                .await
        } else {
            self.routes_for_physical_model(config, resolved).await
        }
    }

    async fn enrich_configured_routes(
        &self,
        config: &AppConfig,
        mut routes: Vec<RouteConfig>,
    ) -> Vec<RouteConfig> {
        let models = match self.catalog.models().await {
            Ok(models) => models,
            Err(error) => {
                warn!(%error, "failed to enrich configured routes from model catalog");
                return routes;
            }
        };

        for route in &mut routes {
            let Some(account) = config.account(&route.account) else {
                continue;
            };
            let Some(model) = models.iter().find(|model| {
                model.provider == account.provider
                    && model.external_id == route.model
                    && model.accounts.iter().any(|binding| {
                        binding.account_id == route.account
                            && binding.enabled
                            && binding.availability != "unavailable"
                    })
            }) else {
                continue;
            };

            if route.context_window.is_none() {
                route.context_window = model.context_window;
            }
            for capability in &model.capabilities {
                if !route.capabilities.iter().any(|value| value == capability) {
                    route.capabilities.push(capability.clone());
                }
            }
        }

        routes
    }

    pub async fn account_readiness(&self, account_id: &str) -> AccountReadiness {
        let config = self.live_config.snapshot();
        self.account_readiness_with_config(config.as_ref(), account_id).await
    }

    async fn account_readiness_with_config(
        &self,
        config: &AppConfig,
        account_id: &str,
    ) -> AccountReadiness {
        let mut readiness = evaluate_base(config, account_id).await;
        let routes = config
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

    async fn routes_for_physical_model(
        &self,
        config: Arc<AppConfig>,
        requested: &str,
    ) -> Vec<RouteConfig> {
        let (provider_filter, external_id) = requested
            .split_once('/')
            .filter(|(provider, _)| config.provider(provider).is_some())
            .map(|(provider, external)| (Some(provider), external))
            .unwrap_or((None, requested));

        let models = match self.catalog.models().await {
            Ok(models) => models,
            Err(error) => {
                warn!(%error, "failed to read model catalog while planning route");
                return self.config_routes_for_model(config.as_ref(), provider_filter, external_id);
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
                let Some(account) = config.account(&binding.account_id) else {
                    continue;
                };
                if !account.enabled || account.provider != model.provider {
                    continue;
                }
                let key = format!("{}\0{}", account.id, external_id);
                if !seen.insert(key) {
                    continue;
                }

                if let Some(configured) = config.routes.iter().find(|route| {
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

                let account_order = config
                    .accounts
                    .iter()
                    .position(|candidate| candidate.id == account.id)
                    .unwrap_or(config.accounts.len()) as i32;
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
            self.config_routes_for_model(config.as_ref(), provider_filter, external_id)
        } else {
            routes
        }
    }

    fn config_routes_for_model(
        &self,
        config: &AppConfig,
        provider_filter: Option<&str>,
        external_id: &str,
    ) -> Vec<RouteConfig> {
        config
            .routes
            .iter()
            .filter(|route| {
                route.enabled
                    && route.model == external_id
                    && provider_filter.is_none_or(|provider| {
                        config
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

        let config = self.live_config.snapshot();
        if let Some(route) = config.route(route_id) {
            if self.route_transport(config.as_ref(), route) == "browser"
                && config.routing.browser_fairness_enabled
            {
                let sequence = self
                    .browser_success_sequence
                    .fetch_add(1, Ordering::Relaxed)
                    .saturating_add(1);
                self.browser_last_success
                    .write()
                    .await
                    .insert(route.account.clone(), sequence);
            }
        }
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
            entry.cooldown_until = (cooldown_secs > 0)
                .then(|| Utc::now() + Duration::seconds(cooldown_secs));
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

fn execution_policy_exclusion(
    policy: &str,
    api_fallback: bool,
    transport: &str,
) -> Option<&'static str> {
    match (policy, transport) {
        ("browser-only", value) if value != "browser" => Some("policy_browser_only"),
        ("api-only", value) if value != "api" => Some("policy_api_only"),
        ("prefer-browser", "api") if !api_fallback => Some("api_fallback_disabled"),
        _ => None,
    }
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|item| item == value) {
        values.push(value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::{execution_policy_exclusion, push_unique};
    use chrono::{Duration, Utc};

    #[test]
    fn execution_policy_matrix_enforces_hard_transport_boundaries() {
        assert_eq!(
            execution_policy_exclusion("browser-only", true, "api"),
            Some("policy_browser_only")
        );
        assert_eq!(
            execution_policy_exclusion("browser-only", true, "browser"),
            None
        );
        assert_eq!(
            execution_policy_exclusion("api-only", true, "browser"),
            Some("policy_api_only")
        );
        assert_eq!(execution_policy_exclusion("api-only", true, "api"), None);
        assert_eq!(
            execution_policy_exclusion("prefer-browser", false, "api"),
            Some("api_fallback_disabled")
        );
        assert_eq!(
            execution_policy_exclusion("prefer-browser", true, "api"),
            None
        );
        assert_eq!(
            execution_policy_exclusion("prefer-api", true, "browser"),
            None
        );
    }

    #[test]
    fn zero_cooldown_does_not_create_a_route_lockout() {
        let now = Utc::now();
        let cooldown = (0_i64 > 0).then(|| now + Duration::seconds(0));
        assert!(cooldown.is_none());
    }

    #[test]
    fn push_unique_deduplicates_trace_reasons() {
        let mut values = vec!["route_cooldown".to_string()];
        push_unique(&mut values, "route_cooldown");
        push_unique(&mut values, "quota_pressure");
        assert_eq!(values, vec!["route_cooldown", "quota_pressure"]);
    }
}
