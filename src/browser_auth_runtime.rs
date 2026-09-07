use crate::browser_auth::{BrowserAuthVault, BrowserAuthVaultError};
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, OnceLock},
};
use tokio::sync::{Mutex, OwnedMutexGuard};

static VAULT: OnceLock<Arc<BrowserAuthVault>> = OnceLock::new();
static RECOVERY: OnceLock<AuthRecoveryCoordinator> = OnceLock::new();

pub fn install(vault: Arc<BrowserAuthVault>) -> Result<(), Arc<BrowserAuthVault>> {
    VAULT.set(vault)
}

pub fn get() -> Option<&'static Arc<BrowserAuthVault>> {
    VAULT.get()
}

pub fn current_generation(session_id: &str) -> Option<Result<u64, BrowserAuthVaultError>> {
    get().map(|vault| vault.current_generation(session_id))
}

pub fn invalidate(session_id: &str) -> Option<Result<u64, BrowserAuthVaultError>> {
    get().map(|vault| vault.invalidate(session_id))
}

#[derive(Default)]
struct AuthRecoveryCoordinator {
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl AuthRecoveryCoordinator {
    async fn acquire(&self, session_id: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().await;
            locks
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    async fn recover_if_stale<F, Fut>(
        &self,
        vault: Arc<BrowserAuthVault>,
        session_id: &str,
        observed_generation: u64,
        recover: F,
    ) -> bool
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = bool>,
    {
        let _guard = self.acquire(session_id).await;
        if vault
            .load(session_id)
            .is_ok_and(|material| material.generation > observed_generation)
        {
            return true;
        }
        if !recover().await {
            return false;
        }
        vault
            .load(session_id)
            .is_ok_and(|material| material.generation > observed_generation)
    }
}

pub async fn recover_generation_if_stale<F, Fut>(
    session_id: &str,
    observed_generation: u64,
    recover: F,
) -> bool
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = bool>,
{
    let Some(vault) = get().cloned() else {
        return false;
    };
    RECOVERY
        .get_or_init(AuthRecoveryCoordinator::default)
        .recover_if_stale(vault, session_id, observed_generation, recover)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_auth::{BrowserAuthCookie, BrowserAuthMaterial};
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use uuid::Uuid;

    fn material(session_id: &str, value: &str) -> BrowserAuthMaterial {
        BrowserAuthMaterial::new(
            session_id,
            "gemini-web",
            "https://gemini.google.com/app",
            "test-agent",
            vec![BrowserAuthCookie {
                name: "__Secure-1PSID".into(),
                value: value.into(),
                domain: ".google.com".into(),
                path: "/".into(),
                expires: 0.0,
                http_only: true,
                secure: true,
                same_site: None,
            }],
            BTreeMap::new(),
            BTreeMap::new(),
        )
    }

    #[tokio::test]
    async fn concurrent_stale_auth_recovery_is_single_flight() {
        let root = std::env::temp_dir().join(format!(
            "llmgateway-auth-recovery-{}",
            Uuid::new_v4().simple()
        ));
        let vault = Arc::new(BrowserAuthVault::open(&root).unwrap());
        let first = vault.store_if_current(&material("gemini-auth", "first"), 0).unwrap();
        assert_eq!(first.generation, 1);

        let coordinator = Arc::new(AuthRecoveryCoordinator::default());
        let recoveries = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let coordinator = coordinator.clone();
            let vault = vault.clone();
            let recoveries = recoveries.clone();
            tasks.push(tokio::spawn(async move {
                let recovery_vault = vault.clone();
                coordinator
                    .recover_if_stale(vault, "gemini-auth", 1, move || async move {
                        recoveries.fetch_add(1, Ordering::AcqRel);
                        let invalidated = recovery_vault.invalidate("gemini-auth").unwrap();
                        recovery_vault
                            .store_if_current(
                                &material("gemini-auth", "refreshed"),
                                invalidated,
                            )
                            .is_ok()
                    })
                    .await
            }));
        }
        for task in tasks {
            assert!(task.await.unwrap());
        }
        assert_eq!(recoveries.load(Ordering::Acquire), 1);
        assert_eq!(vault.current_generation("gemini-auth").unwrap(), 3);
        let _ = std::fs::remove_dir_all(root);
    }
}
