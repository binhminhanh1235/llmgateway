use crate::embedding_retrieval::EmbeddingRetriever;
use std::sync::{Arc, OnceLock};

static EMBEDDING_RETRIEVER: OnceLock<Arc<EmbeddingRetriever>> = OnceLock::new();

pub fn install(retriever: Arc<EmbeddingRetriever>) -> Result<(), Arc<EmbeddingRetriever>> {
    EMBEDDING_RETRIEVER.set(retriever)
}

pub fn get() -> Option<Arc<EmbeddingRetriever>> {
    EMBEDDING_RETRIEVER.get().cloned()
}
