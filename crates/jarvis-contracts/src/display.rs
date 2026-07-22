//! Display-channel wire DTOs (docs/05 §1, FR-09/10).
//!
//! Two surfaces:
//!
//! * [`DisplayDirective`] — what jarvisd sends to `jarvis-agent` over the
//!   `display` channel of `/ws/v1`. A **closed** set: the agent executes exactly
//!   these narrow commands ("it is not a shell", docs/02 §8). A tag the agent
//!   does not recognize is a decode error, never a silent no-op — the same
//!   strict stance as [`crate::events::DomainEvent`], and for the same reason
//!   (producer and agent share one contract version).
//! * [`OpenArtifactRequest`] / [`OpenArtifactResponse`] — the REST body for
//!   `POST /api/v1/artifacts/{id}/open`, the owner-driven entry point that
//!   places an artifact's canvas on a selected display (exit evidence #2).
//!
//! Directives are **transient** (not replayed): they are commands, not timeline
//! events, so they ride the display channel like a text delta rides the session
//! channel. Reconnect reconciliation of pending placements is a later concern.

use jarvis_domain::ids::ArtifactId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A logical UI surface (docs/02 §8). Wire mirror of
/// `jarvis_domain::display::Surface`; jarvisd maps between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceDto {
    Conversation,
    RunTimeline,
    ApprovalTray,
    ArtifactCanvas,
    AmbientStatus,
    Diagnostics,
}

/// A directive the server sends to the agent on the `display` channel. The
/// `type` discriminator is dotted-namespaced (`display.place_surface`), matching
/// the envelope convention; the agent routes on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum DisplayDirective {
    /// Move the window for `surface` (identified by its stable Chromium app-mode
    /// `appId`) onto `monitor` (a compositor connector name, e.g. `DP-1`). The
    /// `appId` always comes from the closed server-side surface set — never from
    /// model or user text — and the agent additionally refuses any `appId`
    /// outside the `jarvis.` namespace (defense in depth).
    #[serde(rename = "display.place_surface")]
    PlaceSurface {
        surface: SurfaceDto,
        app_id: String,
        monitor: String,
    },
}

/// `POST /api/v1/artifacts/{id}/open` (FR-09/10): request that an artifact be
/// rendered on a selected display. `display` names a monitor connector; when
/// omitted, the server falls back to the display profile's `ArtifactCanvas`
/// assignment and fails closed (409) if neither resolves a monitor.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenArtifactRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

/// Response to `POST …/open`: the placement that was audited and dispatched to
/// the agent. Delivery to the agent is fire-and-forget over the display channel;
/// a disconnected agent means the directive was audited but not yet applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenArtifactResponse {
    #[schemars(with = "crate::schema::UlidString")]
    pub artifact_id: ArtifactId,
    pub surface: SurfaceDto,
    pub monitor: String,
    /// True when at least one display-agent device was connected to receive the
    /// directive; false means audited-but-undelivered (the UI can surface this).
    pub dispatched: bool,
}
