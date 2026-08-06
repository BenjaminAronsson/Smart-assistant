//! Memory retrieval use case (FR-16, docs/02 §7).
//!
//! Retrieval is a local capability: it embeds a bounded query and asks the
//! memory port for owner/layer-filtered nearest neighbours. It never invokes a
//! reasoning model. Provenance is returned as [`MemoryHit`] values so the
//! context assembler can record exactly what it forwarded later in M4.

use std::sync::Arc;

use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::UserId;
use jarvis_domain::memory::Memory;
use jarvis_domain::memory::MemoryLayer;
use tokio_util::sync::CancellationToken;

use crate::ports::{
    EmbeddedMemory, EmbeddedMemoryStore, EmbeddingError, EmbeddingProvider, MemoryHit,
    MemoryRetriever, RepositoryError,
};

pub const MAX_RETRIEVAL_QUERY_BYTES: usize = 512;
pub const MAX_RETRIEVAL_RESULTS: u32 = 8;
pub const MAX_EMBEDDING_TEXT_BYTES: usize = 2_000;

/// A hit below this cosine similarity is treated as noise, not context: without
/// a floor, every message — even one wholly unrelated to anything the owner has
/// told Jarvis — attaches up to [`MAX_RETRIEVAL_RESULTS`] stored personal facts
/// to the prompt sent to the external reasoning provider (docs/02 §7 retrieval
/// is supposed to combine similarity with deterministic filters; "no secrets in
/// model context by default"). Conservative starting point, not empirically
/// tuned against this embedding model — revisit with the M4 evaluation harness.
pub const MIN_RETRIEVAL_SIMILARITY: f32 = 0.3;

