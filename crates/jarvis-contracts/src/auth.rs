//! v1 pairing bootstrap DTOs (docs/05 §6): one-time pairing code exchanged for
//! a device record + opaque device token. Token value appears only here on the
//! wire — stored hashed server-side, keyring client-side, never logged.

use jarvis_domain::ids::DeviceId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PairRequest {
    pub pairing_code: String,
    pub device_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PairResponse {
    #[schemars(with = "crate::schema::UlidString")]
    pub device_id: DeviceId,
    /// Opaque 256-bit bearer token; the only time it crosses the wire.
    pub device_token: String,
    /// The class the server assigned (docs/05 §6.3) — `owner-ui` for the
    /// bootstrap device. Returned explicitly for the same reason `scopes` is:
    /// a client is told its authority, it never infers it.
    pub device_class: String,
    /// Device scopes, e.g. `ui`, `display-agent`, `voice-capture` (docs/05 §6).
    pub scopes: Vec<String>,
    /// sha256 of the server certificate's DER bytes, lowercase hex (F7.3,
    /// ADR-031). The node **pins** this and refuses anything else afterwards:
    /// the certificate is self-signed, so the fingerprint delivered inside the
    /// pairing ceremony is what turns "encrypted to somebody" into "encrypted
    /// to the daemon I paired with". Absent on a plaintext loopback listener,
    /// where there is no certificate and nothing to pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_fingerprint: Option<String>,
}
