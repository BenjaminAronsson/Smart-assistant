use super::shared::RepositoryError;
use jarvis_domain::artifact::{ArtifactManifest, ArtifactVersion};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::grants::Sha256;
use jarvis_domain::ids::ArtifactId;

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
