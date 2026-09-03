use crate::quota_usage::QuotaUsageStore;
use std::sync::{Arc, OnceLock};

static QUOTA_USAGE: OnceLock<Arc<QuotaUsageStore>> = OnceLock::new();

pub fn install(store: Arc<QuotaUsageStore>) -> Result<(), Arc<QuotaUsageStore>> {
    QUOTA_USAGE.set(store)
}

pub fn get() -> Option<&'static Arc<QuotaUsageStore>> {
    QUOTA_USAGE.get()
}
