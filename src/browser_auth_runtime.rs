use crate::browser_auth::BrowserAuthVault;
use std::sync::{Arc, OnceLock};

static VAULT: OnceLock<Arc<BrowserAuthVault>> = OnceLock::new();

pub fn install(vault: Arc<BrowserAuthVault>) -> Result<(), Arc<BrowserAuthVault>> {
    VAULT.set(vault)
}

pub fn get() -> Option<&'static Arc<BrowserAuthVault>> {
    VAULT.get()
}
