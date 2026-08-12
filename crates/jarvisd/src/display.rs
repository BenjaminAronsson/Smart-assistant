//! Display placement surface (docs/05 §1, FR-09/10): place an artifact's canvas
//! on a selected monitor (exit evidence #2). The owner drives this via
//! `POST /api/v1/artifacts/{id}/open`; the model never does — a placement is an
//! authenticated client action, not a tool the orchestrator can call
//! (invariant 1). Wire DTOs at the boundary, domain types inside.
//!
//! Flow: verify the artifact exists → resolve the target monitor (request
//! override, else the display profile; none ⇒ fail closed) → durably audit the
//! placement (invariant 6, blocking) → dispatch the directive to connected
//! agents (best-effort, fire-and-forget).

use std::sync::Arc;
use std::time::SystemTime;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::{Extension, Json};
use jarvis_application::ports::{ArtifactStore, AuditLog, DisplayDirectiveSink, RepositoryError};
use jarvis_contracts::display::{OpenArtifactRequest, OpenArtifactResponse, SurfaceDto};
use jarvis_contracts::errors::ErrorCode;
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::display::{DisplayProfile, MonitorId, Surface};
use jarvis_domain::ids::ArtifactId;

use crate::auth::DeviceContext;
use crate::problem::problem;

/// State for the display-placement route: the artifact store (existence check),
/// the configured display profile, the fallible audit writer, and the directive
/// sink (the WS hub). Cloneable so it can be axum route state.
#[derive(Clone)]
pub struct DisplayApi {
    artifacts: Arc<dyn ArtifactStore>,
    profile: Arc<DisplayProfile>,
    audit: Arc<dyn AuditLog>,
    sink: Arc<dyn DisplayDirectiveSink>,
    /// Node targeting (F7.5): room name → paired device id, the identity store
    /// that says whether that device may present, and who is connected right
    /// now. `None` in deployments wired without the device surface, where the
    /// only display is the local agent.
    nodes: Option<NodeTargets>,
}

/// What a placement needs in order to address a node honestly.
#[derive(Clone)]
pub struct NodeTargets {
    /// `[display].node_aliases`: the room names the owner actually says.
    pub aliases: std::collections::BTreeMap<String, String>,
    pub identity: Arc<dyn jarvis_application::ports::IdentityStore>,
    pub connected: crate::devices::ConnectedDevices,
    /// What each node should be showing, re-asserted when it reconnects
    /// (F7.7).
    pub surfaces: crate::devices::SurfaceState,
}

impl DisplayApi {
    pub fn new(
        artifacts: Arc<dyn ArtifactStore>,
        profile: Arc<DisplayProfile>,
        audit: Arc<dyn AuditLog>,
        sink: Arc<dyn DisplayDirectiveSink>,
    ) -> Self {
        Self {
            artifacts,
            profile,
            audit,
            sink,
            nodes: None,
        }
    }

    /// Enable node targeting (F7.5).
    pub fn with_nodes(mut self, nodes: NodeTargets) -> Self {
        self.nodes = Some(nodes);
        self
    }
}

