use crate::config::RoutingConfig;
use serde::Serialize;

#[derive(Clone, Debug, Default)]
pub struct AdaptiveRouteState {
    pub success_count: u64,
    pub failure_count: u64,
    pub ewma_latency_ms: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AdaptiveRouteSnapshot {
    pub sample_count: u64,
    pub success_count: u64,
    pub failure_count: u64,
    pub success_rate: Option<f64>,
    pub ewma_latency_ms: Option<f64>,
    pub active: bool,
    pub penalty: i32,
}

impl AdaptiveRouteState {
    pub fn observe_success(&mut self, latency_ms: u64, config: &RoutingConfig) {
        self.success_count = self.success_count.saturating_add(1);
        self.observe_latency(latency_ms, config.adaptive_ewma_alpha);
    }

    pub fn observe_failure(&mut self, latency_ms: u64, config: &RoutingConfig) {
        self.failure_count = self.failure_count.saturating_add(1);
        self.observe_latency(latency_ms, config.adaptive_ewma_alpha);
    }

    pub fn snapshot(&self, config: &RoutingConfig) -> AdaptiveRouteSnapshot {
        let sample_count = self.success_count.saturating_add(self.failure_count);
        let success_rate = if sample_count == 0 {
            None
        } else {
            Some(self.success_count as f64 / sample_count as f64)
        };
        let active = config.adaptive_enabled
            && sample_count >= config.adaptive_min_samples
            && config.adaptive_max_penalty > 0;
        let penalty = if active {
            adaptive_penalty(self, config)
        } else {
            0
        };

        AdaptiveRouteSnapshot {
            sample_count,
            success_count: self.success_count,
            failure_count: self.failure_count,
            success_rate,
            ewma_latency_ms: self.ewma_latency_ms,
            active,
            penalty,
        }
    }

    fn observe_latency(&mut self, latency_ms: u64, alpha: f64) {
        let current = latency_ms as f64;
        self.ewma_latency_ms = Some(match self.ewma_latency_ms {
            Some(previous) => alpha * current + (1.0 - alpha) * previous,
            None => current,
        });
    }
}

fn adaptive_penalty(state: &AdaptiveRouteState, config: &RoutingConfig) -> i32 {
    let samples = state.success_count.saturating_add(state.failure_count);
    if samples == 0 || config.adaptive_max_penalty == 0 {
        return 0;
    }

    let failure_rate = state.failure_count as f64 / samples as f64;
    let failure_weight = config.adaptive_failure_weight;
    let latency_weight = 1.0 - failure_weight;

    let failure_component = failure_rate.clamp(0.0, 1.0) * failure_weight;

    // Latency only penalizes a route after it exceeds the configured target.
    // At 2x target or slower the latency component reaches its cap.
    let latency_component = state
        .ewma_latency_ms
        .map(|latency| {
            let target = config.adaptive_latency_target_ms as f64;
            ((latency / target) - 1.0).clamp(0.0, 1.0) * latency_weight
        })
        .unwrap_or(0.0);

    ((failure_component + latency_component) * config.adaptive_max_penalty as f64)
        .round()
        .clamp(0.0, config.adaptive_max_penalty as f64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RoutingConfig {
        RoutingConfig {
            adaptive_enabled: true,
            adaptive_min_samples: 3,
            adaptive_ewma_alpha: 0.5,
            adaptive_latency_target_ms: 100,
            adaptive_max_penalty: 30,
            adaptive_failure_weight: 0.7,
        }
    }

    #[test]
    fn cold_routes_are_neutral_until_minimum_samples() {
        let mut state = AdaptiveRouteState::default();
        state.observe_success(400, &config());
        state.observe_success(400, &config());

        let snapshot = state.snapshot(&config());
        assert!(!snapshot.active);
        assert_eq!(snapshot.penalty, 0);
        assert_eq!(snapshot.sample_count, 2);
    }

    #[test]
    fn slow_successful_route_gets_bounded_latency_penalty() {
        let mut state = AdaptiveRouteState::default();
        for _ in 0..3 {
            state.observe_success(250, &config());
        }

        let snapshot = state.snapshot(&config());
        assert!(snapshot.active);
        assert_eq!(snapshot.success_rate, Some(1.0));
        assert_eq!(snapshot.penalty, 9);
        assert!(snapshot.penalty <= config().adaptive_max_penalty);
    }

    #[test]
    fn failures_raise_penalty_and_ewma_is_updated() {
        let mut state = AdaptiveRouteState::default();
        state.observe_success(100, &config());
        state.observe_failure(300, &config());
        state.observe_failure(300, &config());

        let snapshot = state.snapshot(&config());
        assert!(snapshot.active);
        assert_eq!(snapshot.sample_count, 3);
        assert!(snapshot.success_rate.unwrap() < 0.5);
        assert!(snapshot.penalty > 9);
        assert_eq!(snapshot.ewma_latency_ms, Some(250.0));
    }

    #[test]
    fn adaptive_penalty_never_exceeds_configured_cap() {
        let mut state = AdaptiveRouteState::default();
        for _ in 0..10 {
            state.observe_failure(10_000, &config());
        }
        assert_eq!(state.snapshot(&config()).penalty, config().adaptive_max_penalty);
    }
}
