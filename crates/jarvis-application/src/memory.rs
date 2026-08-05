//! Memory retrieval use case (FR-16, docs/02 §7).
//!
//! Retrieval is a local capability: it embeds a bounded query and asks the
//! memory port for owner/layer-filtered nearest neighbours. It never invokes a
//! reasoning model. Provenance is returned as [`MemoryHit`] values so the
//! context assembler can record exactly what it forwarded later in M4.

use std::sync::Arc;

use jarvis_domain::ids::UserId;
use jarvis_domain::memory::MemoryLayer;
use tokio_util::sync::CancellationToken;

use crate::ports::{
    EmbeddingError, EmbeddingProvider, MemoryHit, MemoryRetriever, RepositoryError,
};

pub const MAX_RETRIEVAL_QUERY_BYTES: usize = 512;
pub const MAX_RETRIEVAL_RESULTS: u32 = 8;

#[derive(Debug, thiserror::Error)]
pub enum RetrievalError {
    #[error("retrieval query is empty or too long")]
    InvalidQuery,
    #[error("embedding failed")]
    Embedding(#[source] EmbeddingError),
    #[error("memory storage failed")]
    Storage(#[source] RepositoryError),
}

pub struct MemoryRetrievalService {
    embedder: Arc<dyn EmbeddingProvider>,
    retriever: Arc<dyn MemoryRetriever>,
}

impl MemoryRetrievalService {
    pub fn new(embedder: Arc<dyn EmbeddingProvider>, retriever: Arc<dyn MemoryRetriever>) -> Self {
        Self {
            embedder,
            retriever,
        }
    }

    pub async fn retrieve(
        &self,
        user_id: &UserId,
        layer: Option<MemoryLayer>,
        query: &str,
        limit: u32,
        cancel: &CancellationToken,
    ) -> Result<Vec<MemoryHit>, RetrievalError> {
        if query.trim().is_empty() || query.len() > MAX_RETRIEVAL_QUERY_BYTES {
            return Err(RetrievalError::InvalidQuery);
        }
        if cancel.is_cancelled() {
            return Err(RetrievalError::Embedding(EmbeddingError::Cancelled));
        }
        let embedding = self
            .embedder
            .embed(query, cancel)
            .await
            .map_err(RetrievalError::Embedding)?;
        if embedding.len() != self.embedder.dimensions() {
            return Err(RetrievalError::Embedding(EmbeddingError::InvalidDimensions));
        }
        self.retriever
            .retrieve(user_id, layer, &embedding, limit.min(MAX_RETRIEVAL_RESULTS))
            .await
            .map_err(RetrievalError::Storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::MemoryHit;
    use async_trait::async_trait;
    use std::str::FromStr;
    use std::sync::Mutex;

    struct FakeEmbedder;

    #[async_trait]
    impl EmbeddingProvider for FakeEmbedder {
        fn model_id(&self) -> &str {
            "fixture"
        }
        fn dimensions(&self) -> usize {
            2
        }
        async fn embed(&self, _: &str, _: &CancellationToken) -> Result<Vec<f32>, EmbeddingError> {
            Ok(vec![1.0, 0.0])
        }
    }

    struct FakeRetriever(Mutex<(usize, u32)>);

    #[async_trait]
    impl MemoryRetriever for FakeRetriever {
        async fn retrieve(
            &self,
            _: &UserId,
            _: Option<MemoryLayer>,
            embedding: &[f32],
            limit: u32,
        ) -> Result<Vec<MemoryHit>, RepositoryError> {
            *self.0.lock().unwrap() = (embedding.len(), limit);
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn retrieval_is_local_bounded_and_provider_neutral() {
        let retriever = Arc::new(FakeRetriever(Mutex::new((0, 0))));
        let service = MemoryRetrievalService::new(Arc::new(FakeEmbedder), retriever.clone());
        let user = UserId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        service
            .retrieve(
                &user,
                None,
                "what tea do I like",
                100,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(*retriever.0.lock().unwrap(), (2, MAX_RETRIEVAL_RESULTS));
    }

    #[tokio::test]
    async fn cancelled_or_unbounded_queries_do_not_embed() {
        let retriever = Arc::new(FakeRetriever(Mutex::new((0, 0))));
        let service = MemoryRetrievalService::new(Arc::new(FakeEmbedder), retriever);
        let user = UserId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            service.retrieve(&user, None, "fact", 1, &cancel).await,
            Err(RetrievalError::Embedding(EmbeddingError::Cancelled))
        ));
        assert!(matches!(
            service
                .retrieve(
                    &user,
                    None,
                    &"x".repeat(MAX_RETRIEVAL_QUERY_BYTES + 1),
                    1,
                    &CancellationToken::new()
                )
                .await,
            Err(RetrievalError::InvalidQuery)
        ));
    }
}
