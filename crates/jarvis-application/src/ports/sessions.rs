use super::shared::RepositoryError;
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::conversations::Session;
use jarvis_domain::ids::SessionId;

/// Result of an idempotent create (docs/05 §2, NFR-13).
#[derive(Debug, Clone, PartialEq)]
pub enum CreateOutcome {
    Created(Session),
    /// The same idempotency key already created this session with an
    /// identical payload — safe replay, no new side effect.
    AlreadyExists(Session),
}

/// Session persistence (FR-02). Implementations MUST write the given audit
/// event in the same transaction as the domain change (invariant 6) — a
/// session create that cannot be audited must not happen at all.
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    async fn create(
        &self,
        session: &Session,
        idempotency_key: Option<&str>,
        audit: &AuditEvent,
    ) -> Result<CreateOutcome, RepositoryError>;
    async fn get(&self, id: &SessionId) -> Result<Option<Session>, RepositoryError>;
    /// Newest first; basic listing for M0, search lands in M1+.
    async fn list(&self, limit: u32) -> Result<Vec<Session>, RepositoryError>;
}
