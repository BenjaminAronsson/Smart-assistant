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
    /// Device scopes, e.g. `ui`, `display-agent`, `voice-capture` (docs/05 §6).
    pub scopes: Vec<String>,
}
