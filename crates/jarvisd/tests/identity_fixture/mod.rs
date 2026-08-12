//! Gateway test doubles (F7.1) shared by every integration test that needs a
//! paired device — the same `tests/<name>/mod.rs` pattern as `voice_fixture`,
//! so the doubles live entirely in test targets and never in the daemon.
//!
//! # Why this exists
//!
//! Nine integration-test files each carried their own copy-pasted
//! `FakeIdentityStore`. That is the shape of this project's most expensive
//! recurring bug — a fixture that builds its inputs *its own way* and so
//! agrees with nothing (M5 ×3, M6 gate B1). One double, kept behaviourally
//! equal to `PgIdentityStore`, means a change to the port's semantics —
//! revocation idempotency, the last-owner guard, class-derived scopes — is
//! taught in exactly one place and every caller inherits it.

#![allow(dead_code)] // each test target uses a subset of these helpers

use std::sync::Mutex;
use std::time::SystemTime;

use async_trait::async_trait;
use jarvis_application::ports::{
    IdentityStore, NodePairOutcome, RepositoryError, RevocationOutcome,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::identity::{Device, DeviceClass};
use jarvis_domain::ids::DeviceId;

/// In-memory `IdentityStore` with the same observable behaviour as the
/// Postgres one, audit rows included (tests assert on them).
#[derive(Default)]
pub struct InMemoryIdentityStore {
    devices: Mutex<Vec<Device>>,
    audits: Mutex<Vec<AuditEvent>>,
    /// Every call fails with a storage error — the "database unreachable"
    /// path (degraded start, docs/02 §12), which must fail closed rather than
    /// authenticate anyone.
    fail: bool,
}

impl InMemoryIdentityStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store whose every operation reports the identity backend as down.
    pub fn failing() -> Self {
        Self {
            fail: true,
            ..Self::default()
        }
    }

    fn guard(&self) -> Result<(), RepositoryError> {
        if self.fail {
            return Err(RepositoryError::Storage("unreachable".into()));
        }
        Ok(())
    }

    /// Seed a device directly, bypassing pairing — for tests that need a
    /// specific class present before the first request.
    pub fn with_device(self, device: Device) -> Self {
        self.devices.lock().expect("not poisoned").push(device);
        self
    }

    /// Seed a device *after* construction. Distinct from [`Self::with_device`]
    /// because bootstrap only opens a pairing window when the store is empty:
    /// a test that needs both a paired owner and a node must pair first and
    /// add the node second.
    pub fn add_device(&self, device: Device) {
        self.devices.lock().expect("not poisoned").push(device);
    }

    pub fn devices(&self) -> Vec<Device> {
        self.devices.lock().expect("not poisoned").clone()
    }

    /// Mark a device revoked *behind* the port — for tests that need an
    /// already-revoked device without exercising the revocation route (the
    /// "fails closed on the next request" assertion). Returns false if there
    /// is no such device.
    pub fn revoke_behind_the_port(&self, device_id: &DeviceId, at: SystemTime) -> bool {
        let mut devices = self.devices.lock().expect("not poisoned");
        match devices.iter_mut().find(|d| &d.id == device_id) {
            Some(device) => {
                device.revoked_at = Some(at);
                true
            }
            None => false,
        }
    }

    pub fn audits(&self) -> Vec<AuditEvent> {
        self.audits.lock().expect("not poisoned").clone()
    }
}

#[async_trait]
impl IdentityStore for InMemoryIdentityStore {
    async fn device_count(&self) -> Result<u64, RepositoryError> {
        self.guard()?;
        Ok(self.devices.lock().expect("not poisoned").len() as u64)
    }

    async fn pair_device(
        &self,
        _owner_name: &str,
        device: &Device,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        self.guard()?;
        self.devices
            .lock()
            .expect("not poisoned")
            .push(device.clone());
        self.audits
            .lock()
            .expect("not poisoned")
            .push(audit.clone());
        Ok(())
    }

