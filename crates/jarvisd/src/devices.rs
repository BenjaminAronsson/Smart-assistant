//! Device management (docs/05 §1, §6.3/§6.4; FR-19).
//!
//! Two routes and one bus. `GET /api/v1/devices` is the owner's view of every
//! paired client; `POST /api/v1/devices/{id}/revoke` is the break-glass
//! control docs/05 §6.4 promises. Both require the `ui` class scope, which
//! **only `owner-ui` devices hold** — a room satellite can neither enumerate
//! its siblings nor revoke them.
//!
//! # Why revocation is not an approval-gated action
//!
//! docs/06 §3 puts "change access" at R3, and the M7 feature list sketched
//! revocation as an R2 approval flow. That is the right instinct for a
//! *model-proposed* action and the wrong one here: `policy::evaluate` governs
//! the path from model output to tool execution (invariant 1), and there is no
//! model in this loop. The actor is the authenticated owner operating their
//! own settings surface, the control is the one used when a device is *lost*,
//! and putting an approval card between the owner and it would add a delay to
//! the exact operation that must be immediate. So: scope-gated to `ui`,
//! audited transactionally, effective at once — and, because taking effect
//! only on the next HTTP request would leave a live socket streaming to a
//! stolen tablet, the device's WebSocket is closed too ([`RevocationBus`]).

use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use jarvis_application::ports::RevocationOutcome;
use jarvis_contracts::devices::{DeviceDto, DeviceListResponse, RevokeDeviceRequest};
use jarvis_contracts::errors::ErrorCode;
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::identity::{ClassScope, Device};
use jarvis_domain::ids::DeviceId;
use std::time::SystemTime;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::broadcast;

use crate::auth::{AuthState, DeviceContext};
use crate::problem::problem;

/// Longest revocation reason accepted. It is owner-authored free text that
/// lands in an append-only audit row and in every future device list, so it
/// is bounded and control-character-free like any other stored text.
const MAX_REASON_CHARS: usize = 200;

/// Broadcasts "this device is revoked, now" to anything holding a live
/// connection for it. In-process only, which is all a single-daemon
/// deployment needs; the durable half of the decision is the `revoked_at`
/// column, so a missed notification degrades to "closed on next request"
/// rather than to "still authorized".
#[derive(Clone)]
pub struct RevocationBus {
    tx: broadcast::Sender<DeviceId>,
}

impl Default for RevocationBus {
    fn default() -> Self {
        Self::new()
    }
}

impl RevocationBus {
    pub fn new() -> Self {
        // Small: a subscriber that lags on *revocations* has bigger problems,
        // and `Lagged` is handled by closing the socket anyway (fail closed).
        let (tx, _) = broadcast::channel(16);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DeviceId> {
        self.tx.subscribe()
    }

    /// Announce a revocation. Returns the number of live subscribers told,
    /// which is `0` in a daemon with no open sockets — not an error.
    pub fn publish(&self, device_id: &DeviceId) -> usize {
        self.tx.send(device_id.clone()).unwrap_or(0)
    }
}

/// `GET /api/v1/devices` — every paired device, revoked ones included.
#[tracing::instrument(skip_all, fields(device_id = %caller.device_id))]
pub async fn list(
    State(auth): State<AuthState>,
    Extension(caller): Extension<DeviceContext>,
) -> Result<Json<DeviceListResponse>, Response> {
    if !holds_ui(&caller) {
        return Err(ui_scope_required(&caller));
    }
    let devices = auth.identity().list_devices().await.map_err(|e| {
        tracing::error!(error = %e, "device list failed");
        problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::ProviderUnavailable,
            "identity store unavailable",
            None,
        )
    })?;
    Ok(Json(DeviceListResponse {
        devices: devices.iter().map(to_dto).collect(),
    }))
}

