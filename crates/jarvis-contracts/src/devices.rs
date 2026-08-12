//! Device management DTOs (docs/05 §1 `/api/v1/devices`, §6.3/§6.4).
//!
//! The owner's view of every paired client and the revocation control that
//! docs/05 §6.4 promises ("immediate per-device token revocation via
//! settings"). Token hashes are **not** on this surface: a device list is a
//! management read, not a credential dump.

use jarvis_domain::ids::DeviceId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One paired device as the owner sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceDto {
    #[schemars(with = "crate::schema::UlidString")]
    pub device_id: DeviceId,
    pub name: String,
    /// `owner-ui`, `display-node`, `voice-node`, `room-node` (docs/05 §6.3).
    /// The class is what decides the scopes below — a device never names its
    /// own authority.
    pub device_class: String,
    /// The scopes this class holds, sent explicitly so clients never infer
    /// them (same rule as the pair response, docs/05 §6.1).
    pub scopes: Vec<String>,
    /// Whether this class may execute tools at all. Room satellites cannot;
    /// surfacing it here keeps the device list honest about what a node is.
    pub executes_tools: bool,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339; absent until the device has connected.
    pub last_seen_at: Option<String>,
    /// RFC 3339; present exactly when the device is revoked.
    pub revoked_at: Option<String>,
    pub revoked_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceListResponse {
    pub devices: Vec<DeviceDto>,
}

/// Body of `POST /api/v1/devices/{id}/revoke`. The reason is recorded in the
/// audit event and shown in the device list — revocation is a security event
/// worth being able to explain later.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RevokeDeviceRequest {
    pub reason: Option<String>,
}
