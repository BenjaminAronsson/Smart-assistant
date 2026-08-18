//! Health/diagnostics DTOs for `GET /api/v1/diagnostics/health` (docs/05 §1).
//! Unauthenticated, loopback only; must never carry secrets or prompt content.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServiceStatus {
    /// Core and all enabled adapters ready.
    Ok,
    /// Core up, one or more adapters down — degraded mode (FR-12).
    Degraded,
}

/// `disabled` = present in config but switched off (e.g. voice before M5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdapterState {
    Up,
    Down,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdapterHealth {
    pub state: AdapterState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Non-sensitive HUD presentation settings surfaced to the paired shell.
/// These are display policy, not credentials or filesystem paths (docs/09 §1,
/// docs/12 §5–§6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiSettingsDto {
    /// `none | abstract | photo` (docs/12 §5).
    pub background: String,
    /// Silent panel expiry in hours; approvals remain exempt (docs/12 §4).
    pub panel_ttl_hours: u32,
    /// `auto | reduced` (docs/12 §6).
    pub motion: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    pub status: ServiceStatus,
    /// jarvisd semver, for support/diagnostics.
    pub version: String,
    /// Adapter readiness by adapter name (docs/02 §12 startup order).
    pub adapters: BTreeMap<String, AdapterHealth>,
    /// One-time pairing code, present only while the first-run pairing
    /// window is open (docs/05 §6: shown on the health page, loopback only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing_code: Option<String>,
    /// Whether the daemon has an owner device yet.
    ///
    /// The health endpoint is the only unauthenticated surface (docs/05 §6.2),
    /// so this is the one honest answer an install script can get to "did the
    /// pairing actually work" without holding a token. It is a bare boolean
    /// precisely so it discloses nothing else: not how many devices, not which,
    /// not their classes.
    ///
    /// **Fails closed.** If the identity store cannot be read, this is `false`
    /// — the daemon does not *know* it has an owner, and a check that reported
    /// success on an unreadable database would be worse than one that failed.
    #[serde(default)]
    pub paired: bool,
    /// Optional for embedders and older test fixtures during additive rollout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui: Option<UiSettingsDto>,
}
