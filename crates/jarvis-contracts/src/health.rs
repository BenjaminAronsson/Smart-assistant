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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    pub status: ServiceStatus,
    /// jarvisd semver, for support/diagnostics.
    pub version: String,
    /// Adapter readiness by adapter name (docs/02 §12 startup order).
    pub adapters: BTreeMap<String, AdapterHealth>,
}
