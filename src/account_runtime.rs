use crate::execution::{
    ExecutionFailure, ExecutionPhase, FailureClass, FailureScope, ReplaySafety,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::sync::{Mutex, Notify};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionState {
    Open,
    Draining,
    Stopped,
}

impl AdmissionState {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Draining => 1,
            Self::Stopped => 2,
        }
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Draining,
            2 => Self::Stopped,
            _ => Self::Open,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BurstBehavior {
    Serialized,
    Bounded,
    RateSensitive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderRuntimePolicy {
    pub safe_concurrency: usize,
    pub max_in_flight: usize,
    pub max_queue_depth: usize,
    pub max_queue_wait: Duration,
    pub recovery_successes_per_step: usize,
    pub burst_behavior: BurstBehavior,
}

impl ProviderRuntimePolicy {
    pub fn api_default() -> Self {
        Self {
            safe_concurrency: 4,
            max_in_flight: 8,
            max_queue_depth: 64,
            max_queue_wait: Duration::from_secs(30),
            recovery_successes_per_step: 4,
            burst_behavior: BurstBehavior::Bounded,
        }
    }

    pub fn browserless_preferred() -> Self {
        Self {
            // Web-backed provider sessions are conservatively serialized until a
            // provider explicitly proves a higher safe concurrency contract.
            safe_concurrency: 1,
            max_in_flight: 1,
            max_queue_depth: 32,
            max_queue_wait: Duration::from_secs(30),
            recovery_successes_per_step: 4,
            burst_behavior: BurstBehavior::RateSensitive,
        }
    }

    pub fn serialized_browser() -> Self {
        Self {
            safe_concurrency: 1,
            max_in_flight: 1,
            max_queue_depth: 16,
            max_queue_wait: Duration::from_secs(30),
            recovery_successes_per_step: 4,
            burst_behavior: BurstBehavior::Serialized,
        }
    }

    fn normalized(self) -> Self {
        let max_in_flight = self.max_in_flight.max(1);
        Self {
            safe_concurrency: self.safe_concurrency.max(1).min(max_in_flight),
            max_in_flight,
            max_queue_depth: self.max_queue_depth,
            max_queue_wait: self.max_queue_wait,
            recovery_successes_per_step: self.recovery_successes_per_step.max(1),
            burst_behavior: self.burst_behavior,
        }
    }
}

impl Default for ProviderRuntimePolicy {
    fn default() -> Self {
        Self::api_default()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct AccountRuntimeKey {
    pub provider: String,
    pub account_id: String,
}

impl AccountRuntimeKey {
    fn new(provider: &str, account_id: &str) -> Self {
        Self {
            provider: provider.to_string(),
            account_id: account_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountRuntimeSnapshot {
    pub key: AccountRuntimeKey,
    pub admission_state: AdmissionState,
    pub in_flight: usize,
    pub queue_depth: usize,
    pub concurrency_limit: usize,
    pub max_in_flight: usize,
    pub max_queue_depth: usize,
    pub max_queue_wait_ms: u64,
    pub burst_behavior: BurstBehavior,
    pub generation: u64,
    pub last_queue_wait_ms: u64,
    pub rejection_count: u64,
    pub last_rejection_reason: Option<String>,
    pub overload_events: u64,
    pub recovery_successes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionRejection {
    QueueFull,
    QueueTimeout,
    RuntimeUnavailable,
}

impl AdmissionRejection {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::QueueFull => "queue_full",
            Self::QueueTimeout => "queue_timeout",
            Self::RuntimeUnavailable => "runtime_unavailable",
        }
    }
}

#[derive(Clone, Debug, Error)]
#[error("account runtime admission rejected: {reason:?}")]
pub struct AccountAdmissionError {
    pub reason: AdmissionRejection,
    pub waited: Duration,
    pub snapshot: AccountRuntimeSnapshot,
}

impl AccountAdmissionError {
    pub fn execution_failure(
        &self,
        provider: &str,
        account_id: &str,
        model: &str,
        transport: &str,
    ) -> ExecutionFailure {
        let class = match self.reason {
            AdmissionRejection::QueueFull => FailureClass::QueueOverflow,
            AdmissionRejection::QueueTimeout => FailureClass::QueueTimeout,
            AdmissionRejection::RuntimeUnavailable => FailureClass::AdmissionRejected,
        };
        ExecutionFailure::new(
            class,
            true,
            ReplaySafety::Safe,
            ExecutionPhase::PreSubmit,
            FailureScope::Account,
            format!(
                "account runtime admission rejected: {} after {} ms",
                self.reason.as_str(),
                self.waited.as_millis()
            ),
        )
        .with_context(provider, account_id, model, transport)
        .with_cooldown(0)
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AccountLifecycleError {
    #[error("account runtime is not accepting lifecycle work")]
    Unavailable,
    #[error("account runtime lifecycle result belongs to stale generation {observed}; current generation is {current}")]
    StaleGeneration { observed: u64, current: u64 },
}

pub struct AccountAdmissionPermit {
    runtime: Arc<AccountRuntime>,
}

impl AccountAdmissionPermit {
    pub fn observe_success(&self) {
        self.runtime.observe_success();
    }

    pub fn observe_failure(&self, class: FailureClass) {
        self.runtime.observe_failure(class);
    }
}

impl Drop for AccountAdmissionPermit {
    fn drop(&mut self) {
        let previous = self.runtime.in_flight.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
        self.runtime.notify.notify_one();
    }
}

struct QueueGuard {
    runtime: Arc<AccountRuntime>,
    active: bool,
}

impl QueueGuard {
    fn new(runtime: Arc<AccountRuntime>) -> Self {
        Self {
            runtime,
            active: true,
        }
    }
}

impl Drop for QueueGuard {
    fn drop(&mut self) {
        if self.active {
            let previous = self.runtime.queue_depth.fetch_sub(1, Ordering::AcqRel);
            debug_assert!(previous > 0);
        }
    }
}

struct AccountRuntime {
    key: AccountRuntimeKey,
    policy: RwLock<ProviderRuntimePolicy>,
    admission_state: AtomicU8,
    generation: AtomicU64,
    in_flight: AtomicUsize,
    queue_depth: AtomicUsize,
    effective_limit: AtomicUsize,
    last_queue_wait_ms: AtomicU64,
    rejection_count: AtomicU64,
    last_rejection_reason: RwLock<Option<String>>,
    overload_events: AtomicU64,
    recovery_successes: AtomicUsize,
    notify: Notify,
    lifecycle: Mutex<()>,
    lifecycle_epoch: AtomicU64,
    lifecycle_result: AtomicU8,
}

impl AccountRuntime {
    fn new(key: AccountRuntimeKey, policy: ProviderRuntimePolicy) -> Self {
        let policy = policy.normalized();
        Self {
            key,
            policy: RwLock::new(policy),
            admission_state: AtomicU8::new(AdmissionState::Open.as_u8()),
            generation: AtomicU64::new(1),
            in_flight: AtomicUsize::new(0),
            queue_depth: AtomicUsize::new(0),
            effective_limit: AtomicUsize::new(policy.safe_concurrency),
            last_queue_wait_ms: AtomicU64::new(0),
            rejection_count: AtomicU64::new(0),
            last_rejection_reason: RwLock::new(None),
            overload_events: AtomicU64::new(0),
            recovery_successes: AtomicUsize::new(0),
            notify: Notify::new(),
            lifecycle: Mutex::new(()),
            lifecycle_epoch: AtomicU64::new(0),
            lifecycle_result: AtomicU8::new(0),
        }
    }

    fn policy(&self) -> ProviderRuntimePolicy {
        *self
            .policy
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reconfigure(&self, policy: ProviderRuntimePolicy) {
        let policy = policy.normalized();
        {
            let mut guard = self
                .policy
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = policy;
        }
        let current = self.effective_limit.load(Ordering::Acquire);
        let next = current.max(1).min(policy.max_in_flight);
        self.effective_limit.store(next, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn state(&self) -> AdmissionState {
        AdmissionState::from_u8(self.admission_state.load(Ordering::Acquire))
    }

    fn invalidate_lifecycle(&self) {
        self.lifecycle_result.store(1, Ordering::Release);
        self.lifecycle_epoch.fetch_add(1, Ordering::AcqRel);
        self.notify.notify_waiters();
    }

    fn activate(&self) {
        let previous = self
            .admission_state
            .swap(AdmissionState::Open.as_u8(), Ordering::AcqRel);
        if AdmissionState::from_u8(previous) != AdmissionState::Open {
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.invalidate_lifecycle();
        }
    }

    fn stop(&self) {
        let previous = self
            .admission_state
            .swap(AdmissionState::Stopped.as_u8(), Ordering::AcqRel);
        if AdmissionState::from_u8(previous) != AdmissionState::Stopped {
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.invalidate_lifecycle();
        }
        self.notify.notify_waiters();
    }

    fn bump_generation(&self) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.invalidate_lifecycle();
        generation
    }

    fn snapshot(&self) -> AccountRuntimeSnapshot {
        let policy = self.policy();
        let last_rejection_reason = self
            .last_rejection_reason
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        AccountRuntimeSnapshot {
            key: self.key.clone(),
            admission_state: self.state(),
            in_flight: self.in_flight.load(Ordering::Acquire),
            queue_depth: self.queue_depth.load(Ordering::Acquire),
            concurrency_limit: self.effective_limit.load(Ordering::Acquire),
            max_in_flight: policy.max_in_flight,
            max_queue_depth: policy.max_queue_depth,
            max_queue_wait_ms: duration_millis_u64(policy.max_queue_wait),
            burst_behavior: policy.burst_behavior,
            generation: self.generation.load(Ordering::Acquire),
            last_queue_wait_ms: self.last_queue_wait_ms.load(Ordering::Acquire),
            rejection_count: self.rejection_count.load(Ordering::Acquire),
            last_rejection_reason,
            overload_events: self.overload_events.load(Ordering::Acquire),
            recovery_successes: self.recovery_successes.load(Ordering::Acquire),
        }
    }

    fn try_enter(
        self: &Arc<Self>,
        queue_wait: Duration,
    ) -> Result<Option<AccountAdmissionPermit>, AdmissionRejection> {
        if self.state() != AdmissionState::Open {
            return Err(AdmissionRejection::RuntimeUnavailable);
        }
        let generation = self.generation.load(Ordering::Acquire);
        loop {
            let limit = self.effective_limit.load(Ordering::Acquire).max(1);
            let current = self.in_flight.load(Ordering::Acquire);
            if current >= limit {
                return Ok(None);
            }
            if self
                .in_flight
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            if self.state() == AdmissionState::Open
                && self.generation.load(Ordering::Acquire) == generation
            {
                self.last_queue_wait_ms
                    .store(duration_millis_u64(queue_wait), Ordering::Release);
                return Ok(Some(AccountAdmissionPermit {
                    runtime: self.clone(),
                }));
            }
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            self.notify.notify_one();
            return Err(AdmissionRejection::RuntimeUnavailable);
        }
    }

    fn enqueue(self: &Arc<Self>) -> Result<QueueGuard, AdmissionRejection> {
        let max_queue_depth = self.policy().max_queue_depth;
        loop {
            let current = self.queue_depth.load(Ordering::Acquire);
            if current >= max_queue_depth {
                return Err(AdmissionRejection::QueueFull);
            }
            if self
                .queue_depth
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(QueueGuard::new(self.clone()));
            }
        }
    }

    async fn admit(
        self: &Arc<Self>,
        requested_max_queue_wait: Duration,
        deadline_remaining: Duration,
    ) -> Result<AccountAdmissionPermit, AccountAdmissionError> {
        let started = Instant::now();
        match self.try_enter(Duration::ZERO) {
            Ok(Some(permit)) => return Ok(permit),
            Err(reason) => return Err(self.reject(reason, started.elapsed())),
            Ok(None) => {}
        }

        let queue_guard = match self.enqueue() {
            Ok(guard) => guard,
            Err(reason) => return Err(self.reject(reason, started.elapsed())),
        };
        let policy = self.policy();
        let allowed = requested_max_queue_wait
            .min(policy.max_queue_wait)
            .min(deadline_remaining);
        if allowed.is_zero() {
            drop(queue_guard);
            return Err(self.reject(AdmissionRejection::QueueTimeout, started.elapsed()));
        }

        let wait_result = tokio::time::timeout(allowed, async {
            loop {
                let notified = self.notify.notified();
                match self.try_enter(started.elapsed()) {
                    Ok(Some(permit)) => return Ok(permit),
                    Err(reason) => return Err(reason),
                    Ok(None) => {}
                }
                notified.await;
            }
        })
        .await;
        drop(queue_guard);

        match wait_result {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(reason)) => Err(self.reject(reason, started.elapsed())),
            Err(_) => Err(self.reject(AdmissionRejection::QueueTimeout, started.elapsed())),
        }
    }

    fn reject(&self, reason: AdmissionRejection, waited: Duration) -> AccountAdmissionError {
        self.last_queue_wait_ms
            .store(duration_millis_u64(waited), Ordering::Release);
        self.rejection_count.fetch_add(1, Ordering::AcqRel);
        {
            let mut guard = self
                .last_rejection_reason
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = Some(reason.as_str().to_string());
        }
        AccountAdmissionError {
            reason,
            waited,
            snapshot: self.snapshot(),
        }
    }

    fn observe_failure(&self, class: FailureClass) {
        if !matches!(
            class,
            FailureClass::RateLimited | FailureClass::UpstreamOverloaded
        ) {
            return;
        }
        self.overload_events.fetch_add(1, Ordering::AcqRel);
        self.recovery_successes.store(0, Ordering::Release);
        loop {
            let current = self.effective_limit.load(Ordering::Acquire).max(1);
            let next = (current / 2).max(1);
            if self
                .effective_limit
                .compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }
        self.notify.notify_waiters();
    }

    fn observe_success(&self) {
        let policy = self.policy();
        let current = self.effective_limit.load(Ordering::Acquire);
        if current >= policy.max_in_flight {
            self.recovery_successes.store(0, Ordering::Release);
            return;
        }

        let successes = self.recovery_successes.fetch_add(1, Ordering::AcqRel) + 1;
        if successes < policy.recovery_successes_per_step {
            return;
        }
        self.recovery_successes.store(0, Ordering::Release);
        loop {
            let current = self.effective_limit.load(Ordering::Acquire);
            if current >= policy.max_in_flight {
                break;
            }
            if self
                .effective_limit
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.notify.notify_waiters();
                break;
            }
        }
    }
}

#[derive(Default)]
pub struct AccountRuntimeRegistry {
    runtimes: RwLock<HashMap<AccountRuntimeKey, Arc<AccountRuntime>>>,
}

impl AccountRuntimeRegistry {
    fn ensure_runtime(
        &self,
        provider: &str,
        account_id: &str,
        policy: ProviderRuntimePolicy,
    ) -> Arc<AccountRuntime> {
        let key = AccountRuntimeKey::new(provider, account_id);
        if let Some(runtime) = self
            .runtimes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned()
        {
            runtime.reconfigure(policy);
            return runtime;
        }

        let mut runtimes = self
            .runtimes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let runtime = runtimes
            .entry(key.clone())
            .or_insert_with(|| Arc::new(AccountRuntime::new(key, policy)))
            .clone();
        runtime.reconfigure(policy);
        runtime
    }

    pub async fn acquire(
        &self,
        provider: &str,
        account_id: &str,
        policy: ProviderRuntimePolicy,
        max_queue_wait: Duration,
        deadline_remaining: Duration,
    ) -> Result<AccountAdmissionPermit, AccountAdmissionError> {
        self.ensure_runtime(provider, account_id, policy)
            .admit(max_queue_wait, deadline_remaining)
            .await
    }

    #[cfg(test)]
    pub fn snapshot(&self, provider: &str, account_id: &str) -> Option<AccountRuntimeSnapshot> {
        let key = AccountRuntimeKey::new(provider, account_id);
        self.runtimes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .map(|runtime| runtime.snapshot())
    }

    pub fn snapshot_or_create(
        &self,
        provider: &str,
        account_id: &str,
        policy: ProviderRuntimePolicy,
    ) -> AccountRuntimeSnapshot {
        self.ensure_runtime(provider, account_id, policy).snapshot()
    }

    pub fn bump_account_generation(&self, account_id: &str) {
        let runtimes = self
            .runtimes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (key, runtime) in runtimes.iter() {
            if key.account_id == account_id {
                runtime.bump_generation();
            }
        }
    }

    pub fn activate_account(&self, account_id: &str) {
        let runtimes = self
            .runtimes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (key, runtime) in runtimes.iter() {
            if key.account_id == account_id {
                runtime.activate();
            }
        }
    }

    pub fn stop_account(&self, account_id: &str) {
        let runtimes = self
            .runtimes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (key, runtime) in runtimes.iter() {
            if key.account_id == account_id {
                runtime.stop();
            }
        }
    }

    pub async fn single_flight_lifecycle<F, Fut>(
        &self,
        provider: &str,
        account_id: &str,
        policy: ProviderRuntimePolicy,
        operation: F,
    ) -> Result<bool, AccountLifecycleError>
    where
        F: FnOnce(u64) -> Fut,
        Fut: Future<Output = bool>,
    {
        let runtime = self.ensure_runtime(provider, account_id, policy);
        let observed_epoch = runtime.lifecycle_epoch.load(Ordering::Acquire);
        let _guard = runtime.lifecycle.lock().await;
        if runtime.state() != AdmissionState::Open {
            return Err(AccountLifecycleError::Unavailable);
        }

        let current_epoch = runtime.lifecycle_epoch.load(Ordering::Acquire);
        if current_epoch != observed_epoch {
            return Ok(runtime.lifecycle_result.load(Ordering::Acquire) == 2);
        }

        let generation = runtime.generation.load(Ordering::Acquire);
        let value = operation(generation).await;
        let current = runtime.generation.load(Ordering::Acquire);
        if current != generation || runtime.state() != AdmissionState::Open {
            runtime.lifecycle_result.store(1, Ordering::Release);
            runtime.lifecycle_epoch.fetch_add(1, Ordering::AcqRel);
            return Err(AccountLifecycleError::StaleGeneration {
                observed: generation,
                current,
            });
        }

        runtime
            .lifecycle_result
            .store(if value { 2 } else { 1 }, Ordering::Release);
        runtime.lifecycle_epoch.fetch_add(1, Ordering::AcqRel);
        Ok(value)
    }

    #[cfg(test)]
    fn runtime_count(&self) -> usize {
        self.runtimes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::{
        sync::{oneshot, Barrier},
        task::JoinHandle,
        time::{sleep, timeout},
    };

    fn policy(
        safe_concurrency: usize,
        max_in_flight: usize,
        max_queue_depth: usize,
    ) -> ProviderRuntimePolicy {
        ProviderRuntimePolicy {
            safe_concurrency,
            max_in_flight,
            max_queue_depth,
            max_queue_wait: Duration::from_secs(1),
            recovery_successes_per_step: 3,
            burst_behavior: BurstBehavior::Bounded,
        }
    }

    async fn wait_for_queue(
        registry: &AccountRuntimeRegistry,
        provider: &str,
        account: &str,
        depth: usize,
    ) {
        timeout(Duration::from_secs(1), async {
            loop {
                if registry
                    .snapshot(provider, account)
                    .is_some_and(|snapshot| snapshot.queue_depth == depth)
                {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn per_account_concurrency_limit_is_enforced_with_bounded_queue() {
        let registry = Arc::new(AccountRuntimeRegistry::default());
        let p = policy(2, 2, 2);
        let first = registry
            .acquire(
                "provider",
                "account-a",
                p,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        let second = registry
            .acquire(
                "provider",
                "account-a",
                p,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        let queued_registry = registry.clone();
        let queued: JoinHandle<AccountAdmissionPermit> = tokio::spawn(async move {
            queued_registry
                .acquire(
                    "provider",
                    "account-a",
                    p,
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .await
                .unwrap()
        });
        wait_for_queue(&registry, "provider", "account-a", 1).await;
        let snapshot = registry.snapshot("provider", "account-a").unwrap();
        assert_eq!(snapshot.in_flight, 2);
        assert_eq!(snapshot.queue_depth, 1);

        drop(first);
        let third = timeout(Duration::from_secs(1), queued)
            .await
            .unwrap()
            .unwrap();
        drop(second);
        drop(third);
        assert_eq!(
            registry
                .snapshot("provider", "account-a")
                .unwrap()
                .in_flight,
            0
        );
    }

    #[tokio::test]
    async fn queue_overflow_is_typed_and_does_not_burst() {
        let registry = Arc::new(AccountRuntimeRegistry::default());
        let p = policy(1, 1, 1);
        let first = registry
            .acquire(
                "provider",
                "account-a",
                p,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        let queued_registry = registry.clone();
        let queued = tokio::spawn(async move {
            queued_registry
                .acquire(
                    "provider",
                    "account-a",
                    p,
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .await
        });
        wait_for_queue(&registry, "provider", "account-a", 1).await;

        let overflow = match registry
            .acquire(
                "provider",
                "account-a",
                p,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("queue overflow unexpectedly admitted a request"),
        };
        assert_eq!(overflow.reason, AdmissionRejection::QueueFull);
        assert_eq!(
            overflow
                .execution_failure("provider", "account-a", "model", "transport")
                .class,
            FailureClass::QueueOverflow
        );

        queued.abort();
        let _ = queued.await;
        drop(first);
    }

    #[tokio::test]
    async fn queue_wait_respects_execution_deadline() {
        let registry = AccountRuntimeRegistry::default();
        let p = policy(1, 1, 2);
        let first = registry
            .acquire(
                "provider",
                "account-a",
                p,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        let rejected = match registry
            .acquire(
                "provider",
                "account-a",
                p,
                Duration::from_secs(1),
                Duration::from_millis(25),
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("deadline-limited queue wait unexpectedly admitted a request"),
        };
        assert_eq!(rejected.reason, AdmissionRejection::QueueTimeout);
        assert!(
            rejected.waited >= Duration::from_millis(20),
            "{:?}",
            rejected.waited
        );
        drop(first);
    }

    #[tokio::test]
    async fn forty_request_browserless_stress_is_bounded_by_admission() {
        let registry = Arc::new(AccountRuntimeRegistry::default());
        let policy = ProviderRuntimePolicy::browserless_preferred();
        let first = registry
            .acquire(
                "gemini",
                "account-a",
                policy,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        let barrier = Arc::new(Barrier::new(39));
        let mut tasks = Vec::new();
        for _ in 0..39 {
            let registry = registry.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                registry
                    .acquire(
                        "gemini",
                        "account-a",
                        policy,
                        Duration::from_secs(1),
                        Duration::from_secs(1),
                    )
                    .await
            }));
        }

        timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = registry.snapshot("gemini", "account-a").unwrap();
                if snapshot.queue_depth == policy.max_queue_depth
                    && snapshot.rejection_count >= 7
                {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        let snapshot = registry.snapshot("gemini", "account-a").unwrap();
        assert_eq!(snapshot.in_flight, 1);
        assert_eq!(snapshot.concurrency_limit, 1);
        assert_eq!(snapshot.queue_depth, policy.max_queue_depth);
        assert!(snapshot.rejection_count >= 7);

        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        timeout(Duration::from_secs(1), async {
            loop {
                if registry
                    .snapshot("gemini", "account-a")
                    .is_some_and(|snapshot| snapshot.queue_depth == 0)
                {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(first);
    }

    #[tokio::test]
    async fn overload_reduces_concurrency_and_recovery_is_hysteretic() {
        let registry = AccountRuntimeRegistry::default();
        let p = policy(4, 4, 4);
        let permit = registry
            .acquire(
                "provider",
                "account-a",
                p,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        permit.observe_failure(FailureClass::RateLimited);
        assert_eq!(
            registry
                .snapshot("provider", "account-a")
                .unwrap()
                .concurrency_limit,
            2
        );

        permit.observe_success();
        permit.observe_success();
        assert_eq!(
            registry
                .snapshot("provider", "account-a")
                .unwrap()
                .concurrency_limit,
            2
        );
        permit.observe_success();
        assert_eq!(
            registry
                .snapshot("provider", "account-a")
                .unwrap()
                .concurrency_limit,
            3
        );
        drop(permit);
    }

    #[tokio::test]
    async fn overload_is_isolated_between_accounts() {
        let registry = AccountRuntimeRegistry::default();
        let p = policy(4, 4, 4);
        let a = registry
            .acquire(
                "provider",
                "account-a",
                p,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        let b = registry
            .acquire(
                "provider",
                "account-b",
                p,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        a.observe_failure(FailureClass::UpstreamOverloaded);
        assert_eq!(
            registry
                .snapshot("provider", "account-a")
                .unwrap()
                .concurrency_limit,
            2
        );
        assert_eq!(
            registry
                .snapshot("provider", "account-b")
                .unwrap()
                .concurrency_limit,
            4
        );
        drop(a);
        drop(b);
    }

    #[tokio::test]
    async fn cancellation_releases_queue_and_admission_permits() {
        let registry = Arc::new(AccountRuntimeRegistry::default());
        let p = policy(1, 1, 2);
        let first = registry
            .acquire(
                "provider",
                "account-a",
                p,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        let queued_registry = registry.clone();
        let queued = tokio::spawn(async move {
            queued_registry
                .acquire(
                    "provider",
                    "account-a",
                    p,
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .await
        });
        wait_for_queue(&registry, "provider", "account-a", 1).await;
        queued.abort();
        let _ = queued.await;
        timeout(Duration::from_secs(1), async {
            loop {
                if registry
                    .snapshot("provider", "account-a")
                    .is_some_and(|snapshot| snapshot.queue_depth == 0)
                {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(first);
        assert_eq!(
            registry
                .snapshot("provider", "account-a")
                .unwrap()
                .in_flight,
            0
        );
    }

    #[tokio::test]
    async fn concurrent_cold_lifecycle_requests_single_flight_startup() {
        let registry = Arc::new(AccountRuntimeRegistry::default());
        let startups = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));
        let p = policy(2, 2, 8);
        let mut tasks = Vec::new();

        for _ in 0..8 {
            let registry = registry.clone();
            let startups = startups.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                registry
                    .single_flight_lifecycle("provider", "account-a", p, |_| async move {
                        startups.fetch_add(1, Ordering::AcqRel);
                        sleep(Duration::from_millis(30)).await;
                        true
                    })
                    .await
                    .unwrap()
            }));
        }
        for task in tasks {
            assert!(task.await.unwrap());
        }
        assert_eq!(startups.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn concurrent_failed_lifecycle_waiters_share_one_startup_result() {
        let registry = Arc::new(AccountRuntimeRegistry::default());
        let startups = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));
        let p = policy(2, 2, 8);
        let mut tasks = Vec::new();

        for _ in 0..8 {
            let registry = registry.clone();
            let startups = startups.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                registry
                    .single_flight_lifecycle("provider", "account-a", p, |_| async move {
                        startups.fetch_add(1, Ordering::AcqRel);
                        sleep(Duration::from_millis(30)).await;
                        false
                    })
                    .await
                    .unwrap()
            }));
        }
        for task in tasks {
            assert!(!task.await.unwrap());
        }
        assert_eq!(startups.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn aborted_lifecycle_operation_does_not_deadlock_future_startup() {
        let registry = Arc::new(AccountRuntimeRegistry::default());
        let p = policy(1, 1, 2);
        let (entered_tx, entered_rx) = oneshot::channel();
        let first_registry = registry.clone();
        let first = tokio::spawn(async move {
            first_registry
                .single_flight_lifecycle("provider", "account-a", p, |_| async move {
                    let _ = entered_tx.send(());
                    sleep(Duration::from_secs(5)).await;
                    false
                })
                .await
        });
        entered_rx.await.unwrap();
        first.abort();
        let _ = first.await;

        let next = timeout(
            Duration::from_secs(1),
            registry.single_flight_lifecycle("provider", "account-a", p, |_| async { true }),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(next);
    }

    #[tokio::test]
    async fn generation_change_rejects_stale_lifecycle_callback() {
        let registry = Arc::new(AccountRuntimeRegistry::default());
        let p = policy(1, 1, 2);
        let (entered_tx, entered_rx) = oneshot::channel();
        let task_registry = registry.clone();
        let task = tokio::spawn(async move {
            task_registry
                .single_flight_lifecycle("provider", "account-a", p, |_| async move {
                    let _ = entered_tx.send(());
                    sleep(Duration::from_millis(50)).await;
                    true
                })
                .await
        });
        entered_rx.await.unwrap();
        registry.bump_account_generation("account-a");
        assert!(matches!(
            task.await.unwrap(),
            Err(AccountLifecycleError::StaleGeneration { .. })
        ));
    }

    #[tokio::test]
    async fn shutdown_reload_reuses_same_runtime_slot() {
        let registry = AccountRuntimeRegistry::default();
        let p = policy(1, 1, 2);
        let before = registry.snapshot_or_create("provider", "account-a", p);
        registry.stop_account("account-a");
        assert_eq!(
            registry
                .snapshot_or_create("provider", "account-a", p)
                .admission_state,
            AdmissionState::Stopped
        );
        registry.activate_account("account-a");
        let after = registry.snapshot_or_create("provider", "account-a", p);
        assert_eq!(registry.runtime_count(), 1);
        assert!(after.generation > before.generation);
        assert_eq!(after.admission_state, AdmissionState::Open);
    }
}
