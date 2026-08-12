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
    /// The credential-free media window (FR-22, ADR-012 cast-a-link).
    MediaWindow,
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
        /// The device this placement is for (F7.5). Absent means the local
        /// desktop agent, which is how every placement worked before nodes
        /// existed — so an older agent that ignores the field still behaves
        /// correctly for its own placements. Delivery is filtered on it, so a
        /// node never *sees* another node's directive regardless.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_device_id: Option<String>,
    },
    /// Open `url` in the **dedicated media window** on `monitor` (FR-22, ADR-012
    /// cast-a-link): launch it if it is not running, reuse it if it is, then
    /// place it. The window has its own app-id (`jarvis.media`), its own profile
    /// directory, and **no credentials** — it renders third-party web video and
    /// is deliberately isolated from both the shell and the browser worker's
    /// profiles (docs/02 §11a).
    ///
    /// This is the one directive that causes the agent to **launch a process**,
    /// so the constraints are part of the contract, not an implementation
    /// detail: `url` must be `https`, and the agent independently re-validates
    /// it and launches only a fixed, allowlisted browser command with the URL as
    /// a single argv element — never through a shell (docs/02 §8: "it is not a
    /// shell").
    #[serde(rename = "display.open_media_url")]
    OpenMediaUrl {
        url: String,
        monitor: String,
        /// Which screen this cast is for (M7 gate D-M7-2). Absent keeps the
        /// pre-node behaviour — every presenter — which is safe with one
        /// screen in the house and is not once there are several: the URL is
        /// carried verbatim and `media.open_url` is **R1**, so it executes
        /// without an approval and its value can be influenced by model output
        /// derived from untrusted web content.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_device_id: Option<String>,
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
    /// **Which node** should present it (F7.5, FR-19): a paired device id, or a
    /// room name from `[display].node_aliases`. Omitted means the local
    /// desktop agent, which is what every placement meant before nodes.
    ///
    /// An unknown room, an offline node, or a device that cannot present is a
    /// **visible failure** — never a silent fallback to a local surface. "Put
    /// it on the kitchen screen" must not look like it worked when the kitchen
    /// screen is unplugged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
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
    /// The device the placement was addressed to (F7.5); absent for the local
    /// desktop agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
    /// True when at least one display-agent device was connected to receive the
    /// directive; false means audited-but-undelivered (the UI can surface this).
    pub dispatched: bool,
}