/// `POST /api/v1/devices/{id}/revoke` — immediate, idempotent, audited.
#[tracing::instrument(skip_all, fields(device_id = %caller.device_id, target = %id))]
pub async fn revoke(
    State(auth): State<AuthState>,
    Extension(caller): Extension<DeviceContext>,
    Path(id): Path<String>,
    body: Option<Json<RevokeDeviceRequest>>,
) -> Result<Json<DeviceDto>, Response> {
    if !holds_ui(&caller) {
        return Err(ui_scope_required(&caller));
    }
    let target: DeviceId = id.parse().map_err(|_| {
        problem(
            StatusCode::BAD_REQUEST,
            ErrorCode::ValidationFailed,
            "device id is not a ULID",
            None,
        )
    })?;
    let reason = body
        .and_then(|Json(b)| b.reason)
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty());
    if let Some(reason) = &reason {
        if reason.chars().count() > MAX_REASON_CHARS {
            return Err(problem(
                StatusCode::BAD_REQUEST,
                ErrorCode::ValidationFailed,
                "reason is too long",
                Some(format!("at most {MAX_REASON_CHARS} characters")),
            ));
        }
        if reason.chars().any(char::is_control) {
            return Err(problem(
                StatusCode::BAD_REQUEST,
                ErrorCode::ValidationFailed,
                "reason must not contain control characters",
                None,
            ));
        }
    }

    let now = SystemTime::now();
    let audit = AuditEvent {
        occurred_at: now,
        actor: format!("device:{}", caller.device_id),
        event_type: "device.revoked".into(),
        target: format!("device:{target}"),
        correlation_id: None,
        payload_json: serde_json::json!({
            "reason": reason,
            "selfRevocation": target == caller.device_id,
        })
        .to_string(),
    };

    let outcome = auth
        .identity()
        .revoke_device(&target, reason.as_deref(), now, &audit)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "device revocation failed");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "identity store unavailable",
                None,
            )
        })?;

    match outcome {
        RevocationOutcome::Revoked => {
            // Close whatever this device has open. Durability lives in the
            // committed `revoked_at`; this is what makes it *immediate*.
            let told = auth.revocations().publish(&target);
            tracing::info!(target = %target, sockets_closed = told, "device revoked");
        }
        RevocationOutcome::AlreadyRevoked => {
            tracing::info!(target = %target, "device already revoked — idempotent");
        }
        RevocationOutcome::NotFound => {
            return Err(problem(
                StatusCode::NOT_FOUND,
                ErrorCode::ResourceNotFound,
                "no such device",
                None,
            ));
        }
        RevocationOutcome::LastOwnerDevice => {
            return Err(problem(
                StatusCode::CONFLICT,
                ErrorCode::IdentityLastOwnerDevice,
                "cannot revoke the last owner device",
                Some(
                    "pair a replacement owner device first — revoking this one would leave \
                     nothing able to pair, short of restarting jarvisd"
                        .into(),
                ),
            ));
        }
    }

    // Read back rather than synthesizing: the response shows what is stored.
    let devices = auth.identity().list_devices().await.map_err(|e| {
        tracing::error!(error = %e, "device read-back failed");
        problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::ProviderUnavailable,
            "identity store unavailable",
            None,
        )
    })?;
    devices
        .iter()
        .find(|d| d.id == target)
        .map(|d| Json(to_dto(d)))
        .ok_or_else(|| {
            problem(
                StatusCode::NOT_FOUND,
                ErrorCode::ResourceNotFound,
                "no such device",
                None,
            )
        })
}

/// Device management is the owner's alone (docs/05 §6.3). This is the first
/// route in the tree to enforce a scope at the HTTP boundary — `policy::evaluate`
/// covers tool execution, and `auth.scope_missing` has been in the registry
/// since M0 waiting for a caller.
///
/// Returns `bool` rather than `Result<(), Response>` for the reason
/// [`crate::lists::IdFault`] exists: an axum `Response` is large, and putting
/// one in a helper's `Err` makes every caller's result enormous (clippy
/// `result_large_err`).
fn holds_ui(caller: &DeviceContext) -> bool {
    caller.holds(ClassScope::Ui.as_str())
}

fn ui_scope_required(caller: &DeviceContext) -> Response {
    tracing::warn!(
        device_id = %caller.device_id,
        class = %caller.class,
        "device management refused: device lacks the `ui` scope"
    );
    problem(
        StatusCode::FORBIDDEN,
        ErrorCode::AuthScopeMissing,
        "device management requires the `ui` scope",
        None,
    )
}

fn to_dto(device: &Device) -> DeviceDto {
    DeviceDto {
        device_id: device.id.clone(),
        name: device.name.clone(),
        device_class: device.class.as_str().to_owned(),
        scopes: device.effective_scopes(),
        executes_tools: device.class.executes_tools(),
        created_at: rfc3339(device.created_at),
        last_seen_at: device.last_seen_at.map(rfc3339),
        revoked_at: device.revoked_at.map(rfc3339),
        revoked_reason: device.revoked_reason.clone(),
    }
}

fn rfc3339(at: SystemTime) -> String {
    OffsetDateTime::from(at)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}
