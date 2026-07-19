//! Identity entities (docs/04 §2, docs/05 §6). Token VALUES never appear
//! here — only their hashes; the value exists transiently at the gateway.

use crate::ids::{DeviceId, UserId};
use std::time::SystemTime;

/// A paired client device and its granted scopes.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: DeviceId,
    pub user_id: UserId,
    pub name: String,
    /// sha256 hex of the opaque bearer token (docs/05 §6).
    pub token_hash: String,
    pub scopes: Vec<String>,
    pub created_at: SystemTime,
    pub revoked_at: Option<SystemTime>,
}

impl Device {
    /// Revoked tokens fail closed on the next request (docs/05 §6).
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}
