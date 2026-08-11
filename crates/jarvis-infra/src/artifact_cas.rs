//! Content-addressed blob store for artifact bytes (docs/04 §1, ADR-008,
//! FR-08). The infra side of the [`BlobStore`] port.
//!
//! Blobs live under a root directory in a two-level fan-out keyed by their
//! SHA-256: `<root>/<aa>/<bb>/<full-64-hex>`. Properties:
//!   * **content-addressed** — the key IS the hash of the bytes, so identical
//!     content dedupes automatically and a wrong address can never fetch the
//!     wrong blob;
//!   * **write-once** — a put of already-present bytes is a no-op (the address
//!     already holds exactly those bytes), so puts are idempotent and races are
//!     harmless;
//!   * **atomic** — bytes are written to a unique temp file, fsync'd, then
//!     `rename`d into place (rename is atomic within a filesystem), so a reader
//!     never sees a half-written blob and a crash mid-write leaves only a stray
//!     temp file, never a corrupt address (CF-2 durability);
//!   * **verify-on-read** — every read re-hashes the bytes and checks them
//!     against the requested address, failing closed on any mismatch
//!     ([`BlobStoreError::IntegrityMismatch`]) rather than returning tampered or
//!     corrupted content.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use jarvis_application::ports::{
    BLOB_CHUNK_BYTES, BlobRead, BlobStore, BlobStoreError, MAX_INLINE_BLOB_BYTES,
};
use jarvis_domain::grants::Sha256 as Address;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

/// A blob store rooted at a directory on the local filesystem.
pub struct FileBlobStore {
    root: PathBuf,
}

impl FileBlobStore {
    /// Create a store rooted at `root`. The directory (and per-blob
    /// subdirectories) are created lazily on first write; `root` itself need not
    /// exist yet.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `<root>/<aa>/<bb>/<full-hex>` for a given address. The two-level fan-out
    /// keeps any single directory from growing unbounded.
    fn path_for(&self, hex: &str) -> PathBuf {
        self.root.join(&hex[0..2]).join(&hex[2..4]).join(hex)
    }
}

fn io_err(context: &str, e: std::io::Error) -> BlobStoreError {
    // Stable, non-sensitive message — never interpolate a path a caller could
    // not already see (invariant #5 is about secrets, but keep messages tidy).
    BlobStoreError::Io(format!("{context}: {}", e.kind()))
}

fn hash(bytes: &[u8]) -> Address {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Address::from_bytes(hasher.finalize().into())
}

#[async_trait]
impl BlobStore for FileBlobStore {
    async fn put(&self, bytes: &[u8]) -> Result<Address, BlobStoreError> {
        let address = hash(bytes);
        let hex = address.to_string();
        let final_path = self.path_for(&hex);

        // Write-once: if the address already exists it holds exactly these bytes
        // (the address is their hash), so there is nothing to do.
        if fs_exists(&final_path).await? {
            return Ok(address);
        }

        let dir = final_path
            .parent()
            .expect("path_for always has a parent directory");
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| io_err("create blob dir", e))?;

