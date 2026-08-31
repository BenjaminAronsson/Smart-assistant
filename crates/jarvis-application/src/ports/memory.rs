use super::shared::RepositoryError;
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::memory::{Memory, MemoryLayer};
use std::time::SystemTime;

/// Durable memory persistence (FR-16, docs/02 §7). Implementations own the
/// database transaction and must co-transact the supplied audit event for
/// every mutation. User identity is an explicit argument on every read/write
/// so a route cannot accidentally turn an owner-scoped collection into a
/// global one.
#[async_trait::async_trait]
pub trait MemoryStore: Send + Sync {
    async fn create(&self, memory: &Memory, audit: &AuditEvent) -> Result<(), RepositoryError>;
    async fn get(
        &self,
        user_id: &jarvis_domain::ids::UserId,
        id: &jarvis_domain::ids::MemoryId,
    ) -> Result<Option<Memory>, RepositoryError>;
    async fn list(
        &self,
        user_id: &jarvis_domain::ids::UserId,
        layer: Option<MemoryLayer>,
        query: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Memory>, RepositoryError>;
    /// Replace mutable metadata/text. The implementation must invalidate any
    /// old embedding in the same transaction; re-embedding is a later,
    /// deferrable job and stale vectors must never be used.
    async fn replace(&self, memory: &Memory, audit: &AuditEvent) -> Result<(), RepositoryError>;
    /// Forget is idempotent. `Ok(false)` means the scoped item was already
    /// absent; no audit row is written for a change that did not happen.
    async fn forget(
        &self,
        user_id: &jarvis_domain::ids::UserId,
        id: &jarvis_domain::ids::MemoryId,
        audit: &AuditEvent,
    ) -> Result<bool, RepositoryError>;
}

/// A memory mutation whose embedding was produced from the exact text being
/// written. Implementations must persist the memory, source, embedding, and
/// audit event in one transaction; a provider failure must never leave a
/// stale vector searchable.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddedMemory {
    pub model_id: String,
    pub dimensions: usize,
    pub embedding: Vec<f32>,
}

#[async_trait::async_trait]
pub trait EmbeddedMemoryStore: Send + Sync {
    async fn create_embedded(
        &self,
        memory: &Memory,
        embedding: &EmbeddedMemory,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError>;
    async fn replace_embedded(
        &self,
        memory: &Memory,
        embedding: &EmbeddedMemory,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError>;
}

/// One bounded memory item forwarded into a run's model context.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryContextUse {
    pub run_id: jarvis_domain::ids::RunId,
    pub memory_id: jarvis_domain::ids::MemoryId,
    pub rank: i32,
    pub similarity: f32,
    pub used_at: SystemTime,
}

#[async_trait::async_trait]
pub trait MemoryContextStore: Send + Sync {
    async fn record_context(
        &self,
        user_id: &jarvis_domain::ids::UserId,
        uses: &[MemoryContextUse],
    ) -> Result<(), RepositoryError>;
}

/// Provider-neutral semantic retrieval. The embedding vector is deliberately
/// an owned slice at this boundary; fastembed/pgvector types do not cross the
/// pure application crate. Implementations must apply the owner/layer filters
/// before ranking and return a bounded result set.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryHit {
    pub memory: Memory,
    pub similarity: f32,
}

#[async_trait::async_trait]
pub trait MemoryRetriever: Send + Sync {
    async fn retrieve(
        &self,
        user_id: &jarvis_domain::ids::UserId,
        layer: Option<MemoryLayer>,
        embedding: &[f32],
        limit: u32,
    ) -> Result<Vec<MemoryHit>, RepositoryError>;
}

#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("embedding provider is unavailable")]
    Unavailable,
    #[error("embedding request was cancelled")]
    Cancelled,
    #[error("embedding vector has invalid dimensions")]
    InvalidDimensions,
    #[error("embedding provider failed")]
    Failed,
}

#[async_trait::async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn model_id(&self) -> &str;
    fn dimensions(&self) -> usize;
    async fn embed(
        &self,
        text: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Vec<f32>, EmbeddingError>;
}
