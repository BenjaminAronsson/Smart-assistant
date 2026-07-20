//! Ports (docs/02 §3): traits the outer layers implement. The application
//! layer names capabilities; infra provides them. No sqlx/axum/provider
//! types may appear here (CLAUDE.md invariant 3, enforced by arch-test).

use jarvis_domain::audit::AuditEvent;
use jarvis_domain::conversations::{Message, Session};
use jarvis_domain::ids::{RunId, SessionId};
use jarvis_domain::run::Run;
use std::time::SystemTime;

#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error("conflict: {0}")]
    Conflict(String),
    /// Same idempotency key, different payload (docs/05 §7
    /// `idempotency.conflict`).
    #[error("idempotency key reused with a different payload")]
    IdempotencyConflict,
    #[error("storage failure: {0}")]
    Storage(String),
}

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

/// A run plus its persistence timestamps — the read model behind
/// `GET /runs/{id}` (docs/05 §1). The domain [`Run`] is deliberately clock-free
/// (F1.2), so the store surfaces `created_at`/`updated_at` alongside the
/// reconstructed run rather than folding clocks into the aggregate.
#[derive(Debug, Clone, PartialEq)]
pub struct RunView {
    pub run: Run,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}

/// Run persistence (FR-01/07, NFR-05). `create` records a new run and its
/// `run.started` event in one transaction (transactional outbox, docs/02 §2);
/// `load` reads a run back for restart recovery — the durable state the
/// orchestrator resumes from (its per-transition checkpoints go through the
/// [`crate::orchestrator::Checkpointer`] port, which infra implements on the
/// same store).
#[async_trait::async_trait]
pub trait RunStore: Send + Sync {
    async fn create(&self, run: &Run) -> Result<(), RepositoryError>;
    async fn load(&self, id: &RunId) -> Result<Option<Run>, RepositoryError>;
    /// Same as [`Self::load`] but including persistence timestamps for the wire
    /// `RunDto` (docs/05 §1).
    async fn view(&self, id: &RunId) -> Result<Option<RunView>, RepositoryError>;
    /// Every run not yet in a terminal state — the restart-recovery worklist
    /// (NFR-05, docs/02 §12). The host re-drives each from its durable
    /// checkpoint; returned oldest-first so recovery order is deterministic.
    async fn load_unfinished(&self) -> Result<Vec<Run>, RepositoryError>;
}

/// Message persistence (FR-01, FR-02). Messages are immutable (docs/04 §2);
/// `append` writes the row and its `message.created` event in one transaction,
/// `list_by_session` is the timeline read (oldest first).
#[async_trait::async_trait]
pub trait MessageStore: Send + Sync {
    async fn append(&self, message: &Message) -> Result<(), RepositoryError>;
    async fn list_by_session(
        &self,
        session_id: &SessionId,
        limit: u32,
    ) -> Result<Vec<Message>, RepositoryError>;
}