        // Unique temp name in the SAME directory (so the rename stays within one
        // filesystem and is atomic). A process id + a random suffix avoids
        // collisions between concurrent puts of different content.
        let mut suffix = [0u8; 16];
        getrandom::fill(&mut suffix)
            .map_err(|_| BlobStoreError::Io("csprng unavailable".into()))?;
        let tmp_path = dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            hex::encode(suffix)
        ));

        // Write → flush → fsync the file, rename into place, then fsync the
        // parent directory. All three fsyncs are needed for crash durability: the
        // first makes the *bytes* durable before the rename, the last makes the
        // *directory entry* the rename created durable — without it a `put` that
        // returned Ok could vanish on a crash, orphaning a manifest that points
        // at it (closes the blob half of CF-2).
        let write_result = async {
            let mut file = tokio::fs::File::create(&tmp_path)
                .await
                .map_err(|e| io_err("create temp blob", e))?;
            file.write_all(bytes)
                .await
                .map_err(|e| io_err("write temp blob", e))?;
            file.flush()
                .await
                .map_err(|e| io_err("flush temp blob", e))?;
            file.sync_all()
                .await
                .map_err(|e| io_err("fsync temp blob", e))?;
            tokio::fs::rename(&tmp_path, &final_path)
                .await
                .map_err(|e| io_err("commit blob", e))?;
            // Make the rename itself durable.
            tokio::fs::File::open(dir)
                .await
                .map_err(|e| io_err("open blob dir for fsync", e))?
                .sync_all()
                .await
                .map_err(|e| io_err("fsync blob dir", e))
        }
        .await;

        if write_result.is_err() {
            // Best-effort cleanup of the temp file; ignore failure (a stray
            // temp file is harmless and swept later).
            let _ = tokio::fs::remove_file(&tmp_path).await;
            write_result?;
        }
        Ok(address)
    }

    async fn get(&self, address: &Address) -> Result<Option<Vec<u8>>, BlobStoreError> {
        let path = self.path_for(&address.to_string());
        // Bounded (F6.3/CF-M3a-A): stat first, refuse an oversized blob before
        // allocating for it. Checking the length after reading would already
        // have cost the memory the cap exists to prevent.
        let len = match tokio::fs::metadata(&path).await {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("stat blob", e)),
        };
        if len > MAX_INLINE_BLOB_BYTES {
            return Err(BlobStoreError::TooLarge {
                len,
                max: MAX_INLINE_BLOB_BYTES,
            });
        }
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                // Verify-on-read: the bytes must still hash to the address they
                // were stored under. A mismatch is corruption/tampering — fail
                // closed, never hand back the bytes.
                if &hash(&bytes) != address {
                    return Err(BlobStoreError::IntegrityMismatch);
                }
                Ok(Some(bytes))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err("read blob", e)),
        }
    }

    /// Verify-then-emit (F6.3): pass one hashes the file in
    /// [`BLOB_CHUNK_BYTES`] chunks and fails closed on mismatch; pass two
    /// streams the same file. Peak memory is one chunk, not one blob — which is
    /// the whole point of CF-M3a-A — and no byte reaches a caller before the
    /// address is proven.
    ///
    /// Two passes rather than one is a deliberate trade: the second read is
    /// almost entirely page-cache, whereas emitting-while-hashing would mean a
    /// corrupt blob is only discovered after most of it is already on the wire,
    /// where HTTP cannot take it back.
    async fn open(
        &self,
        address: &Address,
        max_bytes: u64,
    ) -> Result<Option<BlobRead>, BlobStoreError> {
        let path = self.path_for(&address.to_string());
        let len = match tokio::fs::metadata(&path).await {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("stat blob", e)),
        };
        if len > max_bytes {
            return Err(BlobStoreError::TooLarge {
                len,
                max: max_bytes,
            });
        }

        // Pass 1 — hash the whole file, one chunk at a time.
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("open blob", e)),
        };
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; BLOB_CHUNK_BYTES];
        let mut hashed_len: u64 = 0;
        loop {
            let n = file
                .read(&mut buf)
                .await
                .map_err(|e| io_err("verify blob", e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            hashed_len += n as u64;
            // A file growing under us would otherwise let a blob exceed the cap
            // after the stat. It cannot happen — blobs are write-once and
            // rename-committed — but the loop is the place where it would, so
            // the cap is enforced here too rather than assumed.
            if hashed_len > max_bytes {
                return Err(BlobStoreError::TooLarge {
                    len: hashed_len,
                    max: max_bytes,
                });
            }
        }
        if &Address::from_bytes(hasher.finalize().into()) != address {
            return Err(BlobStoreError::IntegrityMismatch);
        }

        // Pass 2 — stream the verified bytes. `hashed_len`, not the earlier
        // stat, is the length that was actually verified.
        let stream_file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| io_err("open blob for streaming", e))?;

        Ok(Some(BlobRead {
            len: hashed_len,
            chunks: Box::pin(BlobChunkStream {
                inner: Box::pin(ReaderStream::with_capacity(stream_file, BLOB_CHUNK_BYTES)),
            }),
        }))
    }

    async fn contains(&self, address: &Address) -> Result<bool, BlobStoreError> {
        fs_exists(&self.path_for(&address.to_string())).await
    }
}

/// Adapts a [`ReaderStream`]'s `(Bytes, io::Error)` items to the port's
/// `(Vec<u8>, BlobStoreError)` — `jarvis-application` must not speak an HTTP
/// stack's types (invariant 3), so the conversion happens here.
///
/// Hand-rolled rather than pulling in a stream-combinator crate: the whole
/// adapter is one `match`, and the dependency budget (docs/09 §5) is not worth
/// spending on it. Holding the inner stream as `Pin<Box<_>>` makes this type
/// unconditionally `Unpin`, so the projection below needs no `unsafe`.
struct BlobChunkStream {
    inner: std::pin::Pin<Box<ReaderStream<tokio::fs::File>>>,
}

