use crate::memory_provenance::MemoryProvenanceStore;
use std::sync::{Arc, OnceLock};

static MEMORY_PROVENANCE: OnceLock<Arc<MemoryProvenanceStore>> = OnceLock::new();

pub fn install(store: Arc<MemoryProvenanceStore>) -> Result<(), Arc<MemoryProvenanceStore>> {
    MEMORY_PROVENANCE.set(store)
}

pub fn get() -> Option<Arc<MemoryProvenanceStore>> {
    MEMORY_PROVENANCE.get().cloned()
}
