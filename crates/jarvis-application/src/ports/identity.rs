use super::shared::RepositoryError;
use jarvis_domain::audit::AuditEvent;

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
    /// The device with this id, if it exists and has not been revoked.
    ///
    /// Used at automation fire time (F8.6): authority is resolved from the live
    /// row every time, so a revoked device's automations fail closed rather
    /// than acting forever on a decision made when they were created.
    async fn find_active_device_by_id(
        &self,
        id: &jarvis_domain::ids::DeviceId,
    ) -> Result<Option<jarvis_domain::identity::Device>, RepositoryError>;

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
