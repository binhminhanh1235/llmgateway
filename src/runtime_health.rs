use crate::execution::{ExecutionFailure, FailureClass, FailureScope};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use tokio::sync::RwLock;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeHealthScope {
    Account,
    Transport,
    Session,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct RuntimeHealthKey {
    pub scope: RuntimeHealthScope,
    pub provider: String,
    pub account_id: String,
    pub transport: Option<String>,
    pub resource_id: Option<String>,
}

impl RuntimeHealthKey {
    pub fn account(provider: &str, account_id: &str) -> Self {
        Self {
            scope: RuntimeHealthScope::Account,
            provider: provider.to_string(),
            account_id: account_id.to_string(),
            transport: None,
            resource_id: None,
        }
    }

    pub fn transport(provider: &str, account_id: &str, transport: &str) -> Self {
        Self {
            scope: RuntimeHealthScope::Transport,
            provider: provider.to_string(),
            account_id: account_id.to_string(),
            transport: Some(transport.to_string()),
            resource_id: None,
        }
    }

    pub fn session(provider: &str, account_id: &str, transport: &str, resource_id: &str) -> Self {
        Self {
            scope: RuntimeHealthScope::Session,
            provider: provider.to_string(),
            account_id: account_id.to_string(),
            transport: Some(transport.to_string()),
            resource_id: Some(resource_id.to_string()),
        }
    }

    pub fn from_failure(failure: &ExecutionFailure) -> Vec<Self> {
        let (Some(provider), Some(account_id)) =
            (failure.provider.as_deref(), failure.account_id.as_deref())
        else {
            return Vec::new();
        };
        match failure.scope {
            FailureScope::Account => vec![Self::account(provider, account_id)],
            FailureScope::Transport | FailureScope::Provider => failure
                .transport
                .as_deref()
                .map(|transport| vec![Self::transport(provider, account_id, transport)])
                .unwrap_or_else(|| vec![Self::account(provider, account_id)]),
            FailureScope::Session => {
                let Some(transport) = failure.transport.as_deref() else {
                    return vec![Self::account(provider, account_id)];
                };
                let resource_id = failure.resource_id.as_deref().unwrap_or("default");
                vec![
                    Self::session(provider, account_id, transport, resource_id),
                    Self::transport(provider, account_id, transport),
                ]
            }
            FailureScope::Request | FailureScope::Conversation | FailureScope::Model => Vec::new(),
        }
    }

    fn stable_identity(&self) -> String {
        format!(
            "{:?}|{}|{}|{}|{}",
            self.scope,
            self.provider,
            self.account_id,
            self.transport.as_deref().unwrap_or(""),
            self.resource_id.as_deref().unwrap_or("")
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakerState {
    #[default]
    Closed,
    Open,
    HalfOpen,
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeHealthSnapshot {
    pub key: RuntimeHealthKey,
    pub state: BreakerState,
    pub consecutive_failures: u32,
    pub recovery_successes: u32,
    pub retry_at: Option<DateTime<Utc>>,
    pub half_open_in_flight: u32,
    pub last_error: Option<String>,
    pub last_failure_class: Option<String>,
    pub last_cooldown_secs: Option<i64>,
    pub recovery_penalty: i32,
}

#[derive(Clone, Debug)]
struct RuntimeHealthEntry {
    state: BreakerState,
    consecutive_failures: u32,
    recovery_successes: u32,
    retry_at: Option<DateTime<Utc>>,
    half_open_in_flight: u32,
    last_error: Option<String>,
    last_failure_class: Option<String>,
    last_cooldown_secs: Option<i64>,
}

impl Default for RuntimeHealthEntry {
    fn default() -> Self {
        Self {
            state: BreakerState::Closed,
            consecutive_failures: 0,
            recovery_successes: 0,
            retry_at: None,
            half_open_in_flight: 0,
            last_error: None,
            last_failure_class: None,
            last_cooldown_secs: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeHealthSettings {
    pub max_half_open_probes: u32,
    pub close_after_successes: u32,
    pub max_cooldown_secs: i64,
    pub half_open_penalty: i32,
}

impl Default for RuntimeHealthSettings {
    fn default() -> Self {
        Self {
            max_half_open_probes: 1,
            close_after_successes: 2,
            max_cooldown_secs: 900,
            half_open_penalty: 25,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeHealthPermit {
    keys: Vec<RuntimeHealthKey>,
}

#[derive(Clone, Debug)]
pub struct RuntimeHealthBlocked {
    pub snapshot: RuntimeHealthSnapshot,
}

pub struct RuntimeHealthGraph {
    entries: RwLock<HashMap<RuntimeHealthKey, RuntimeHealthEntry>>,
    settings: RuntimeHealthSettings,
}

impl Default for RuntimeHealthGraph {
    fn default() -> Self {
        Self::new(RuntimeHealthSettings::default())
    }
}

impl RuntimeHealthGraph {
    pub fn new(settings: RuntimeHealthSettings) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            settings,
        }
    }

    pub async fn snapshots(&self, keys: &[RuntimeHealthKey]) -> Vec<RuntimeHealthSnapshot> {
        self.snapshots_at(keys, Utc::now()).await
    }

    async fn snapshots_at(
        &self,
        keys: &[RuntimeHealthKey],
        now: DateTime<Utc>,
    ) -> Vec<RuntimeHealthSnapshot> {
        let entries = self.entries.read().await;
        unique_keys(keys)
            .into_iter()
            .map(|key| snapshot_from_entry(&key, entries.get(&key), now, &self.settings))
            .collect()
    }

    #[cfg(test)]
    async fn snapshot_at(
        &self,
        key: &RuntimeHealthKey,
        now: DateTime<Utc>,
    ) -> RuntimeHealthSnapshot {
        let entries = self.entries.read().await;
        snapshot_from_entry(key, entries.get(key), now, &self.settings)
    }

    pub async fn try_acquire_all(
        &self,
        keys: &[RuntimeHealthKey],
    ) -> Result<RuntimeHealthPermit, RuntimeHealthBlocked> {
        self.try_acquire_all_at(keys, Utc::now()).await
    }

    async fn try_acquire_all_at(
        &self,
        keys: &[RuntimeHealthKey],
        now: DateTime<Utc>,
    ) -> Result<RuntimeHealthPermit, RuntimeHealthBlocked> {
        let keys = unique_keys(keys);
        let mut entries = self.entries.write().await;

        for key in &keys {
            let snapshot = snapshot_from_entry(key, entries.get(key), now, &self.settings);
            match snapshot.state {
                BreakerState::Open => return Err(RuntimeHealthBlocked { snapshot }),
                BreakerState::HalfOpen
                    if snapshot.half_open_in_flight >= self.settings.max_half_open_probes =>
                {
                    return Err(RuntimeHealthBlocked { snapshot });
                }
                BreakerState::Closed | BreakerState::HalfOpen => {}
            }
        }

        for key in &keys {
            let should_probe = entries
                .get(key)
                .is_some_and(|entry| effective_state(entry, now) == BreakerState::HalfOpen);
            if should_probe {
                let entry = entries.entry(key.clone()).or_default();
                if entry.state == BreakerState::Open {
                    entry.state = BreakerState::HalfOpen;
                    entry.retry_at = None;
                    entry.recovery_successes = 0;
                }
                entry.half_open_in_flight = entry.half_open_in_flight.saturating_add(1);
            }
        }

        Ok(RuntimeHealthPermit { keys })
    }

    pub async fn release(&self, permit: RuntimeHealthPermit) {
        self.release_at(permit, Utc::now()).await;
    }

    async fn release_at(&self, permit: RuntimeHealthPermit, now: DateTime<Utc>) {
        let mut entries = self.entries.write().await;
        for key in permit.keys {
            if let Some(entry) = entries.get_mut(&key) {
                if effective_state(entry, now) == BreakerState::HalfOpen {
                    entry.half_open_in_flight = entry.half_open_in_flight.saturating_sub(1);
                }
            }
        }
    }

    pub async fn record_success(&self, permit: RuntimeHealthPermit) {
        self.record_success_at(permit, Utc::now()).await;
    }

    async fn record_success_at(&self, permit: RuntimeHealthPermit, now: DateTime<Utc>) {
        let mut entries = self.entries.write().await;
        for key in permit.keys {
            let Some(entry) = entries.get_mut(&key) else {
                continue;
            };
            if effective_state(entry, now) != BreakerState::HalfOpen {
                continue;
            }
            if entry.state == BreakerState::Open {
                entry.state = BreakerState::HalfOpen;
                entry.retry_at = None;
            }
            entry.half_open_in_flight = entry.half_open_in_flight.saturating_sub(1);
            entry.recovery_successes = entry.recovery_successes.saturating_add(1);
            if entry.recovery_successes >= self.settings.close_after_successes.max(1) {
                *entry = RuntimeHealthEntry::default();
            }
        }
    }

    pub async fn record_failure(
        &self,
        permit: RuntimeHealthPermit,
        affected_keys: &[RuntimeHealthKey],
        failure_class: FailureClass,
        base_cooldown_secs: i64,
        error: &str,
    ) {
        self.record_failure_at(
            permit,
            affected_keys,
            failure_class,
            base_cooldown_secs,
            error,
            Utc::now(),
        )
        .await;
    }

    async fn record_failure_at(
        &self,
        permit: RuntimeHealthPermit,
        affected_keys: &[RuntimeHealthKey],
        failure_class: FailureClass,
        base_cooldown_secs: i64,
        error: &str,
        now: DateTime<Utc>,
    ) {
        let mut entries = self.entries.write().await;
        for key in permit.keys {
            if let Some(entry) = entries.get_mut(&key) {
                if effective_state(entry, now) == BreakerState::HalfOpen {
                    entry.half_open_in_flight = entry.half_open_in_flight.saturating_sub(1);
                }
            }
        }
        if base_cooldown_secs <= 0 {
            return;
        }

        for key in unique_keys(affected_keys) {
            let entry = entries.entry(key.clone()).or_default();
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            entry.recovery_successes = 0;
            entry.half_open_in_flight = 0;
            let cooldown = exponential_cooldown_with_jitter(
                base_cooldown_secs,
                entry.consecutive_failures,
                &key,
                self.settings.max_cooldown_secs,
            );
            entry.state = BreakerState::Open;
            entry.retry_at = Some(now + Duration::seconds(cooldown));
            entry.last_error = Some(error.to_string());
            entry.last_failure_class = Some(failure_class.as_str().to_string());
            entry.last_cooldown_secs = Some(cooldown);
        }
    }
}

fn unique_keys(keys: &[RuntimeHealthKey]) -> Vec<RuntimeHealthKey> {
    let mut seen = HashSet::new();
    keys.iter()
        .filter(|key| seen.insert((*key).clone()))
        .cloned()
        .collect()
}

fn effective_state(entry: &RuntimeHealthEntry, now: DateTime<Utc>) -> BreakerState {
    if entry.state == BreakerState::Open
        && entry
            .retry_at
            .as_ref()
            .is_some_and(|retry_at| retry_at <= &now)
    {
        BreakerState::HalfOpen
    } else {
        entry.state
    }
}

fn snapshot_from_entry(
    key: &RuntimeHealthKey,
    entry: Option<&RuntimeHealthEntry>,
    now: DateTime<Utc>,
    settings: &RuntimeHealthSettings,
) -> RuntimeHealthSnapshot {
    let default = RuntimeHealthEntry::default();
    let entry = entry.unwrap_or(&default);
    let state = effective_state(entry, now);
    RuntimeHealthSnapshot {
        key: key.clone(),
        state,
        consecutive_failures: entry.consecutive_failures,
        recovery_successes: entry.recovery_successes,
        retry_at: entry.retry_at,
        half_open_in_flight: entry.half_open_in_flight,
        last_error: entry.last_error.clone(),
        last_failure_class: entry.last_failure_class.clone(),
        last_cooldown_secs: entry.last_cooldown_secs,
        recovery_penalty: if state == BreakerState::HalfOpen {
            settings.half_open_penalty.max(0)
        } else {
            0
        },
    }
}

fn exponential_cooldown_with_jitter(
    base_cooldown_secs: i64,
    consecutive_failures: u32,
    key: &RuntimeHealthKey,
    max_cooldown_secs: i64,
) -> i64 {
    let exponent = consecutive_failures.saturating_sub(1).min(6);
    let multiplier = 1_i64 << exponent;
    let capped = base_cooldown_secs
        .max(1)
        .saturating_mul(multiplier)
        .min(max_cooldown_secs.max(1));
    let span = (capped / 10).max(1);
    let hash = stable_hash(&format!(
        "{}|{}",
        key.stable_identity(),
        consecutive_failures
    ));
    let width = span.saturating_mul(2).saturating_add(1) as u64;
    let offset = (hash % width) as i64 - span;
    capped
        .saturating_add(offset)
        .clamp(1, max_cooldown_secs.max(1))
}

fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.timestamp_millis_opt(1_800_000_000_000)
            .single()
            .unwrap()
    }

    #[tokio::test]
    async fn transport_breakers_are_isolated_for_same_logical_account() {
        let graph = RuntimeHealthGraph::default();
        let direct = RuntimeHealthKey::transport("browser-qwen", "account-a", "direct_http");
        let browser = RuntimeHealthKey::transport("browser-qwen", "account-a", "browser_runtime");
        let permit = graph
            .try_acquire_all_at(&[direct.clone()], now())
            .await
            .unwrap();
        graph
            .record_failure_at(
                permit,
                &[direct.clone()],
                FailureClass::WafRejected,
                30,
                "waf rejected direct transport",
                now(),
            )
            .await;

        assert_eq!(
            graph.snapshot_at(&direct, now()).await.state,
            BreakerState::Open
        );
        assert_eq!(
            graph.snapshot_at(&browser, now()).await.state,
            BreakerState::Closed
        );
    }

    #[tokio::test]
    async fn half_open_probe_concurrency_is_bounded() {
        let graph = RuntimeHealthGraph::default();
        let key = RuntimeHealthKey::transport("provider", "account", "direct_http");
        let permit = graph
            .try_acquire_all_at(&[key.clone()], now())
            .await
            .unwrap();
        graph
            .record_failure_at(
                permit,
                &[key.clone()],
                FailureClass::NetworkTransient,
                10,
                "network",
                now(),
            )
            .await;
        let retry_at = graph.snapshot_at(&key, now()).await.retry_at.unwrap();
        let probe_time = retry_at + Duration::milliseconds(1);

        let first = graph
            .try_acquire_all_at(&[key.clone()], probe_time)
            .await
            .unwrap();
        let second = graph.try_acquire_all_at(&[key.clone()], probe_time).await;
        assert!(second.is_err());
        graph.release_at(first, probe_time).await;
    }

    #[tokio::test]
    async fn half_open_requires_multiple_successes_before_closing() {
        let graph = RuntimeHealthGraph::default();
        let key = RuntimeHealthKey::transport("provider", "account", "direct_http");
        let permit = graph
            .try_acquire_all_at(&[key.clone()], now())
            .await
            .unwrap();
        graph
            .record_failure_at(
                permit,
                &[key.clone()],
                FailureClass::NetworkTransient,
                10,
                "network",
                now(),
            )
            .await;
        let probe_time =
            graph.snapshot_at(&key, now()).await.retry_at.unwrap() + Duration::milliseconds(1);

        let first = graph
            .try_acquire_all_at(&[key.clone()], probe_time)
            .await
            .unwrap();
        graph.record_success_at(first, probe_time).await;
        assert_eq!(
            graph.snapshot_at(&key, probe_time).await.state,
            BreakerState::HalfOpen
        );

        let second = graph
            .try_acquire_all_at(&[key.clone()], probe_time)
            .await
            .unwrap();
        graph.record_success_at(second, probe_time).await;
        assert_eq!(
            graph.snapshot_at(&key, probe_time).await.state,
            BreakerState::Closed
        );
    }

    #[tokio::test]
    async fn repeated_failures_use_exponential_cooldown_with_bounded_jitter() {
        let graph = RuntimeHealthGraph::default();
        let key = RuntimeHealthKey::transport("provider", "account", "direct_http");

        let first_permit = graph
            .try_acquire_all_at(&[key.clone()], now())
            .await
            .unwrap();
        graph
            .record_failure_at(
                first_permit,
                &[key.clone()],
                FailureClass::NetworkTransient,
                10,
                "first",
                now(),
            )
            .await;
        let first = graph
            .snapshot_at(&key, now())
            .await
            .last_cooldown_secs
            .unwrap();

        let empty = RuntimeHealthPermit { keys: Vec::new() };
        graph
            .record_failure_at(
                empty,
                &[key.clone()],
                FailureClass::NetworkTransient,
                10,
                "second",
                now(),
            )
            .await;
        let second = graph
            .snapshot_at(&key, now())
            .await
            .last_cooldown_secs
            .unwrap();

        assert!((9..=11).contains(&first));
        assert!((18..=22).contains(&second));
        assert!(second > first);
    }
}
