use crate::browser_session::BrowserSessionStore;
use std::sync::{Arc, OnceLock};

static STORE: OnceLock<Arc<BrowserSessionStore>> = OnceLock::new();

pub fn install(store: Arc<BrowserSessionStore>) -> Result<(), Arc<BrowserSessionStore>> {
    STORE.set(store)
}

pub fn get() -> Option<&'static Arc<BrowserSessionStore>> {
    STORE.get()
}
