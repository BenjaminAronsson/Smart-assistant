//! Ports (docs/02 §3): traits the outer layers implement. The application
//! layer names capabilities; infra provides them. No sqlx/axum/provider
//! types may appear here (CLAUDE.md invariant 3, enforced by arch-test).

use jarvis_domain::audit::AuditEvent;
use jarvis_domain::conversations::Session;
use jarvis_domain::ids::SessionId;

#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("storage failure: {0}")]
    Storage(String),
}

/// Session persistence (FR-02). Implementations MUST write the given audit
/// event in the same transaction as the domain change (invariant 6) — a
/// session create that cannot be audited must not happen at all.
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    async fn create(&self, session: &Session, audit: &AuditEvent) -> Result<(), RepositoryError>;
    async fn get(&self, id: &SessionId) -> Result<Option<Session>, RepositoryError>;
    /// Newest first; basic listing for M0, search lands in M1+.
    async fn list(&self, limit: u32) -> Result<Vec<Session>, RepositoryError>;
}

/// Identity persistence (docs/05 §6). Pairing writes its audit event in the
/// same transaction (invariant 6); token values never cross this port —
/// hashes only.
#[async_trait::async_trait]
pub trait IdentityStore: Send + Sync {
    async fn device_count(&self) -> Result<u64, RepositoryError>;
    /// First-run pairing: creates the owner user (named `owner_name`, id
    /// `device.user_id`) + first device atomically.
    async fn pair_device(
        &self,
        owner_name: &str,
        device: &jarvis_domain::identity::Device,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError>;
    async fn find_active_device_by_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<jarvis_domain::identity::Device>, RepositoryError>;
}