/// Build a [`DisplayProfile`] from the `[display]` config map. Keys are surface
/// names in snake_case; an unknown surface name is a config error (fail fast
/// rather than silently ignore a typo'd assignment), as is a malformed monitor.
pub fn profile_from_config(
    map: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<DisplayProfile> {
    let mut assignments = Vec::new();
    for (surface_name, monitor) in map {
        let surface = surface_from_wire(surface_name).ok_or_else(|| {
            anyhow::anyhow!("[display].profile: unknown surface {surface_name:?}")
        })?;
        let monitor = MonitorId::new(monitor.clone())
            .map_err(|e| anyhow::anyhow!("[display].profile.{surface_name}: {e}"))?;
        assignments.push((surface, monitor));
    }
    Ok(DisplayProfile::new(assignments))
}

/// `POST /api/v1/artifacts/{id}/open` (FR-09/10). Places the artifact canvas on a
/// selected monitor. The artifact must exist (404 otherwise); the monitor is the
/// request's `display` override or the profile default, and if neither resolves
/// the request fails closed (409 — never place on an arbitrary monitor).
pub async fn open_artifact(
    State(api): State<DisplayApi>,
    Path(id): Path<String>,
    Extension(device): Extension<DeviceContext>,
    Json(req): Json<OpenArtifactRequest>,
) -> Result<Json<OpenArtifactResponse>, Response> {
    let id = id
        .parse::<ArtifactId>()
        .map_err(|_| not_found("no such artifact"))?;

    // The artifact must exist to be opened — its latest manifest is the reopen
    // target (exit evidence #1 semantics). Unknown id ⇒ 404.
    if api
        .artifacts
        .latest(&id)
        .await
        .map_err(repository_problem)?
        .is_none()
    {
        return Err(not_found("no such artifact"));
    }

    // A supplied `display` is validated as a monitor id BEFORE resolution — a
    // malformed value (empty, control chars) is a client 400, and the validation
    // also stops a newline smuggling into a Hyprland dispatch line at the agent.
    let requested = match req.display.as_deref() {
        Some(raw) => Some(MonitorId::new(raw).map_err(|_| {
            problem(
                StatusCode::BAD_REQUEST,
                ErrorCode::ValidationFailed,
                "display is not a valid monitor id",
                None,
            )
        })?),
        None => None,
    };

    // Resolve the node BEFORE auditing: an unreachable target must fail
    // visibly, and a placement nobody can present should not be recorded as
    // though it happened (F7.5).
    let target_device_id = match req.node.as_deref() {
        Some(node) => Some(resolve_node(&api, node).await?),
        None => None,
    };

    let surface = Surface::ArtifactCanvas;
    let placement = api.profile.resolve(surface, requested).ok_or_else(|| {
        problem(
            StatusCode::CONFLICT,
            ErrorCode::ResourceVersionConflict,
            "no monitor for the artifact canvas: name one via `display` or configure \
             [display].profile.artifact_canvas",
            None,
        )
    })?;

    // Durably audit BEFORE dispatch (invariant 6, stricter reading): a placement
    // that cannot be recorded must not be issued. The payload names only the
    // surface and monitor — no artifact content. The actor is the authenticated
    // device that requested the placement, so the event attributes who acted
    // (docs/04 §2 actor format) rather than an anonymous "user".
    let audit = AuditEvent {
        occurred_at: SystemTime::now(),
        actor: format!("device:{}", device.device_id),
        event_type: "display.surface_placed".to_owned(),
        target: format!("artifact:{id}"),
        correlation_id: None,
        payload_json: serde_json::json!({
            "surface": "artifact_canvas",
            "monitor": placement.monitor.as_str(),
            "targetDeviceId": target_device_id,
        })
        .to_string(),
    };
    api.audit.record(&audit).await.map_err(repository_problem)?;

    // Fire-and-forget to connected agents; a disconnected agent means the
    // directive was audited but not applied (reported via `dispatched`).
    let dispatched = api
        .sink
        .dispatch(&placement, target_device_id.as_deref())
        .await;

    // Remember what this node should be showing. Directives are transient by
    // design, so without this a node that reconnects comes back blank and
    // stays blank — the placement it missed is never coming again (F7.7).
    if let (Some(nodes), Some(target)) = (&api.nodes, target_device_id.as_deref())
        && let Ok(device_id) = target.parse::<jarvis_domain::ids::DeviceId>()
    {
        nodes.surfaces.remember(device_id, placement.clone());
    }

    Ok(Json(OpenArtifactResponse {
        artifact_id: id,
        surface: SurfaceDto::ArtifactCanvas,
        monitor: placement.monitor.as_str().to_owned(),
        target_device_id,
        dispatched,
    }))
}

/// Wire surface name (snake_case) → domain [`Surface`]. Exhaustive over the
/// closed surface set; unknown names return `None` (caller decides the error).
fn surface_from_wire(name: &str) -> Option<Surface> {
    match name {
        "conversation" => Some(Surface::Conversation),
        "run_timeline" => Some(Surface::RunTimeline),
        "approval_tray" => Some(Surface::ApprovalTray),
        "artifact_canvas" => Some(Surface::ArtifactCanvas),
        "ambient_status" => Some(Surface::AmbientStatus),
        "diagnostics" => Some(Surface::Diagnostics),
        "media_window" => Some(Surface::MediaWindow),
        _ => None,
    }
}

/// Turn what the owner said — a room name or a device id — into a device that
/// can actually present, or an honest refusal.
///
/// Every failure here is the same 409 `display.node_unavailable`, because from
/// the owner's side they are one situation ("that screen can't take it") and
/// because distinguishing "no such device" from "revoked" would answer
/// questions a caller has no business asking. The *log* keeps the distinction.
async fn resolve_node(api: &DisplayApi, node: &str) -> Result<String, Response> {
    let Some(nodes) = &api.nodes else {
        return Err(node_unavailable(
            "this deployment has no paired display nodes",
        ));
    };
    // A room alias first — that is what the owner says out loud — then a raw
    // device id for the UI, which has the list in front of it.
    let device_id = nodes
        .aliases
        .get(node)
        .cloned()
        .unwrap_or_else(|| node.to_owned());
    let Ok(device_id) = device_id.parse::<jarvis_domain::ids::DeviceId>() else {
        tracing::warn!(node, "placement named an unknown room");
        return Err(node_unavailable("no such room or device"));
    };

    let devices = nodes.identity.list_devices().await.map_err(|e| {
        tracing::error!(error = %e, "device lookup failed");
        problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::ProviderUnavailable,
            "identity store unavailable",
            None,
        )
    })?;
    let Some(device) = devices.iter().find(|d| d.id == device_id) else {
        tracing::warn!(%device_id, "placement named a device that is not paired");
        return Err(node_unavailable("no such room or device"));
    };
    if !device.is_active() {
        tracing::warn!(%device_id, "placement named a revoked device");
        return Err(node_unavailable("that device has been revoked"));
    }
    if !device
        .class
        .holds(jarvis_domain::identity::ClassScope::DisplayAgent.as_str())
    {
        tracing::warn!(%device_id, class = %device.class, "placement named a device with no screen");
        return Err(node_unavailable("that device cannot present a surface"));
    }
    if !nodes.connected.is_connected(&device_id) {
        // The honest one. A fire-and-forget directive to a disconnected screen
        // would leave the owner believing it worked.
        return Err(node_unavailable("that device is not connected"));
    }
    Ok(device_id.to_string())
}

fn node_unavailable(detail: &str) -> Response {
    problem(
        StatusCode::CONFLICT,
        ErrorCode::DisplayNodeUnavailable,
        "the named node cannot take this placement",
        Some(detail.to_owned()),
    )
}

fn not_found(what: &str) -> Response {
    problem(
        StatusCode::NOT_FOUND,
        ErrorCode::ResourceNotFound,
        what,
        None,
    )
}

fn repository_problem(error: RepositoryError) -> Response {
    match error {
        RepositoryError::Conflict(_) | RepositoryError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            ErrorCode::ResourceVersionConflict,
            "display placement conflict",
            None,
        ),
        RepositoryError::Storage(e) => {
            tracing::error!(error = %e, "display placement storage failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "storage unavailable",
                None,
            )
        }
    }
}
