use serde::Serialize;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    AuthExpired,
    AuthIncomplete,
    HumanActionRequired,
    RateLimited,
    UpstreamOverloaded,
    WafRejected,
    CdpDisconnected,
    PageTargetLost,
    BrowserCrashed,
    SessionBusy,
    SessionStateDesync,
    StreamDropped,
    StreamEmpty,
    ModelUnavailable,
    ModelRecipeStale,
    NetworkTransient,
    Upstream5xx,
    Unknown,
}

impl FailureClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuthExpired => "auth_expired",
            Self::AuthIncomplete => "auth_incomplete",
            Self::HumanActionRequired => "human_action_required",
            Self::RateLimited => "rate_limited",
            Self::UpstreamOverloaded => "upstream_overloaded",
            Self::WafRejected => "waf_rejected",
            Self::CdpDisconnected => "cdp_disconnected",
            Self::PageTargetLost => "page_target_lost",
            Self::BrowserCrashed => "browser_crashed",
            Self::SessionBusy => "session_busy",
            Self::SessionStateDesync => "session_state_desync",
            Self::StreamDropped => "stream_dropped",
            Self::StreamEmpty => "stream_empty",
            Self::ModelUnavailable => "model_unavailable",
            Self::ModelRecipeStale => "model_recipe_stale",
            Self::NetworkTransient => "network_transient",
            Self::Upstream5xx => "upstream_5xx",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPhase {
    PreSubmit,
    Submitted,
    Committed,
    Terminal,
}

impl ExecutionPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreSubmit => "pre_submit",
            Self::Submitted => "submitted",
            Self::Committed => "committed",
            Self::Terminal => "terminal",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaySafety {
    Safe,
    ProbablySafe,
    Unsafe,
}

impl ReplaySafety {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::ProbablySafe => "probably_safe",
            Self::Unsafe => "unsafe",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureScope {
    Request,
    Conversation,
    Session,
    Transport,
    Account,
    Model,
    Provider,
}

impl FailureScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Conversation => "conversation",
            Self::Session => "session",
            Self::Transport => "transport",
            Self::Account => "account",
            Self::Model => "model",
            Self::Provider => "provider",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionFailure {
    pub class: FailureClass,
    pub provider: Option<String>,
    pub account_id: Option<String>,
    pub model: Option<String>,
    pub transport: Option<String>,
    pub conversation_id: Option<String>,
    pub retryable: bool,
    pub replay_safety: ReplaySafety,
    pub phase: ExecutionPhase,
    pub scope: FailureScope,
    pub suggested_cooldown_secs: Option<i64>,
    pub human_action_required: bool,
    pub diagnostic: String,
}

impl ExecutionFailure {
    pub fn new(
        class: FailureClass,
        retryable: bool,
        replay_safety: ReplaySafety,
        phase: ExecutionPhase,
        scope: FailureScope,
        diagnostic: impl Into<String>,
    ) -> Self {
        Self {
            class,
            provider: None,
            account_id: None,
            model: None,
            transport: None,
            conversation_id: None,
            retryable,
            replay_safety,
            phase,
            scope,
            suggested_cooldown_secs: None,
            human_action_required: false,
            diagnostic: diagnostic.into(),
        }
    }

    pub fn with_context(
        mut self,
        provider: &str,
        account_id: &str,
        model: &str,
        transport: &str,
    ) -> Self {
        self.provider = Some(provider.to_string());
        self.account_id = Some(account_id.to_string());
        self.model = Some(model.to_string());
        self.transport = Some(transport.to_string());
        self
    }

    pub fn with_cooldown(mut self, seconds: i64) -> Self {
        self.suggested_cooldown_secs = Some(seconds.max(0));
        self
    }

    pub fn requiring_human_action(mut self) -> Self {
        self.human_action_required = true;
        self
    }

