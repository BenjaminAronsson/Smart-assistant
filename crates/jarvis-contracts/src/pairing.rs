//! Node pairing DTOs (docs/05 §1 `/api/v1/devices/pair`, §6.5; FR-19, ADR-031).
//!
//! Three steps, three shapes. The owner opens a window; the node starts a
//! pairing attempt with its **public** key and is handed a challenge; the node
//! returns a signature over that challenge and receives its token.
//!
//! No private key material appears on this surface in any direction — the node
//! generates its keypair locally and the private half never leaves it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `POST /api/v1/devices/pairing-window` — the owner opens a window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PairingWindowDto {
    /// The one-time code the owner reads out to the node. Shown once, here;
    /// never logged, never stored in the clear.
    pub pairing_code: String,
    /// RFC 3339. A window that is not used in time closes itself.
    pub expires_at: String,
}

/// `POST /api/v1/devices/pair` — step one, from the node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodePairStartRequest {
    /// Base64 (standard, padded) Ed25519 public key, 32 bytes decoded.
    pub public_key: String,
    pub device_name: String,
    /// `display-node`, `voice-node` or `room-node`. A node **requests**; the
    /// server **assigns**. Asking for `owner-ui` is refused, never upgraded
    /// (docs/05 §6.3).
    pub requested_class: String,
    pub pairing_code: String,
}

/// The challenge to sign. Single-use, short-lived, and bound to the public key
/// that asked for it — a challenge is not a bearer object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodePairChallengeDto {
    pub challenge_id: String,
    /// Base64 (standard, padded) random bytes to sign.
    pub challenge: String,
    /// RFC 3339.
    pub expires_at: String,
}

/// `POST /api/v1/devices/pair/complete` — step two, from the node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodePairCompleteRequest {
    pub challenge_id: String,
    /// Base64 (standard, padded) Ed25519 signature over the raw challenge
    /// bytes, 64 bytes decoded.
    pub signature: String,
}
