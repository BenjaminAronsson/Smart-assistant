//! Test doubles genuinely reused, byte-for-byte, across crate boundaries (F9.4).
//!
//! **What belongs here, and what does not.** Before a double moves into this
//! crate it has to be verified IDENTICAL at every call site it replaces — same
//! storage representation, same behaviour, not merely the same struct name.
//! The M9 review found several same-named "duplicates" that were not: three
//! `RecordingSink`s implementing three different port traits for three
//! different purposes, three `FakeArtifactStore`s with three different
//! dedup/failure semantics, and three `FakeBlobs`/`FakeArtifacts` pairs in
//! `jarvis-application` each keyed and stored differently (a `[u8; 32]`-keyed
//! `BTreeMap`, a `String`-keyed `HashMap`, and an unwrapped `Vec`). Forcing any
//! of those into one shared type would have silently changed what the tests
//! using them actually exercise — the exact fixture-vs-caller trap this
//! project has been bitten by before. They stay where they are.
//!
//! What genuinely was identical, verified line-for-line, and is here:
//! `FakeBlobs`/`FakeArtifacts` (both `jarvis-adapters` call sites), and
//! `FakeAuditLog` (all four call sites, modulo an optional `fail` flag two of
//! them did not need).
//!
//! Reachable only as a dev-dependency (`cargo xtask arch-test` enforces this:
//! a normal-dependency edge into this crate is rejected the same way an
//! inverted layering edge is). `jarvis-application/src/testing.rs` stays
//! exactly where it is — it is feature-gated, already used cross-crate, and
//! moving it would be scope this feature does not need.

use std::sync::Mutex;

use jarvis_application::ports::RepositoryError;
use jarvis_application::ports::{ArtifactStore, AuditLog, BlobRead, BlobStore, BlobStoreError};
use jarvis_domain::artifact::{ArtifactManifest, ArtifactVersion};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::grants::Sha256;
use jarvis_domain::ids::ArtifactId;

/// An in-memory [`BlobStore`], content-addressed by a deterministic (not
/// cryptographic) key over the first 31 bytes plus the length. Real enough for
/// a test to exercise `put`/`get`/`contains`/`open` — including the
/// `open` size-cap refusal — without a real hash implementation.
#[derive(Default)]
pub struct FakeBlobs {
    pub stored: Mutex<std::collections::BTreeMap<[u8; 32], Vec<u8>>>,
}

#[async_trait::async_trait]
impl BlobStore for FakeBlobs {
    async fn put(&self, bytes: &[u8]) -> Result<Sha256, BlobStoreError> {
        let mut key = [0u8; 32];
        for (i, b) in bytes.iter().take(31).enumerate() {
            key[i] = *b;
        }
        key[31] = bytes.len() as u8;
        self.stored.lock().unwrap().insert(key, bytes.to_vec());
        Ok(Sha256::from_bytes(key))
    }

    async fn get(&self, hash: &Sha256) -> Result<Option<Vec<u8>>, BlobStoreError> {
        Ok(self.stored.lock().unwrap().get(hash.as_bytes()).cloned())
    }

    async fn contains(&self, hash: &Sha256) -> Result<bool, BlobStoreError> {
        Ok(self.stored.lock().unwrap().contains_key(hash.as_bytes()))
    }

    async fn open(
        &self,
        hash: &Sha256,
        max_bytes: u64,
    ) -> Result<Option<BlobRead>, BlobStoreError> {
        match self.get(hash).await? {
            Some(bytes) if bytes.len() as u64 > max_bytes => Err(BlobStoreError::TooLarge {
                len: bytes.len() as u64,
                max: max_bytes,
            }),
            Some(bytes) => Ok(Some(BlobRead::from_bytes(bytes))),
            None => Ok(None),
        }
    }
}

/// An in-memory [`ArtifactStore`]. `fail`, when set, makes every
/// `create_version` call fail — a coding-worker test needs "the store is
/// down" as a distinct case from "nothing has been created yet"; the other
/// call sites simply never set it and get the always-succeeds behaviour they
/// had before.
#[derive(Default)]
pub struct FakeArtifacts {
    pub manifests: Mutex<Vec<ArtifactManifest>>,
    pub audits: Mutex<Vec<AuditEvent>>,
    pub fail: bool,
}

#[async_trait::async_trait]
impl ArtifactStore for FakeArtifacts {
    async fn create_version(
        &self,
        manifest: &ArtifactManifest,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        if self.fail {
            return Err(RepositoryError::Storage("store down".to_owned()));
        }
        // Mirror the real store: the payload is parsed as JSON before it is
        // hashed/stored (jarvis-infra audit::append). A malformed payload must
        // fail here too, so tests exercise the real constraint, not just a clone.
        serde_json::from_str::<serde_json::Value>(&audit.payload_json)
            .map_err(|e| RepositoryError::Storage(format!("bad audit payload: {e}")))?;
        self.manifests.lock().unwrap().push(manifest.clone());
        self.audits.lock().unwrap().push(audit.clone());
        Ok(())
    }

    async fn get(
        &self,
        _id: &ArtifactId,
        _version: ArtifactVersion,
    ) -> Result<Option<ArtifactManifest>, RepositoryError> {
        Ok(None)
    }

    async fn latest(&self, _id: &ArtifactId) -> Result<Option<ArtifactManifest>, RepositoryError> {
        Ok(None)
    }

    async fn list_versions(
        &self,
        _id: &ArtifactId,
    ) -> Result<Vec<ArtifactManifest>, RepositoryError> {
        Ok(self.manifests.lock().unwrap().clone())
    }
}

/// An in-memory [`AuditLog`]. `fail`, when set, makes `record` fail — three of
/// the four original call sites needed this to test an audit-write failure
/// path; the fourth (`golden11_support`) never sets it.
#[derive(Default)]
pub struct FakeAuditLog {
    pub events: Mutex<Vec<AuditEvent>>,
    pub fail: bool,
}

#[async_trait::async_trait]
impl AuditLog for FakeAuditLog {
    async fn record(&self, audit: &AuditEvent) -> Result<(), RepositoryError> {
        if self.fail {
            return Err(RepositoryError::Storage("audit forced failure".into()));
        }
        self.events.lock().unwrap().push(audit.clone());
        Ok(())
    }
}
