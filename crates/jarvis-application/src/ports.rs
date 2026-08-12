//! Ports (docs/02 §3): traits the outer layers implement. The application
//! layer names capabilities; infra provides them. No sqlx/axum/provider
//! types may appear here (CLAUDE.md invariant 3, enforced by arch-test).

pub use crate::calendar::{CalendarReader, CalendarReaderError};

use jarvis_domain::artifact::{ArtifactManifest, ArtifactVersion};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::conversations::{Message, Session};
use jarvis_domain::grants::Sha256;
use jarvis_domain::ids::{ArtifactId, ListId, ListItemId, RunId, SessionId, TimerId};
use jarvis_domain::lists::{ItemList, ListItem, ListName};
use jarvis_domain::memory::{Memory, MemoryLayer};
use jarvis_domain::run::Run;
use jarvis_domain::timers::{Timer, TimerState};
use std::time::SystemTime;

/// `sha256(canonical_form(arguments))` — the same normalization and hash the
/// grant minter binds (docs/06 §4).
///
/// A port rather than a function because the application layer computes no
/// crypto (invariant 3; `sha2` lives in infra). It exists to close **D-M5-4**:
/// through M5 a `tool.executed` audit row named only the tool, so the
/// append-only trail could say *that* `home.set_light` ran and never *which
/// light*. Binding the argument hash makes an executed effect answerable after
/// the fact without storing the arguments themselves, which may be sensitive
/// (invariant 5) — the same trade the grant table already makes.
pub trait ArgumentDigest: Send + Sync {
    fn digest(&self, arguments: &jarvis_domain::tools::CanonicalValue) -> Sha256;
}

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
    /// Every paired device, revoked ones included — the owner's device list
    /// (docs/05 §6.4) has to show what was revoked, not silently forget it.
    async fn list_devices(&self) -> Result<Vec<jarvis_domain::identity::Device>, RepositoryError>;
    /// Pair a **node** onto the owner's existing user (F7.2, FR-19). Distinct
    /// from [`Self::pair_device`], which bootstraps the owner and creates the
    /// user row: a satellite joins an owner who already exists, and fails
    /// closed if none does — nothing should be pairable to a house with no
    /// owner in it.
    async fn pair_node_device(
        &self,
        device: &jarvis_domain::identity::Device,
        audit: &AuditEvent,
    ) -> Result<NodePairOutcome, RepositoryError>;
    /// Record that a device was seen (F7.4). Called when a socket opens, so
    /// the owner's device list can distinguish "paired" from "actually here".
    /// Best-effort by nature: a failure must not refuse the connection, so the
    /// caller logs and continues.
    async fn touch_last_seen(
        &self,
        device_id: &jarvis_domain::ids::DeviceId,
        at: std::time::SystemTime,
    ) -> Result<(), RepositoryError>;
    /// Is this device still active? The WebSocket upgrade re-asks after
    /// subscribing to the revocation bus, because a socket authorizes once and
    /// then holds that authority for its lifetime: without this read, a
    /// revocation landing between authorization and subscription is lost
    /// entirely. Unknown device ⇒ `false` (fail closed).
    async fn is_device_active(
        &self,
        device_id: &jarvis_domain::ids::DeviceId,
    ) -> Result<bool, RepositoryError>;
    /// Revoke one device, writing `audit` in the same transaction
    /// (invariant 6). Idempotent: revoking an already-revoked device reports
    /// [`RevocationOutcome::AlreadyRevoked`] and writes no second audit row.
    ///
    /// The last-owner-device guard is evaluated **inside** the transaction —
    /// two concurrent revocations must not be able to remove the owner's last
    /// way in between them.
    async fn revoke_device(
        &self,
        device_id: &jarvis_domain::ids::DeviceId,
        reason: Option<&str>,
        revoked_at: std::time::SystemTime,
        audit: &AuditEvent,
    ) -> Result<RevocationOutcome, RepositoryError>;
}

/// What a node-pairing attempt did (F7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodePairOutcome {
    Paired,
    /// No owner device exists yet, so there is no user to attach the node to.
    NoOwner,
    /// Another device already holds this public key. Re-presenting a key is
    /// not a re-pair: the existing device is revoked first, deliberately.
    KeyAlreadyPaired,
}

/// What a revocation attempt did (docs/05 §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevocationOutcome {
    Revoked,
    /// Already revoked — the caller's intent already holds.
    AlreadyRevoked,
    NotFound,
    /// Refused: this is the last active `owner-ui` device, and revoking it
    /// would leave nothing able to pair a replacement.
    LastOwnerDevice,
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
    /// The blob is larger than the caller's cap (F6.3, CF-M3a-A). Reported
    /// rather than truncated: a partial blob is not the blob, and its bytes
    /// would not hash to its address.
    #[error("blob is {len} bytes, over the {max}-byte cap")]
    TooLarge { len: u64, max: u64 },
}