#[derive(Debug, thiserror::Error)]
pub enum RetrievalError {
    #[error("retrieval query is empty or too long")]
    InvalidQuery,
    #[error("embedding failed")]
    Embedding(#[source] EmbeddingError),
    #[error("memory storage failed")]
    Storage(#[source] RepositoryError),
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryWriteError {
    #[error("memory text is empty or too long")]
    InvalidText,
    #[error("embedding failed")]
    Embedding(#[source] EmbeddingError),
    #[error("memory storage failed")]
    Storage(#[source] RepositoryError),
}

/// Bounded, atomic memory writes. Embedding happens before entering the store
/// transaction, and the store receives the vector and audit event together.
pub struct MemoryWriteService {
    embedder: Arc<dyn EmbeddingProvider>,
    store: Arc<dyn EmbeddedMemoryStore>,
}

impl MemoryWriteService {
    pub fn new(embedder: Arc<dyn EmbeddingProvider>, store: Arc<dyn EmbeddedMemoryStore>) -> Self {
        Self { embedder, store }
    }

    async fn embedding(
        &self,
        text: &str,
        cancel: &CancellationToken,
    ) -> Result<EmbeddedMemory, MemoryWriteError> {
        if text.trim().is_empty() || text.len() > MAX_EMBEDDING_TEXT_BYTES {
            return Err(MemoryWriteError::InvalidText);
        }
        if cancel.is_cancelled() {
            return Err(MemoryWriteError::Embedding(EmbeddingError::Cancelled));
        }
        let embedding = self
            .embedder
            .embed(text, cancel)
            .await
            .map_err(MemoryWriteError::Embedding)?;
        if embedding.len() != self.embedder.dimensions()
            || embedding.is_empty()
            || embedding.iter().any(|value| !value.is_finite())
        {
            return Err(MemoryWriteError::Embedding(
                EmbeddingError::InvalidDimensions,
            ));
        }
        Ok(EmbeddedMemory {
            model_id: self.embedder.model_id().to_owned(),
            dimensions: embedding.len(),
            embedding,
        })
    }

    pub async fn create(
        &self,
        memory: &Memory,
        audit: &AuditEvent,
        cancel: &CancellationToken,
    ) -> Result<(), MemoryWriteError> {
        let embedding = self.embedding(&memory.text, cancel).await?;
        self.store
            .create_embedded(memory, &embedding, audit)
            .await
            .map_err(MemoryWriteError::Storage)
    }

    pub async fn replace(
        &self,
        memory: &Memory,
        audit: &AuditEvent,
        cancel: &CancellationToken,
    ) -> Result<(), MemoryWriteError> {
        let embedding = self.embedding(&memory.text, cancel).await?;
        self.store
            .replace_embedded(memory, &embedding, audit)
            .await
            .map_err(MemoryWriteError::Storage)
    }

    pub async fn reembed(
        &self,
        memory: &Memory,
        audit: &AuditEvent,
        cancel: &CancellationToken,
    ) -> Result<(), MemoryWriteError> {
        self.replace(memory, audit, cancel).await
    }
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
        let hits = self
            .retriever
            .retrieve(user_id, layer, &embedding, limit.min(MAX_RETRIEVAL_RESULTS))
            .await
            .map_err(RetrievalError::Storage)?;
        Ok(hits
            .into_iter()
            .filter(|hit| hit.similarity >= MIN_RETRIEVAL_SIMILARITY)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{EmbeddedMemoryStore, MemoryHit};
    use async_trait::async_trait;
    use jarvis_domain::audit::AuditEvent;
    use jarvis_domain::location::Sensitivity;
    use jarvis_domain::memory::{Memory, MemoryScope, MemorySource, RetentionRule};
    use std::str::FromStr;
    use std::sync::Mutex;
    use std::time::SystemTime;

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

    struct FakeEmbeddedStore(Mutex<Vec<EmbeddedMemory>>);

    #[async_trait]
    impl EmbeddedMemoryStore for FakeEmbeddedStore {
        async fn create_embedded(
            &self,
            _: &Memory,
            embedding: &EmbeddedMemory,
            _: &AuditEvent,
        ) -> Result<(), RepositoryError> {
            self.0.lock().unwrap().push(embedding.clone());
            Ok(())
        }

        async fn replace_embedded(
            &self,
            _: &Memory,
            embedding: &EmbeddedMemory,
            _: &AuditEvent,
        ) -> Result<(), RepositoryError> {
            self.0.lock().unwrap().push(embedding.clone());
            Ok(())
        }
    }

    fn memory() -> Memory {
        Memory::new(
            "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
            "01BX5ZZKBKACTAV9WEVGEMMVRZ".parse().unwrap(),
            MemoryLayer::Semantic,
            "likes green tea".to_owned(),
            MemorySource::Explicit,
            MemoryScope::User,
            RetentionRule::UntilForgotten,
            1.0,
            Sensitivity::Normal,
            false,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap()
    }

    fn audit() -> AuditEvent {
        AuditEvent {
            occurred_at: SystemTime::UNIX_EPOCH,
            actor: "test".to_owned(),
            event_type: "memory.created".to_owned(),
            target: "memory:test".to_owned(),
            correlation_id: None,
            payload_json: "{}".to_owned(),
        }
    }

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

    struct FixedHitsRetriever(Vec<MemoryHit>);

    #[async_trait]
    impl MemoryRetriever for FixedHitsRetriever {
        async fn retrieve(
            &self,
            _: &UserId,
            _: Option<MemoryLayer>,
            _: &[f32],
            _: u32,
        ) -> Result<Vec<MemoryHit>, RepositoryError> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn hits_below_the_similarity_floor_are_dropped_before_reaching_a_prompt() {
        let relevant = MemoryHit {
            memory: memory(),
            similarity: 0.8,
        };
        let noise = MemoryHit {
            memory: memory(),
            similarity: 0.1,
        };
        let retriever = Arc::new(FixedHitsRetriever(vec![relevant.clone(), noise]));
        let service = MemoryRetrievalService::new(Arc::new(FakeEmbedder), retriever);
        let user = UserId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        let hits = service
            .retrieve(
                &user,
                None,
                "what tea do I like",
                8,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(hits, vec![relevant]);
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

    #[tokio::test]
    async fn memory_write_populates_bounded_embedding_for_create_and_replace() {
        let store = Arc::new(FakeEmbeddedStore(Mutex::new(Vec::new())));
        let service = MemoryWriteService::new(Arc::new(FakeEmbedder), store.clone());
        let memory = memory();
        service
            .create(&memory, &audit(), &CancellationToken::new())
            .await
            .unwrap();
        service
            .replace(&memory, &audit(), &CancellationToken::new())
            .await
            .unwrap();
        let writes = store.0.lock().unwrap();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].model_id, "fixture");
        assert_eq!(writes[0].dimensions, 2);
        assert_eq!(writes[0].embedding, vec![1.0, 0.0]);
    }

    #[tokio::test]
    async fn memory_write_rejects_cancelled_or_invalid_provider_before_store() {
        let store = Arc::new(FakeEmbeddedStore(Mutex::new(Vec::new())));
        let service = MemoryWriteService::new(Arc::new(FakeEmbedder), store.clone());
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            service.create(&memory(), &audit(), &cancel).await,
            Err(MemoryWriteError::Embedding(EmbeddingError::Cancelled))
        ));
        assert!(store.0.lock().unwrap().is_empty());
    }
}
