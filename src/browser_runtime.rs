use crate::chromium_driver::{ChromiumDriver, ChromiumDriverError};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

static BROWSER_RUNTIME: OnceLock<Arc<BrowserRuntimeSupervisor>> = OnceLock::new();

#[derive(Clone, Debug, Deserialize)]
pub struct BrowserRuntimeConfig {
    #[serde(default)]
    pub allow_visible_auto_launch: bool,
    #[serde(default = "default_max_running_browsers")]
    pub max_running_browsers: usize,
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_session_ttl_secs")]
    pub session_ttl_secs: u64,
}

impl Default for BrowserRuntimeConfig {
    fn default() -> Self {
        Self {
            allow_visible_auto_launch: false,
            max_running_browsers: default_max_running_browsers(),
            idle_timeout_secs: default_idle_timeout_secs(),
            session_ttl_secs: default_session_ttl_secs(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ConfigEnvelope {
    #[serde(default)]
    browser_runtime: BrowserRuntimeConfig,
}

#[derive(Debug, Error)]
pub enum BrowserRuntimeConfigError {
    #[error("browser runtime config I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("browser runtime config TOML error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid browser runtime configuration: {0}")]
    InvalidConfig(String),
}

impl BrowserRuntimeConfig {
    pub fn load_from_gateway_config(
        path: impl AsRef<Path>,
    ) -> Result<Self, BrowserRuntimeConfigError> {
        let raw = fs::read_to_string(path)?;
        let envelope: ConfigEnvelope = toml::from_str(&raw)?;
        validate_config(&envelope.browser_runtime)?;
        Ok(envelope.browser_runtime)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowserRuntimeMode {
    Background,
    Interactive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GenerationToken {
    browser: u64,
    page: u64,
}

#[derive(Clone, Debug)]
struct RuntimeEntry {
    running: bool,
    mode: BrowserRuntimeMode,
    browser_generation: u64,
    page_generation: u64,
    active_leases: usize,
    lru_tick: u64,
    started_at: Instant,
    last_used: Instant,
}

#[derive(Default)]
struct RuntimeState {
    entries: BTreeMap<String, RuntimeEntry>,
    tick: u64,
}

impl RuntimeState {
    fn next_tick(&mut self) -> u64 {
        self.tick = self.tick.wrapping_add(1).max(1);
        self.tick
    }

    fn running_count(&self) -> usize {
        self.entries.values().filter(|entry| entry.running).count()
    }

    fn reserve(
        &mut self,
        session_id: &str,
        mode: BrowserRuntimeMode,
        now: Instant,
    ) -> GenerationToken {
        let tick = self.next_tick();
        let browser_generation = self
            .entries
            .get(session_id)
            .map(|entry| entry.browser_generation.wrapping_add(1).max(1))
            .unwrap_or(1);
        self.entries.insert(
            session_id.to_string(),
            RuntimeEntry {
                running: true,
                mode,
                browser_generation,
                page_generation: 0,
                active_leases: 0,
                lru_tick: tick,
                started_at: now,
                last_used: now,
            },
        );
        GenerationToken {
            browser: browser_generation,
            page: 0,
        }
    }

    fn observe_existing(
        &mut self,
        session_id: &str,
        default_mode: BrowserRuntimeMode,
        now: Instant,
    ) -> GenerationToken {
        let tick = self.next_tick();
        if let Some(entry) = self.entries.get_mut(session_id) {
            if !entry.running {
                entry.running = true;
                entry.browser_generation = entry.browser_generation.wrapping_add(1).max(1);
                entry.page_generation = 1;
                entry.started_at = now;
            } else if entry.page_generation == 0 {
                entry.page_generation = 1;
            }
            if entry.mode != BrowserRuntimeMode::Interactive {
                entry.mode = default_mode;
            }
            entry.last_used = now;
            entry.lru_tick = tick;
            return GenerationToken {
                browser: entry.browser_generation,
                page: entry.page_generation,
            };
        }
        self.entries.insert(
            session_id.to_string(),
            RuntimeEntry {
                running: true,
                mode: default_mode,
                browser_generation: 1,
                page_generation: 1,
                active_leases: 0,
                lru_tick: tick,
                started_at: now,
                last_used: now,
            },
        );
        GenerationToken {
            browser: 1,
            page: 1,
        }
    }

    fn complete_launch(&mut self, session_id: &str, browser_generation: u64, now: Instant) -> bool {
        let tick = self.next_tick();
        let Some(entry) = self.entries.get_mut(session_id) else {
            return false;
        };
        if entry.browser_generation != browser_generation || !entry.running {
            return false;
        }
        entry.mode = BrowserRuntimeMode::Background;
        entry.page_generation = entry.page_generation.max(1);
        entry.last_used = now;
        entry.lru_tick = tick;
        true
    }

    fn mark_page_reacquired(
        &mut self,
        session_id: &str,
        token: GenerationToken,
        now: Instant,
    ) -> bool {
        let tick = self.next_tick();
        let Some(entry) = self.entries.get_mut(session_id) else {
            return false;
        };
        if entry.browser_generation != token.browser || entry.page_generation != token.page {
            return false;
        }
        entry.page_generation = entry.page_generation.wrapping_add(1).max(1);
        entry.last_used = now;
        entry.lru_tick = tick;
        true
    }

    fn acquire_lease(&mut self, session_id: &str, now: Instant) -> Option<GenerationToken> {
        let tick = self.next_tick();
        let entry = self.entries.get_mut(session_id)?;
        if !entry.running || entry.page_generation == 0 {
            return None;
        }
        entry.active_leases = entry.active_leases.saturating_add(1);
        entry.last_used = now;
        entry.lru_tick = tick;
        Some(GenerationToken {
            browser: entry.browser_generation,
            page: entry.page_generation,
        })
    }

    fn release_lease(&mut self, session_id: &str, token: GenerationToken, now: Instant) {
        let tick = self.next_tick();
        let Some(entry) = self.entries.get_mut(session_id) else {
            return;
        };
        if entry.browser_generation != token.browser || entry.page_generation != token.page {
            return;
        }
        entry.active_leases = entry.active_leases.saturating_sub(1);
        entry.last_used = now;
        entry.lru_tick = tick;
    }

    #[cfg(test)]
    fn current(&self, session_id: &str, token: GenerationToken) -> bool {
        self.entries.get(session_id).is_some_and(|entry| {
            entry.running
                && entry.browser_generation == token.browser
                && entry.page_generation == token.page
        })
    }

    fn browser_generation_current(&self, session_id: &str, browser_generation: u64) -> bool {
        self.entries
            .get(session_id)
            .is_some_and(|entry| entry.running && entry.browser_generation == browser_generation)
    }

    fn invalidate(&mut self, session_id: &str, now: Instant) {
        let tick = self.next_tick();
        if let Some(entry) = self.entries.get_mut(session_id) {
            entry.browser_generation = entry.browser_generation.wrapping_add(1).max(1);
            entry.page_generation = entry.page_generation.wrapping_add(1).max(1);
            entry.active_leases = 0;
            entry.last_used = now;
            entry.lru_tick = tick;
            return;
        }
        self.entries.insert(
            session_id.to_string(),
            RuntimeEntry {
                running: false,
                mode: BrowserRuntimeMode::Interactive,
                browser_generation: 1,
                page_generation: 1,
                active_leases: 0,
                lru_tick: tick,
                started_at: now,
                last_used: now,
            },
        );
    }

    fn mark_stopped(&mut self, session_id: &str, now: Instant) {
        let tick = self.next_tick();
        if let Some(entry) = self.entries.get_mut(session_id) {
            entry.running = false;
            entry.browser_generation = entry.browser_generation.wrapping_add(1).max(1);
            entry.page_generation = entry.page_generation.wrapping_add(1).max(1);
            entry.active_leases = 0;
            entry.last_used = now;
            entry.lru_tick = tick;
        }
    }

    fn mark_stopped_if_generation(
        &mut self,
        session_id: &str,
        browser_generation: u64,
        now: Instant,
    ) {
        if self
            .entries
            .get(session_id)
            .is_some_and(|entry| entry.browser_generation == browser_generation)
        {
            self.mark_stopped(session_id, now);
        }
    }

    fn lru_background_victim(&self, exclude: &str) -> Option<(String, u64)> {
        self.entries
            .iter()
            .filter(|(session_id, entry)| {
                session_id.as_str() != exclude
                    && entry.running
                    && entry.mode == BrowserRuntimeMode::Background
                    && entry.active_leases == 0
            })
            .min_by(|(id_a, a), (id_b, b)| a.lru_tick.cmp(&b.lru_tick).then_with(|| id_a.cmp(id_b)))
            .map(|(session_id, entry)| (session_id.clone(), entry.browser_generation))
    }

    fn reclaimable(
        &self,
        now: Instant,
        idle_timeout: Duration,
        session_ttl: Duration,
    ) -> Vec<(String, u64)> {
        let mut candidates = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.running
                    && entry.mode == BrowserRuntimeMode::Background
                    && entry.active_leases == 0
                    && (now.duration_since(entry.last_used) >= idle_timeout
                        || now.duration_since(entry.started_at) >= session_ttl)
            })
            .map(|(id, entry)| (id.clone(), entry.browser_generation, entry.lru_tick))
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
        candidates
            .into_iter()
            .map(|(id, generation, _)| (id, generation))
            .collect()
    }
}

pub struct BrowserRuntimeSupervisor {
    config: BrowserRuntimeConfig,
    driver: Arc<ChromiumDriver>,
    state: Mutex<RuntimeState>,
    lifecycle: AsyncMutex<()>,
}

impl BrowserRuntimeSupervisor {
    pub fn new(
        config: BrowserRuntimeConfig,
        driver: Arc<ChromiumDriver>,
    ) -> Result<Self, BrowserRuntimeConfigError> {
        validate_config(&config)?;
        Ok(Self {
            config,
            driver,
            state: Mutex::new(RuntimeState::default()),
            lifecycle: AsyncMutex::new(()),
        })
    }

    pub fn invalidate(&self, session_id: &str) {
        self.lock_state().invalidate(session_id, Instant::now());
    }

    pub fn is_running(&self, session_id: &str) -> bool {
        self.lock_state()
            .entries
            .get(session_id)
            .is_some_and(|entry| entry.running)
    }

    pub fn acquire_lease(self: &Arc<Self>, session_id: &str) -> Option<BrowserRuntimeLease> {
        let token = self
            .lock_state()
            .acquire_lease(session_id, Instant::now())?;
        Some(BrowserRuntimeLease {
            runtime: self.clone(),
            session_id: session_id.to_string(),
            token,
        })
    }

    pub async fn ensure_background_ready(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<bool, ChromiumDriverError> {
        let _lifecycle_guard = self.lifecycle.lock().await;
        self.ensure_background_ready_locked(session_id).await
    }

    pub async fn ensure_background_lease(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<Option<BrowserRuntimeLease>, ChromiumDriverError> {
        let _lifecycle_guard = self.lifecycle.lock().await;
        if !self.ensure_background_ready_locked(session_id).await? {
            return Ok(None);
        }
        Ok(self.acquire_lease(session_id))
    }

    async fn ensure_background_ready_locked(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<bool, ChromiumDriverError> {
        self.sync_running_from_driver().await;

        let status = self.driver.status(session_id).await?;
        if status.running && status.debugger_reachable {
            let token = self.lock_state().observe_existing(
                session_id,
                BrowserRuntimeMode::Background,
                Instant::now(),
            );
            if status.ready_match.is_some() {
                return Ok(true);
            }

            // Recovery ladder: stale page/target is cheaper than a browser restart.
            // Any reacquire or verification failure falls through to the restart step.
            if self
                .driver
                .reacquire_ready_page(session_id)
                .await
                .unwrap_or(false)
            {
                if self
                    .driver
                    .verify(session_id)
                    .await
                    .is_ok_and(|verification| verification.authenticated)
                {
                    self.lock_state()
                        .mark_page_reacquired(session_id, token, Instant::now());
                    return Ok(true);
                }
            }
        }

        if status.running {
            self.driver.suspend(session_id).await?;
            self.lock_state().mark_stopped(session_id, Instant::now());
        }

        if !self.ensure_budget_for(session_id).await? {
            return Ok(false);
        }

        let token =
            self.lock_state()
                .reserve(session_id, BrowserRuntimeMode::Background, Instant::now());
        let mut pending = PendingLaunchGuard::new(self, session_id, token.browser);

        if let Err(error) = self.driver.launch_headless(session_id).await {
            self.lock_state()
                .mark_stopped_if_generation(session_id, token.browser, Instant::now());
            pending.disarm();
            return Err(error);
        }

        let verification = self.driver.verify(session_id).await?;
        if !verification.authenticated {
            let _ = self.driver.suspend(session_id).await;
            self.lock_state()
                .mark_stopped_if_generation(session_id, token.browser, Instant::now());
            pending.disarm();
            return Ok(false);
        }

        let committed =
            self.lock_state()
                .complete_launch(session_id, token.browser, Instant::now());
        pending.disarm();
        Ok(committed)
    }

    pub async fn prepare_interactive(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<(), ChromiumDriverError> {
        let _lifecycle_guard = self.lifecycle.lock().await;
        self.sync_running_from_driver().await;
        self.lock_state().invalidate(session_id, Instant::now());

        if self.driver.status(session_id).await?.running {
            self.driver.suspend(session_id).await?;
            self.lock_state().mark_stopped(session_id, Instant::now());
        }

        if !self.ensure_budget_for(session_id).await? {
            return Err(ChromiumDriverError::ResourceBudgetExhausted(
                self.config.max_running_browsers,
            ));
        }

        self.lock_state()
            .reserve(session_id, BrowserRuntimeMode::Interactive, Instant::now());
        Ok(())
    }

    pub fn note_interactive_started(&self, session_id: &str) {
        let mut state = self.lock_state();
        if state
            .entries
            .get(session_id)
            .is_some_and(|entry| entry.running && entry.mode == BrowserRuntimeMode::Interactive)
        {
            return;
        }
        state.reserve(session_id, BrowserRuntimeMode::Interactive, Instant::now());
    }

    pub fn note_interactive_launch_failed(&self, session_id: &str) {
        self.lock_state().mark_stopped(session_id, Instant::now());
    }

    pub async fn finish_interactive_login(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<crate::chromium_driver::ChromiumStatusView, ChromiumDriverError> {
        let _lifecycle_guard = self.lifecycle.lock().await;
        let status = self.driver.suspend(session_id).await?;
        self.lock_state().mark_stopped(session_id, Instant::now());
        Ok(status)
    }

    pub fn note_explicit_stop(&self, session_id: &str) {
        self.lock_state().mark_stopped(session_id, Instant::now());
    }

    pub async fn reclaim_idle(self: &Arc<Self>) -> Vec<String> {
        let _lifecycle_guard = self.lifecycle.lock().await;
        self.sync_running_from_driver().await;
        let now = Instant::now();
        let idle_timeout = Duration::from_secs(self.config.idle_timeout_secs);
        let session_ttl = Duration::from_secs(self.config.session_ttl_secs);
        let candidates = self
            .lock_state()
            .reclaimable(now, idle_timeout, session_ttl);
        let mut reclaimed = Vec::new();

        for (session_id, generation) in candidates {
            if self.driver.suspend(&session_id).await.is_ok() {
                self.lock_state().mark_stopped_if_generation(
                    &session_id,
                    generation,
                    Instant::now(),
                );
                reclaimed.push(session_id);
            }
        }
        reclaimed
    }

    async fn ensure_budget_for(&self, session_id: &str) -> Result<bool, ChromiumDriverError> {
        loop {
            let victim = {
                let state = self.lock_state();
                if state.running_count() < self.config.max_running_browsers {
                    return Ok(true);
                }
                state.lru_background_victim(session_id)
            };
            let Some((victim_id, generation)) = victim else {
                return Ok(false);
            };
            self.driver.suspend(&victim_id).await?;
            self.lock_state()
                .mark_stopped_if_generation(&victim_id, generation, Instant::now());
        }
    }

    async fn sync_running_from_driver(&self) {
        for session_id in self.driver.configured_session_ids() {
            let Ok(status) = self.driver.status(&session_id).await else {
                continue;
            };
            let default_mode = if self
                .driver
                .session_allows_background_runtime(&session_id)
                .await
            {
                BrowserRuntimeMode::Background
            } else {
                BrowserRuntimeMode::Interactive
            };
            let mut state = self.lock_state();
            let tracked_running = state
                .entries
                .get(&session_id)
                .is_some_and(|entry| entry.running);
            if status.running && !tracked_running {
                state.observe_existing(&session_id, default_mode, Instant::now());
            } else if !status.running && tracked_running {
                state.mark_stopped(&session_id, Instant::now());
            }
        }
    }

    async fn cancel_pending_launch(self: Arc<Self>, session_id: String, browser_generation: u64) {
        let _lifecycle_guard = self.lifecycle.lock().await;
        if !self
            .lock_state()
            .browser_generation_current(&session_id, browser_generation)
        {
            return;
        }
        let _ = self.driver.suspend(&session_id).await;
        self.lock_state().mark_stopped_if_generation(
            &session_id,
            browser_generation,
            Instant::now(),
        );
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, RuntimeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub struct BrowserRuntimeLease {
    runtime: Arc<BrowserRuntimeSupervisor>,
    session_id: String,
    token: GenerationToken,
}

impl Drop for BrowserRuntimeLease {
    fn drop(&mut self) {
        self.runtime
            .lock_state()
            .release_lease(&self.session_id, self.token, Instant::now());
    }
}

struct PendingLaunchGuard {
    runtime: Weak<BrowserRuntimeSupervisor>,
    session_id: String,
    browser_generation: u64,
    armed: bool,
}

impl PendingLaunchGuard {
    fn new(
        runtime: &Arc<BrowserRuntimeSupervisor>,
        session_id: &str,
        browser_generation: u64,
    ) -> Self {
        Self {
            runtime: Arc::downgrade(runtime),
            session_id: session_id.to_string(),
            browser_generation,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingLaunchGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(runtime) = self.runtime.upgrade() else {
            return;
        };
        if !runtime
            .lock_state()
            .browser_generation_current(&self.session_id, self.browser_generation)
        {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let session_id = self.session_id.clone();
        let browser_generation = self.browser_generation;
        handle.spawn(async move {
            runtime
                .cancel_pending_launch(session_id, browser_generation)
                .await;
        });
    }
}

pub fn install(
    runtime: Arc<BrowserRuntimeSupervisor>,
) -> Result<(), Arc<BrowserRuntimeSupervisor>> {
    BROWSER_RUNTIME.set(runtime)
}

pub fn get() -> Option<&'static Arc<BrowserRuntimeSupervisor>> {
    BROWSER_RUNTIME.get()
}

fn validate_config(config: &BrowserRuntimeConfig) -> Result<(), BrowserRuntimeConfigError> {
    if config.allow_visible_auto_launch {
        return Err(BrowserRuntimeConfigError::InvalidConfig(
            "browser_runtime.allow_visible_auto_launch must remain false; visible Chromium is reserved for explicit user interaction".into(),
        ));
    }
    if config.max_running_browsers == 0 || config.max_running_browsers > 16 {
        return Err(BrowserRuntimeConfigError::InvalidConfig(
            "browser_runtime.max_running_browsers must be between 1 and 16".into(),
        ));
    }
    if config.idle_timeout_secs == 0 || config.idle_timeout_secs > 3600 {
        return Err(BrowserRuntimeConfigError::InvalidConfig(
            "browser_runtime.idle_timeout_secs must be between 1 and 3600".into(),
        ));
    }
    if config.session_ttl_secs == 0 || config.session_ttl_secs > 86_400 {
        return Err(BrowserRuntimeConfigError::InvalidConfig(
            "browser_runtime.session_ttl_secs must be between 1 and 86400".into(),
        ));
    }
    Ok(())
}

fn default_max_running_browsers() -> usize {
    1
}

fn default_idle_timeout_secs() -> u64 {
    45
}

fn default_session_ttl_secs() -> u64 {
    300
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_invisible_first_and_bounded() {
        let config = BrowserRuntimeConfig::default();
        assert!(!config.allow_visible_auto_launch);
        assert_eq!(config.max_running_browsers, 1);
        assert_eq!(config.idle_timeout_secs, 45);
        assert_eq!(config.session_ttl_secs, 300);
    }

    #[test]
    fn visible_auto_launch_is_rejected() {
        let config = BrowserRuntimeConfig {
            allow_visible_auto_launch: true,
            ..BrowserRuntimeConfig::default()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn lru_budget_evicts_oldest_idle_background_runtime() {
        let mut state = RuntimeState::default();
        let now = Instant::now();
        let a = state.reserve("session-a", BrowserRuntimeMode::Background, now);
        assert!(state.complete_launch("session-a", a.browser, now));
        let b = state.reserve("session-b", BrowserRuntimeMode::Background, now);
        assert!(state.complete_launch("session-b", b.browser, now));

        let victim = state.lru_background_victim("session-c").unwrap();
        assert_eq!(victim.0, "session-a");
    }

    #[test]
    fn active_lease_protects_runtime_from_lru_eviction() {
        let mut state = RuntimeState::default();
        let now = Instant::now();
        let a = state.reserve("session-a", BrowserRuntimeMode::Background, now);
        assert!(state.complete_launch("session-a", a.browser, now));
        let b = state.reserve("session-b", BrowserRuntimeMode::Background, now);
        assert!(state.complete_launch("session-b", b.browser, now));
        let _lease = state.acquire_lease("session-a", now).unwrap();

        let victim = state.lru_background_victim("session-c").unwrap();
        assert_eq!(victim.0, "session-b");
    }

    #[test]
    fn idle_and_ttl_reclaim_are_deterministic() {
        let mut state = RuntimeState::default();
        let now = Instant::now();
        let token = state.reserve(
            "session-a",
            BrowserRuntimeMode::Background,
            now - Duration::from_secs(120),
        );
        assert!(state.complete_launch("session-a", token.browser, now - Duration::from_secs(120)));
        if let Some(entry) = state.entries.get_mut("session-a") {
            entry.last_used = now - Duration::from_secs(60);
            entry.started_at = now - Duration::from_secs(120);
        }

        let reclaimable = state.reclaimable(now, Duration::from_secs(45), Duration::from_secs(300));
        assert_eq!(reclaimable.len(), 1);
        assert_eq!(reclaimable[0].0, "session-a");
    }

    #[test]
    fn reacquire_advances_page_generation_without_browser_restart() {
        let mut state = RuntimeState::default();
        let now = Instant::now();
        let launch = state.reserve("session-a", BrowserRuntimeMode::Background, now);
        assert!(state.complete_launch("session-a", launch.browser, now));
        let before = state.acquire_lease("session-a", now).unwrap();
        state.release_lease("session-a", before, now);

        assert!(state.mark_page_reacquired("session-a", before, now));
        let after = state.acquire_lease("session-a", now).unwrap();
        assert_eq!(after.browser, before.browser);
        assert_ne!(after.page, before.page);
    }

    #[test]
    fn cancelled_cold_start_releases_reserved_runtime_capacity() {
        let mut state = RuntimeState::default();
        let now = Instant::now();
        let pending = state.reserve("session-a", BrowserRuntimeMode::Background, now);
        assert_eq!(state.running_count(), 1);

        state.mark_stopped_if_generation("session-a", pending.browser, now);

        assert_eq!(state.running_count(), 0);
        assert_eq!(state.entries["session-a"].active_leases, 0);
    }

    #[test]
    fn stale_generation_callback_is_ignored() {
        let mut state = RuntimeState::default();
        let now = Instant::now();
        let token = state.reserve("session-a", BrowserRuntimeMode::Background, now);
        assert!(state.complete_launch("session-a", token.browser, now));
        let current = state.acquire_lease("session-a", now).unwrap();
        assert!(state.current("session-a", current));

        state.invalidate("session-a", now);
        assert!(!state.current("session-a", current));
    }

    #[test]
    fn cancellation_style_lease_release_does_not_leak_capacity() {
        let mut state = RuntimeState::default();
        let now = Instant::now();
        let token = state.reserve("session-a", BrowserRuntimeMode::Background, now);
        assert!(state.complete_launch("session-a", token.browser, now));
        let lease = state.acquire_lease("session-a", now).unwrap();
        assert_eq!(state.entries["session-a"].active_leases, 1);

        state.release_lease("session-a", lease, now);
        assert_eq!(state.entries["session-a"].active_leases, 0);
    }
}