/// The largest blob [`BlobStore::get`] will materialize in memory (F6.3). `get`
/// exists for **small, host-produced** blobs the caller genuinely needs whole —
/// a promoted markdown document, a patch. Anything that could be large goes
/// through [`BlobStore::open`]. The cap is here, not left to each caller,
/// because CF-M3a-A was precisely an unbounded read nobody had thought about.
pub const MAX_INLINE_BLOB_BYTES: u64 = 1024 * 1024;

/// The largest blob the HTTP blob route will serve (F6.3, CF-M3a-A). Bounds
/// what one request can cost regardless of what a producer wrote; comfortably
/// above the generated-app bundle ceiling
/// (`jarvis_domain::appspec::MAX_BUNDLE_BYTES`).
pub const MAX_SERVED_BLOB_BYTES: u64 = 8 * 1024 * 1024;

/// The chunk size a streaming blob read emits. 64 KiB keeps per-request resident
/// memory flat on an 8 GB ultrabook (NFR-15, docs/09 §5) while staying well
/// above a syscall-per-byte regime.
pub const BLOB_CHUNK_BYTES: usize = 64 * 1024;

/// The chunk stream of a [`BlobRead`]. `Vec<u8>` rather than `bytes::Bytes`:
/// `jarvis-application` may not depend on an HTTP stack's types (invariant 3),
/// and the boundary converts once.
pub type BlobChunks =
    std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<Vec<u8>, BlobStoreError>> + Send>>;

/// An **already-verified**, streaming read of one blob (F6.3).
///
/// The integrity check has completed before this value exists — see
/// [`BlobStore::open`]. `len` is therefore trustworthy and is what the caller
/// puts in `Content-Length`.
pub struct BlobRead {
    /// Exact byte length of the verified blob.
    pub len: u64,
    /// The bytes, in [`BLOB_CHUNK_BYTES`] chunks.
    pub chunks: BlobChunks,
}

// Hand-written: a boxed stream has no `Debug`, and the bytes must not be
// rendered into a log anyway (the blob may be sensitive, invariant 5). The
// length is provenance, not content, so it stays.
impl std::fmt::Debug for BlobRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobRead")
            .field("len", &self.len)
            .field("chunks", &"<stream>")
            .finish()
    }
}

impl BlobRead {
    /// A [`BlobRead`] over bytes already in memory — for in-memory and fake
    /// blob stores, whose contents are test-sized by construction. A real
    /// file-backed store must not use this: the point of [`BlobStore::open`] is
    /// that the blob is never whole in memory.
    pub fn from_bytes(bytes: Vec<u8>) -> BlobRead {
        BlobRead {
            len: bytes.len() as u64,
            chunks: Box::pin(OneChunk(Some(bytes))),
        }
    }
}

/// A stream yielding one chunk then ending. `Option<Vec<u8>>` is `Unpin`, so
/// the projection needs no `unsafe`.
struct OneChunk(Option<Vec<u8>>);

impl futures_core::Stream for OneChunk {
    type Item = Result<Vec<u8>, BlobStoreError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Ready(std::pin::Pin::get_mut(self).0.take().map(Ok))
    }
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
    /// Read a whole blob by its address, verifying integrity on read. Unknown
    /// hash => `Ok(None)`.
    ///
    /// **Bounded** at [`MAX_INLINE_BLOB_BYTES`] (F6.3): a larger blob is
    /// [`BlobStoreError::TooLarge`], not a surprise allocation. Use this only
    /// where the caller needs the whole blob and the producer bounds its size;
    /// use [`BlobStore::open`] for anything a client can ask for by id.
    async fn get(&self, hash: &Sha256) -> Result<Option<Vec<u8>>, BlobStoreError>;

    /// Open a blob for **streaming** reading, capped at `max_bytes`
    /// (F6.3, closes CF-M3a-A). Unknown hash => `Ok(None)`.
    ///
    /// **Verify-then-emit.** Integrity is checked over the whole blob — hashing
    /// chunk by chunk, so memory stays flat — *before* the returned
    /// [`BlobRead`] yields its first byte. That is deliberately stricter than
    /// hashing while emitting: hash-while-emitting cannot fail until after the
    /// caller has already received most of the body, and an HTTP response whose
    /// bytes are already on the wire cannot be retracted. Here a corrupt blob is
    /// an error with **zero bytes emitted**, exactly like the buffered path it
    /// replaces — the only thing that changes is peak memory.
    ///
    /// Implementations must not hold the whole blob in memory at any point.
    async fn open(&self, hash: &Sha256, max_bytes: u64)
    -> Result<Option<BlobRead>, BlobStoreError>;

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