impl futures_core::Stream for BlobChunkStream {
    type Item = Result<Vec<u8>, BlobStoreError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = std::pin::Pin::get_mut(self);
        match this.inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                std::task::Poll::Ready(Some(Ok(chunk.to_vec())))
            }
            std::task::Poll::Ready(Some(Err(e))) => {
                std::task::Poll::Ready(Some(Err(io_err("stream blob", e))))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

async fn fs_exists(path: &Path) -> Result<bool, BlobStoreError> {
    match tokio::fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_err("stat blob", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> PathBuf {
        let mut p = std::env::temp_dir();
        let mut suffix = [0u8; 12];
        getrandom::fill(&mut suffix).unwrap();
        p.push(format!("jarvis-cas-test-{}", hex::encode(suffix)));
        p
    }

    #[tokio::test]
    async fn put_then_get_round_trips_and_addresses_by_content() {
        let store = FileBlobStore::new(tmp_root());
        let bytes = b"# Research Notes\n\nmitochondria are the powerhouse".to_vec();

        let addr = store.put(&bytes).await.unwrap();
        // The address is exactly the sha256 of the content.
        assert_eq!(addr, hash(&bytes));

        let read = store.get(&addr).await.unwrap().unwrap();
        assert_eq!(read, bytes);
        assert!(store.contains(&addr).await.unwrap());
    }

    #[tokio::test]
    async fn put_is_idempotent_and_dedupes_identical_bytes() {
        let store = FileBlobStore::new(tmp_root());
        let bytes = b"same bytes".to_vec();

        let a1 = store.put(&bytes).await.unwrap();
        let a2 = store.put(&bytes).await.unwrap();
        assert_eq!(a1, a2, "identical content yields one address");
        assert_eq!(store.get(&a1).await.unwrap().unwrap(), bytes);
    }

    #[tokio::test]
    async fn get_unknown_address_is_none() {
        let store = FileBlobStore::new(tmp_root());
        let missing = hash(b"never stored");
        assert_eq!(store.get(&missing).await.unwrap(), None);
        assert!(!store.contains(&missing).await.unwrap());
    }

    #[tokio::test]
    async fn corrupted_blob_fails_closed_on_read() {
        let root = tmp_root();
        let store = FileBlobStore::new(&root);
        let bytes = b"trust me".to_vec();
        let addr = store.put(&bytes).await.unwrap();

        // Tamper with the on-disk bytes without changing the filename (address).
        let path = store.path_for(&addr.to_string());
        tokio::fs::write(&path, b"tampered!").await.unwrap();

        let err = store.get(&addr).await.unwrap_err();
        assert!(
            matches!(err, BlobStoreError::IntegrityMismatch),
            "a blob that no longer hashes to its address must fail closed, got {err:?}"
        );
    }

    #[tokio::test]
    async fn distinct_content_gets_distinct_addresses() {
        let store = FileBlobStore::new(tmp_root());
        let a = store.put(b"alpha").await.unwrap();
        let b = store.put(b"beta").await.unwrap();
        assert_ne!(a, b);
        assert_eq!(store.get(&a).await.unwrap().unwrap(), b"alpha");
        assert_eq!(store.get(&b).await.unwrap().unwrap(), b"beta");
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use futures_core::Stream;
    use jarvis_application::ports::{BlobChunks, MAX_SERVED_BLOB_BYTES};

    fn tmp_root() -> PathBuf {
        let mut p = std::env::temp_dir();
        let mut suffix = [0u8; 12];
        getrandom::fill(&mut suffix).unwrap();
        p.push(format!("jarvis-cas-stream-{}", hex::encode(suffix)));
        p
    }

    /// Drain a chunk stream without pulling in a combinator crate.
    async fn drain(mut chunks: BlobChunks) -> Result<Vec<u8>, BlobStoreError> {
        let mut out = Vec::new();
        std::future::poll_fn(|cx| {
            loop {
                match std::pin::Pin::new(&mut chunks).poll_next(cx) {
                    std::task::Poll::Ready(Some(Ok(chunk))) => out.extend_from_slice(&chunk),
                    std::task::Poll::Ready(Some(Err(e))) => {
                        return std::task::Poll::Ready(Err(e));
                    }
                    std::task::Poll::Ready(None) => return std::task::Poll::Ready(Ok(())),
                    std::task::Poll::Pending => return std::task::Poll::Pending,
                }
            }
        })
        .await?;
        Ok(out)
    }

    /// A blob spanning many chunks round-trips byte-for-byte, and `len` is the
    /// verified length rather than a stat the reader has to trust.
    #[tokio::test]
    async fn open_streams_a_multi_chunk_blob_byte_for_byte() {
        let store = FileBlobStore::new(tmp_root());
        // Deliberately not a chunk multiple: the last partial chunk is where an
        // off-by-one in the read loop would show up.
        let bytes: Vec<u8> = (0..(BLOB_CHUNK_BYTES * 3 + 517))
            .map(|i| (i % 251) as u8)
            .collect();
        let addr = store.put(&bytes).await.unwrap();

        let read = store
            .open(&addr, MAX_SERVED_BLOB_BYTES)
            .await
            .unwrap()
            .expect("blob is present");
        assert_eq!(read.len, bytes.len() as u64);
        assert_eq!(drain(read.chunks).await.unwrap(), bytes);
    }

    /// The headline property (CF-M3a-A + F6.3 threat note #2): verification is
    /// **complete before the first byte is emitted**, so a tampered blob larger
    /// than one chunk yields an error and *no* stream at all — never a partial
    /// body that a client could mistake for the artifact.
    #[tokio::test]
    async fn a_tampered_multi_chunk_blob_fails_before_any_byte_is_emitted() {
        let root = tmp_root();
        let store = FileBlobStore::new(&root);
        let bytes = vec![b'a'; BLOB_CHUNK_BYTES * 2 + 10];
        let addr = store.put(&bytes).await.unwrap();

        // Flip a byte in the LAST chunk: a hash-while-emitting implementation
        // would have streamed almost the whole blob before noticing.
        let mut tampered = bytes.clone();
        let last = tampered.len() - 1;
        tampered[last] = b'z';
        tokio::fs::write(store.path_for(&addr.to_string()), &tampered)
            .await
            .unwrap();

        let err = store
            .open(&addr, MAX_SERVED_BLOB_BYTES)
            .await
            .expect_err("a tampered blob must not open");
        assert!(
            matches!(err, BlobStoreError::IntegrityMismatch),
            "expected IntegrityMismatch, got {err:?}"
        );
    }

    #[tokio::test]
    async fn open_refuses_a_blob_over_the_cap_whole_rather_than_truncating() {
        let store = FileBlobStore::new(tmp_root());
        let bytes = vec![b'x'; 4096];
        let addr = store.put(&bytes).await.unwrap();

        match store.open(&addr, 1024).await {
            Err(BlobStoreError::TooLarge { len, max }) => {
                assert_eq!(len, 4096);
                assert_eq!(max, 1024);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        // Exactly at the cap is fine — the boundary is inclusive.
        let read = store.open(&addr, 4096).await.unwrap().expect("present");
        assert_eq!(read.len, 4096);
    }

    #[tokio::test]
    async fn open_of_an_unknown_address_is_none() {
        let store = FileBlobStore::new(tmp_root());
        let missing = hash(b"never stored");
        assert!(
            store
                .open(&missing, MAX_SERVED_BLOB_BYTES)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// `get` is the whole-blob read, and F6.3 bounds it too: CF-M3a-A was an
    /// unbounded read nobody had thought about, so leaving a second one behind
    /// would only move the problem.
    #[tokio::test]
    async fn get_refuses_a_blob_over_the_inline_cap() {
        let store = FileBlobStore::new(tmp_root());
        let bytes = vec![b'y'; MAX_INLINE_BLOB_BYTES as usize + 1];
        let addr = store.put(&bytes).await.unwrap();

        match store.get(&addr).await {
            Err(BlobStoreError::TooLarge { len, max }) => {
                assert_eq!(len, MAX_INLINE_BLOB_BYTES + 1);
                assert_eq!(max, MAX_INLINE_BLOB_BYTES);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        // …and the same bytes stream fine, which is the point of having both.
        let read = store
            .open(&addr, MAX_SERVED_BLOB_BYTES)
            .await
            .unwrap()
            .expect("present");
        assert_eq!(read.len, bytes.len() as u64);
    }
}