    pub fn allows_silent_fallback(&self, committed: bool) -> bool {
        self.retryable && !committed && self.replay_safety != ReplaySafety::Unsafe
    }
}

#[derive(Clone, Debug)]
pub struct ExecutionBudget {
    pub overall_deadline: Duration,
    pub max_attempts: usize,
    pub max_transport_switches: usize,
    pub max_account_switches: usize,
    pub max_provider_switches: usize,
    pub max_queue_wait: Duration,
}

impl ExecutionBudget {
    pub fn for_candidate_count(candidate_count: usize) -> Self {
        let max_attempts = candidate_count.max(1);
        let max_switches = max_attempts.saturating_sub(1);
        Self {
            overall_deadline: Duration::from_secs(600),
            max_attempts,
            max_transport_switches: max_switches,
            max_account_switches: max_switches,
            max_provider_switches: max_switches,
            max_queue_wait: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExecutionBudgetTracker {
    budget: ExecutionBudget,
    started_at: Instant,
    attempts: usize,
    transport_switches: usize,
    account_switches: usize,
    provider_switches: usize,
}

impl ExecutionBudgetTracker {
    pub fn new(budget: ExecutionBudget) -> Self {
        Self {
            budget,
            started_at: Instant::now(),
            attempts: 0,
            transport_switches: 0,
            account_switches: 0,
            provider_switches: 0,
        }
    }

    pub fn try_begin_attempt(&mut self) -> bool {
        if self.started_at.elapsed() >= self.budget.overall_deadline
            || self.attempts >= self.budget.max_attempts
        {
            return false;
        }
        self.attempts += 1;
        true
    }

    pub fn record_transport_switch(&mut self) -> bool {
        if self.transport_switches >= self.budget.max_transport_switches {
            return false;
        }
        self.transport_switches += 1;
        true
    }

    pub fn record_account_switch(&mut self) -> bool {
        if self.account_switches >= self.budget.max_account_switches {
            return false;
        }
        self.account_switches += 1;
        true
    }

    pub fn record_provider_switch(&mut self) -> bool {
        if self.provider_switches >= self.budget.max_provider_switches {
            return false;
        }
        self.provider_switches += 1;
        true
    }

    pub fn queue_wait_allowed(&self, waited: Duration) -> bool {
        waited <= self.budget.max_queue_wait && self.started_at.elapsed() < self.budget.overall_deadline
    }

    pub fn attempts(&self) -> usize {
        self.attempts
    }
}

#[derive(Clone, Debug)]
pub struct StreamCommitBarrier {
    phase: ExecutionPhase,
    committed: bool,
}

impl StreamCommitBarrier {
    pub fn new() -> Self {
        Self {
            phase: ExecutionPhase::Submitted,
            committed: false,
        }
    }

    pub fn commit_client_visible(&mut self) {
        if self.phase != ExecutionPhase::Terminal {
            self.committed = true;
            self.phase = ExecutionPhase::Committed;
        }
    }

    pub fn finish(&mut self) {
        self.phase = ExecutionPhase::Terminal;
    }

    pub fn cancel(&mut self) {
        self.phase = ExecutionPhase::Terminal;
    }

    pub const fn phase(&self) -> ExecutionPhase {
        self.phase
    }

    pub const fn committed(&self) -> bool {
        self.committed
    }

    pub const fn replay_safety(&self) -> ReplaySafety {
        if self.committed {
            ReplaySafety::Unsafe
        } else {
            ReplaySafety::ProbablySafe
        }
    }

    pub fn allows_silent_fallback(&self, replay_safety: ReplaySafety) -> bool {
        !self.committed && replay_safety != ReplaySafety::Unsafe
    }
}

impl Default for StreamCommitBarrier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExecutionBudget, ExecutionBudgetTracker, ExecutionFailure, ExecutionPhase, FailureClass,
        FailureScope, ReplaySafety, StreamCommitBarrier,
    };
    use std::time::Duration;

    #[test]
    fn typed_failure_contract_carries_replay_and_scope_metadata() {
        let failure = ExecutionFailure::new(
            FailureClass::WafRejected,
            true,
            ReplaySafety::Safe,
            ExecutionPhase::Submitted,
            FailureScope::Transport,
            "upstream rejected the selected transport",
        )
        .with_context("qwen-web", "account-a", "qwen-max", "direct-http")
        .with_cooldown(60);

        assert_eq!(failure.class, FailureClass::WafRejected);
        assert!(failure.retryable);
        assert_eq!(failure.replay_safety, ReplaySafety::Safe);
        assert_eq!(failure.scope, FailureScope::Transport);
        assert_eq!(failure.suggested_cooldown_secs, Some(60));
        assert!(!failure.human_action_required);
    }

    #[test]
    fn safe_pre_commit_failure_allows_fallback() {
        let barrier = StreamCommitBarrier::new();
        assert!(barrier.allows_silent_fallback(ReplaySafety::Safe));
        assert!(barrier.allows_silent_fallback(ReplaySafety::ProbablySafe));
    }

    #[test]
    fn committed_partial_output_blocks_silent_fallback() {
        let mut barrier = StreamCommitBarrier::new();
        barrier.commit_client_visible();
        assert!(barrier.committed());
        assert_eq!(barrier.phase(), ExecutionPhase::Committed);
        assert_eq!(barrier.replay_safety(), ReplaySafety::Unsafe);
        assert!(!barrier.allows_silent_fallback(ReplaySafety::Safe));
    }

    #[test]
    fn cancellation_after_commit_never_reenables_replay() {
        let mut barrier = StreamCommitBarrier::new();
        barrier.commit_client_visible();
        barrier.cancel();
        assert_eq!(barrier.phase(), ExecutionPhase::Terminal);
        assert!(barrier.committed());
        assert_eq!(barrier.replay_safety(), ReplaySafety::Unsafe);
        assert!(!barrier.allows_silent_fallback(ReplaySafety::Safe));
    }

    #[test]
    fn execution_budget_exhaustion_is_bounded() {
        let mut budget = ExecutionBudget::for_candidate_count(2);
        budget.overall_deadline = Duration::from_secs(60);
        let mut tracker = ExecutionBudgetTracker::new(budget);
        assert!(tracker.try_begin_attempt());
        assert!(tracker.try_begin_attempt());
        assert!(!tracker.try_begin_attempt());
        assert_eq!(tracker.attempts(), 2);
    }
}
