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
use jarvis_application::ports::{BlobStore, BlobStoreError};
use jarvis_domain::grants::Sha256 as Address;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

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

    async fn contains(&self, address: &Address) -> Result<bool, BlobStoreError> {
        fs_exists(&self.path_for(&address.to_string())).await
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
