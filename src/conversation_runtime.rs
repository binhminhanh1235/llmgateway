use crate::conversation::ConversationStore;
use std::sync::{Arc, OnceLock};

static STORE: OnceLock<Arc<ConversationStore>> = OnceLock::new();

pub fn install(store: Arc<ConversationStore>) -> Result<(), Arc<ConversationStore>> {
    STORE.set(store)
}

pub fn get() -> Option<&'static Arc<ConversationStore>> {
    STORE.get()
}
