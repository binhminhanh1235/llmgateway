use crate::browser_provider::BrowserProviderRegistry;
use std::sync::{Arc, OnceLock};

static REGISTRY: OnceLock<Arc<BrowserProviderRegistry>> = OnceLock::new();

pub fn install(
    registry: Arc<BrowserProviderRegistry>,
) -> Result<(), Arc<BrowserProviderRegistry>> {
    REGISTRY.set(registry)
}

pub fn get() -> Option<&'static Arc<BrowserProviderRegistry>> {
    REGISTRY.get()
}
