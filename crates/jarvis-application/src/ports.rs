//! Ports (docs/02 §3): traits the outer layers implement. The application
//! layer names capabilities; infra provides them. No sqlx/axum/provider
//! types may appear here (CLAUDE.md invariant 3, enforced by arch-test).

use jarvis_domain::artifact::{ArtifactManifest, ArtifactVersion};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::conversations::{Message, Session};
use jarvis_domain::grants::Sha256;
use jarvis_domain::ids::{ArtifactId, RunId, SessionId};
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

/// Why a content-addressed blob operation failed (docs/04 §1, ADR-008).
/// Integrity is a first-class outcome: a blob whose bytes no longer hash to its
/// key is corruption, reported distinctly from a plain I/O fault so a caller
/// never silently receives wrong bytes.
#[derive(Debug, thiserror::Error)]
pub enum BlobStoreError {
    #[error("blob store I/O failure: {0}")]
    Io(String),
    /// A blob read back from the store did not hash to the key it was stored
    /// under — on-disk corruption or tampering. Fail closed; never return the
    /// bytes.
    #[error("blob integrity check failed: content does not match its address")]
    IntegrityMismatch,
}

/// Content-addressed blob store for artifact bytes (docs/04 §1, ADR-008). Blobs
/// are keyed by their SHA-256: **write-once** (storing identical bytes twice is
/// a no-op and yields the same key) and **verify-on-read** (the bytes are
/// re-hashed on the way out; a mismatch is [`BlobStoreError::IntegrityMismatch`],
/// never a silent wrong read). The blob store holds no manifest metadata — that
/// is [`ArtifactStore`]'s job; the two are joined only by the hash.
#[async_trait::async_trait]
pub trait BlobStore: Send + Sync {
    /// Store `bytes`, returning their content address. Idempotent: a second put
    /// of the same bytes changes nothing and returns the same [`Sha256`].
    async fn put(&self, bytes: &[u8]) -> Result<Sha256, BlobStoreError>;
    /// Read a blob by its address, verifying integrity on read. Unknown hash =>
    /// `Ok(None)`.
    async fn get(&self, hash: &Sha256) -> Result<Option<Vec<u8>>, BlobStoreError>;
    /// Whether a blob with this address is present (no read-back verification).
    async fn contains(&self, hash: &Sha256) -> Result<bool, BlobStoreError>;
}

/// Artifact manifest + provenance persistence (FR-08, invariant 6). A manifest
/// is immutable and a new version is a new row — the store never updates or
/// deletes a manifest (the DB enforces this too). `create_version` writes the
/// manifest, its provenance, and the given audit event in **one transaction**
/// (invariant 6): a manifest that cannot be audited is not persisted. The blob
/// named by `manifest.sha256()` is expected to already be in the [`BlobStore`];
/// this port stores only metadata.
#[async_trait::async_trait]
pub trait ArtifactStore: Send + Sync {
    /// Persist a new manifest version and its audit event atomically. A repeated
    /// (artifact_id, version) is a [`RepositoryError::Conflict`] — versions are
    /// append-only, never overwritten.
    async fn create_version(
        &self,
        manifest: &ArtifactManifest,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError>;
    /// Load one exact version's manifest. Unknown => `Ok(None)`.
    async fn get(
        &self,
        id: &ArtifactId,
        version: ArtifactVersion,
    ) -> Result<Option<ArtifactManifest>, RepositoryError>;
    /// Load the highest-versioned manifest for an artifact — what "reopen the
    /// artifact" resolves to (exit evidence #1). Unknown id => `Ok(None)`.
    async fn latest(&self, id: &ArtifactId) -> Result<Option<ArtifactManifest>, RepositoryError>;
    /// Every version of an artifact, oldest first (the version chain).
    async fn list_versions(
        &self,
        id: &ArtifactId,
    ) -> Result<Vec<ArtifactManifest>, RepositoryError>;
}
