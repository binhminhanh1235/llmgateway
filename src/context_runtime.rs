use crate::context_engine::ContextEngine;
use std::sync::{Arc, OnceLock};

static CONTEXT_ENGINE: OnceLock<Arc<ContextEngine>> = OnceLock::new();

pub fn install(engine: Arc<ContextEngine>) -> Result<(), Arc<ContextEngine>> {
    CONTEXT_ENGINE.set(engine)
}

pub fn get() -> Option<Arc<ContextEngine>> {
    CONTEXT_ENGINE.get().cloned()
}