/// Lists and quick-notes persistence (FR-34, ADR-024, invariant 6). Plain rows,
/// exportable — a list is a grocery line, not an artifact (that is what
/// promotion is for).
///
/// Three properties belong to the contract rather than the implementation:
///
/// * **Every mutating method co-transacts its [`AuditEvent`]** (invariant 6): a
///   check-off that cannot be audited did not happen. Reads take no audit event
///   — "what's on the shopping list" is a query, not a side effect.
/// * **Items are addressed by [`ListItemId`], never by their text.** Two lines
///   may legitimately read the same, and the text is untrusted; the grammar
///   resolves text to an id in the domain before this port is called.
/// * **A miss is `Ok(false)`, not an error.** An item that was already removed,
///   or checked off from another device a moment earlier, is a normal outcome
///   the caller reports honestly — and then **nothing** is written, audit row
///   included.
///
/// Lists are looked up by [`Self::find_by_key`] because the grammar names a list
/// by (untrusted, case-varying) text and the normalized
/// [`jarvis_domain::lists::ListName::key`] is what uniqueness is enforced on.
/// That normalization is why the lookup takes a
/// [`jarvis_domain::lists::ListName`] and not a `&str`: a caller holding a raw
/// name would key on the wrong string and miss every list, silently.
#[async_trait::async_trait]
pub trait ListStore: Send + Sync {
    /// Create a list. A key that already exists is a
    /// [`RepositoryError::Conflict`] — the caller resolves it by loading the
    /// existing list rather than shadowing it with a second one.
    async fn create(&self, list: &ItemList, audit: &AuditEvent) -> Result<(), RepositoryError>;

    /// One list, with its items in insertion order. Unknown => `Ok(None)`.
    async fn get(&self, id: &ListId) -> Result<Option<ItemList>, RepositoryError>;

    /// Find by name, matched on its normalized
    /// [`jarvis_domain::lists::ListName::key`]. Unknown => `Ok(None)`.
    /// Implementations derive the key themselves so no caller can bypass the
    /// normalization — "Shopping", "shopping list" and "  SHOPPING  " must all
    /// find the one list.
    async fn find_by_key(&self, name: &ListName) -> Result<Option<ItemList>, RepositoryError>;

    /// Every list, name-ordered so the shell renders a stable index.
    async fn list_all(&self) -> Result<Vec<ItemList>, RepositoryError>;