    async fn find_active_device_by_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<Device>, RepositoryError> {
        self.guard()?;
        Ok(self
            .devices
            .lock()
            .expect("not poisoned")
            .iter()
            .find(|d| d.token_hash == token_hash && d.is_active())
            .cloned())
    }

    async fn pair_node_device(
        &self,
        device: &Device,
        audit: &AuditEvent,
    ) -> Result<NodePairOutcome, RepositoryError> {
        self.guard()?;
        let mut devices = self.devices.lock().expect("not poisoned");
        // Same order of checks as the Postgres store: no owner ⇒ no node, and
        // a key already in use is a conflict, not a re-pair.
        let Some(owner) = devices
            .iter()
            .find(|d| d.class == DeviceClass::OwnerUi && d.is_active())
            .cloned()
        else {
            return Ok(NodePairOutcome::NoOwner);
        };
        if device.public_key.is_some()
            && devices
                .iter()
                .any(|d| d.public_key.is_some() && d.public_key == device.public_key)
        {
            return Ok(NodePairOutcome::KeyAlreadyPaired);
        }
        let mut stored = device.clone();
        stored.user_id = owner.user_id.clone();
        devices.push(stored);
        self.audits
            .lock()
            .expect("not poisoned")
            .push(audit.clone());
        Ok(NodePairOutcome::Paired)
    }

    async fn is_device_active(&self, device_id: &DeviceId) -> Result<bool, RepositoryError> {
        self.guard()?;
        Ok(self
            .devices
            .lock()
            .expect("not poisoned")
            .iter()
            .any(|d| &d.id == device_id && d.is_active()))
    }

    async fn list_devices(&self) -> Result<Vec<Device>, RepositoryError> {
        self.guard()?;
        let mut devices = self.devices.lock().expect("not poisoned").clone();
        // Same order as `ORDER BY created_at, id` in Postgres. Insertion order
        // would agree with it only by luck: `device()` stamps every fixture
        // device with the same `created_at`, so the tiebreak is the ULID, whose
        // low bits are random within a millisecond — the double and production
        // would disagree *nondeterministically* (rust-reviewer, F7.1).
        devices.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(devices)
    }

    async fn revoke_device(
        &self,
        device_id: &DeviceId,
        reason: Option<&str>,
        revoked_at: SystemTime,
        audit: &AuditEvent,
    ) -> Result<RevocationOutcome, RepositoryError> {
        self.guard()?;
        let mut devices = self.devices.lock().expect("not poisoned");
        let Some(index) = devices.iter().position(|d| &d.id == device_id) else {
            return Ok(RevocationOutcome::NotFound);
        };
        if !devices[index].is_active() {
            return Ok(RevocationOutcome::AlreadyRevoked);
        }
        if devices[index].class == DeviceClass::OwnerUi {
            // The SAME predicate the Postgres store uses — the point of this
            // double is that its behaviour cannot drift from production's.
            let active_owners: Vec<_> = devices
                .iter()
                .filter(|d| d.class == DeviceClass::OwnerUi && d.is_active())
                .map(|d| d.id.clone())
                .collect();
            if jarvis_domain::identity::revoking_would_orphan_the_owner(&active_owners, device_id) {
                return Ok(RevocationOutcome::LastOwnerDevice);
            }
        }
        devices[index].revoked_at = Some(revoked_at);
        devices[index].revoked_reason = reason.map(ToOwned::to_owned);
        self.audits
            .lock()
            .expect("not poisoned")
            .push(audit.clone());
        Ok(RevocationOutcome::Revoked)
    }
}

/// A paired device of `class` whose token hashes to `token_hash` — the shape
/// every gateway test needs and none should have to spell out.
pub fn device(name: &str, class: DeviceClass, token_hash: &str) -> Device {
    Device {
        id: jarvisd::auth::fresh_id(),
        user_id: jarvisd::auth::fresh_id(),
        name: name.to_owned(),
        token_hash: token_hash.to_owned(),
        public_key: None,
        class,
        created_at: SystemTime::UNIX_EPOCH,
        last_seen_at: None,
        revoked_at: None,
        revoked_reason: None,
    }
}
