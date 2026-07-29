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

/// A standalone append-only audit write (invariant 6). Unlike the store ports
/// above — which co-transact their audit with a domain row — a display
/// placement (FR-09/10) has no other row to write: issuing the directive *is*
/// the change, so its audit event is recorded on its own. Implementations open
/// a transaction, append to the hash chain, and commit; a placement that cannot
/// be audited must not be dispatched.
#[async_trait::async_trait]
pub trait AuditLog: Send + Sync {
    async fn record(&self, audit: &AuditEvent) -> Result<(), RepositoryError>;
}

/// Delivery of a resolved display placement to connected desktop agents
/// (FR-09/10, docs/02 §8). The agent is a display-channel client, so this is a
/// best-effort, fire-and-forget broadcast: with no agent connected the directive
/// is audited-but-undelivered, which is a reportable outcome, not an error. The
/// port takes the *domain* placement; the jarvisd implementation maps it to the
/// wire `DisplayDirective` (deriving the surface's app-id) and broadcasts it.
#[async_trait::async_trait]
pub trait DisplayDirectiveSink: Send + Sync {
    /// Dispatch the placement. Returns true if at least one WS client was
    /// subscribed to receive it (the closest signal available before per-device
    /// scoped delivery lands).
    async fn dispatch(&self, placement: &jarvis_domain::display::SurfacePlacement) -> bool;
}

/// Local media transport control (FR-22, docs/02 §11a, ADR-012). The universal
/// control plane is MPRIS over the session bus, but the application layer only
/// names the capability — no D-Bus type appears here (invariant 3).
///
/// Two properties are part of the contract, not the implementation's choice:
///
/// * **Absence is not an error.** No session bus, no player, or a player that
///   vanished between snapshot and command yields a clean empty/`PlayerGone`
///   outcome — a media integration must never fail a run because nothing
///   happened to be playing.
/// * **The cap is not enforced here.** `set_volume` performs exactly what it is
///   told; whether a level is allowed is decided by
///   [`jarvis_domain::media::VolumePct::within_cap`] at the policy boundary
///   (the R1 tool / the owner-driven REST surface), so the controller stays a
///   dumb effector and the hearing-protection decision lives in one place.
#[async_trait::async_trait]
pub trait MediaController: Send + Sync {
    /// Everything currently on the bus. An empty snapshot is a successful
    /// observation.
    async fn snapshot(
        &self,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<jarvis_domain::media::MediaSnapshot, MediaError>;

    /// Apply a transport verb to a specific player.
    async fn transport(
        &self,
        player: &jarvis_domain::media::PlayerId,
        command: jarvis_domain::media::TransportCommand,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), MediaError>;

    /// Set a player's volume. The caller has already decided the level is
    /// authorized (see the trait note).
    async fn set_volume(
        &self,
        player: &jarvis_domain::media::PlayerId,
        volume: jarvis_domain::media::VolumePct,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), MediaError>;
}

/// Why a media operation could not be performed. Deliberately small and
/// content-free: no player-published text and no D-Bus error body reaches this
/// type (invariant 5 — these strings surface in captions and audit rows).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    /// The named player is no longer on the bus (it quit mid-command). A clean,
    /// user-explainable outcome — "that player is no longer running".
    #[error("that player is no longer running")]
    PlayerGone,
    /// The player is present but says it cannot do this (`CanGoNext = false`).
    #[error("the player does not support that control")]
    Unsupported,
    /// No session bus / media control disabled — the whole capability is absent.
    #[error("media control is unavailable")]
    Unavailable,
    #[error("media control was cancelled")]
    Cancelled,
    /// Anything else, already reduced to a short non-sensitive diagnostic.
    #[error("media control failed: {0}")]
    Failed(String),
}

/// Delivery of the current media state to connected clients (FR-22, docs/02
/// §11a). Like [`DisplayDirectiveSink`], this is best-effort fan-out with no
/// error channel: nobody listening is a normal state, not a failure. The
/// jarvisd implementation projects the domain snapshot into the transient
/// `media.state` WS event — it is deliberately **not** persisted (a
/// current-value readout is not timeline history, docs/05 §3).
#[async_trait::async_trait]
pub trait MediaStateSink: Send + Sync {
    async fn publish(&self, snapshot: &jarvis_domain::media::MediaSnapshot);
}

/// Opening a URL in the dedicated media window (FR-22, ADR-012 cast-a-link).
///
/// Separate from [`DisplayDirectiveSink`] because it carries a *payload* (the
/// URL) rather than only a placement, and because it is the one path that makes
/// the agent launch a process — keeping it its own port means a reader can find
/// every caller of that capability by finding this trait's users.
///
/// The implementation is best-effort fan-out to connected agents, same as a
/// placement: no agent connected ⇒ audited-but-undelivered, reported as `false`,
/// not an error.
#[async_trait::async_trait]
pub trait MediaWindowSink: Send + Sync {
    /// Dispatch "open this URL in the media window on this monitor". The caller
    /// has already validated the URL scheme and audited the request.
    async fn open_url(&self, url: &str, monitor: &jarvis_domain::display::MonitorId) -> bool;
}
