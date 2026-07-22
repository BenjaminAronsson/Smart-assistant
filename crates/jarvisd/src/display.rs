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
        }
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
        })
        .to_string(),
    };
    api.audit.record(&audit).await.map_err(repository_problem)?;

    // Fire-and-forget to connected agents; a disconnected agent means the
    // directive was audited but not applied (reported via `dispatched`).
    let dispatched = api.sink.dispatch(&placement).await;

    Ok(Json(OpenArtifactResponse {
        artifact_id: id,
        surface: SurfaceDto::ArtifactCanvas,
        monitor: placement.monitor.as_str().to_owned(),
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
        _ => None,
    }
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