    /// Append one item, ordered by `audit.occurred_at`. An unknown list, or a
    /// list already at its item bound, is a [`RepositoryError::Conflict`].
    async fn add_item(
        &self,
        list: &ListId,
        item: &ListItem,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError>;

    /// Check off / un-check one item **by id**. `Ok(false)` when that list has
    /// no such item.
    async fn set_checked(
        &self,
        list: &ListId,
        item: &ListItemId,
        checked: bool,
        audit: &AuditEvent,
    ) -> Result<bool, RepositoryError>;

    /// Remove one item by id. `Ok(false)` when it was not there.
    async fn remove_item(
        &self,
        list: &ListId,
        item: &ListItemId,
        audit: &AuditEvent,
    ) -> Result<bool, RepositoryError>;

    /// Record which artifact this list was promoted into, so a later promotion
    /// appends a **version** to that artifact instead of minting a second
    /// document for the same list. Write-once: a list that is already promoted
    /// is a [`RepositoryError::Conflict`], never a silently repointed chain.
    async fn record_promotion(
        &self,
        list: &ListId,
        artifact: &ArtifactId,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError>;
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

/// A persisted domain event bound for the transactional outbox (docs/05 §3,
/// skill `sqlx-data` §5) — written in the SAME transaction as the state change
/// it describes, then published to the WS hub by the dispatcher.
///
/// The payload is carried as **already-serialized JSON text**, for the same
/// reason [`AuditEvent`] carries its payload that way: the *wire* shape belongs
/// to `jarvis-contracts`, which neither this crate nor `jarvis-infra` may depend
/// on (invariant 3, enforced by arch-test). The host encodes; the store writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainEventRecord {
    /// Dotted envelope discriminator, e.g. `timer.fired`.
    pub event_type: String,
    /// The event payload MINUS the `type` discriminator (the envelope carries
    /// it) — matching the run/approval outbox convention.
    pub payload_json: String,
}

/// Timer/alarm/reminder persistence (FR-33, ADR-023, invariant 6). Timers must
/// survive a restart (NFR-05): one that came due while the daemon was down is
/// still in this store, still armed, and the sweep announces it as missed rather
/// than swallowing it.
///
/// Two properties belong to the contract rather than the implementation:
///
/// * **Every write co-transacts its audit row** (invariant 6). A timer that
///   cannot be audited is not stored, and a fire that cannot be audited did not
///   happen.
/// * **State changes are compare-and-set.** [`Self::apply`] moves a timer only
///   if it is still in `expected`; a lost race returns `Ok(false)`, never an
///   error and never a second write. That is what makes "a timer rings exactly
///   once" hold when the scheduler wakeup and the restart sweep overlap, or when
///   a human dismisses a timer in the instant it fires.
#[async_trait::async_trait]
pub trait TimerStore: Send + Sync {
    /// Persist a newly scheduled timer and its audit event atomically. A
    /// repeated [`TimerId`] is a [`RepositoryError::Conflict`].
    async fn create(&self, timer: &Timer, audit: &AuditEvent) -> Result<(), RepositoryError>;

    /// One timer by id. Unknown => `Ok(None)`.
    async fn get(&self, id: &TimerId) -> Result<Option<Timer>, RepositoryError>;

    /// Every timer that is not terminal — armed *or* ringing-unanswered —
    /// earliest fire time first. This is both the scheduler's worklist and the
    /// restart sweep's input, so the two can never disagree about what is
    /// outstanding.
    async fn list_live(&self) -> Result<Vec<Timer>, RepositoryError>;

    /// Compare-and-set `next`'s row from `expected` to `next.state()`, writing
    /// `audit` and (when given) `event` in the SAME transaction.
    ///
    /// `Ok(true)` = this caller made the change. `Ok(false)` = the row had
    /// already moved on, and **nothing was written** — no audit row, no event.
    async fn apply(
        &self,
        next: &Timer,
        expected: TimerState,
        audit: &AuditEvent,
        event: Option<&DomainEventRecord>,
    ) -> Result<bool, RepositoryError>;
}

/// Encodes a fired timer into its persisted wire event (FR-33). Implemented by
/// the host, which owns `jarvis-contracts`; named here because the timer use
/// case is what needs it. Kept to the one event this feature *produces* — a
/// module that emits an event it does not own would be contract drift waiting to
/// happen.
pub trait TimerEventEncoder: Send + Sync {
    /// The `timer.fired` outbox record for a timer that just went off.
    fn fired(&self, timer: &Timer, missed: bool) -> DomainEventRecord;
}

/// Why an audible alert could not be played. Deliberately content-free: no
/// device name and no player stderr reaches this type (it is logged and, in
/// `Failed`, already reduced to a short diagnostic).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AlertError {
    /// No audio path is configured or the player is missing. A normal state on a
    /// headless box — the timer still fires, it just does so silently.
    #[error("no audible alert path is available")]
    Unavailable,
    #[error("the alert was cancelled")]
    Cancelled,
    #[error("the alert failed: {0}")]
    Failed(String),
}

/// The **audible** half of a timer going off (ADR-023): a short tone on a
/// playback path that is *independent of the TTS pipeline*, so an alarm sounds
/// even when voice services are down or absent entirely.
///
/// This is deliberately not "speak this text": speaking is [`Announcer`], and
/// the two are separate ports precisely so one can be missing while the other
/// works. A failed alert never fails the fire — the timer is still marked fired,
/// still carded, and still audited.
#[async_trait::async_trait]
pub trait AlertPlayer: Send + Sync {
    async fn play(&self, cancel: tokio_util::sync::CancellationToken) -> Result<(), AlertError>;
}

/// What happened to a spoken announcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnouncementOutcome {
    Spoken,
    /// No voice pipeline (the default before M5) — reported honestly rather than
    /// pretended, because it decides whether the HUD card is the *only* notice
    /// the human gets.
    Unavailable,
}

/// The **spoken** half of a timer going off ("reminder — call Mom").
///
/// **M5 boundary.** Voice is M5 (docs/08); until then the wired implementation
/// is `jarvis_adapters::timer_alert::SilentAnnouncer`, which always answers
/// [`AnnouncementOutcome::Unavailable`]. M5 replaces that one binding with the
/// Wyoming TTS adapter and nothing else in this feature changes — that is the
/// entire seam. The audible alert above is NOT part of that seam and must keep
/// working with no voice pipeline at all.
#[async_trait::async_trait]
pub trait Announcer: Send + Sync {
    async fn announce(
        &self,
        text: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> AnnouncementOutcome;
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
