//! Media surface (F3a.7, FR-22, docs/02 §11a, ADR-012) — **exit evidence #4**:
//! pause whatever is playing, from the media bar.
//!
//! Two entry points, both owner-driven and both authenticated:
//!
//! * `GET /api/v1/media/state` — what is playing right now. Needed because
//!   `media.state` is a *transient* WS event and therefore never replayed
//!   (docs/05 §3): a client that just connected reads this once and follows
//!   events afterwards.
//! * `POST /api/v1/media/command` — apply a transport verb. This is the human
//!   pressing a button on their own paired device, the same shape as
//!   `POST /api/v1/artifacts/{id}/open` (F3a.4): a direct client action, not a
//!   model proposal. **The model's path to the same effect is the registered
//!   `media.playback` tool through `policy::evaluate`** — this endpoint is not
//!   reachable from model output, so invariant 1 is not weakened by it.
//!
//! Two rules the handler enforces regardless of what the client sends:
//!
//! 1. **The volume cap holds here too.** `set_volume` above `[media]
//!    max_volume_pct` is refused (409) — the bar cannot exceed it at all.
//!    Raising volume above the cap is an R2 action that goes through the
//!    approval flow with `media.volume_boost`, deliberately not reachable from
//!    a UI button (docs/02 §11a: hearing protection).
//! 2. **Ambiguity is never resolved by guessing.** With two players playing and
//!    no `player` named, the request fails (409) rather than pausing whichever
//!    sorted first.
//!
//! Every applied command is durably audited **before** it is dispatched
//! (invariant 6) — a command that cannot be recorded must not happen.

use std::sync::Arc;
use std::time::SystemTime;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::{Extension, Json};
use jarvis_application::ports::{AuditLog, MediaController, MediaError, RepositoryError};
use jarvis_contracts::errors::ErrorCode;
use jarvis_contracts::media::{
    MediaCommandRequest, MediaCommandResponse, MediaStateDto, MediaStateResponse,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::media::{MediaSnapshot, PlayerId, TargetSelection, TransportCommand, VolumePct};
use tokio_util::sync::CancellationToken;

use crate::auth::DeviceContext;
use crate::problem::problem;

/// State for the media routes. Cloneable so it can be axum route state.
#[derive(Clone)]
pub struct MediaApi {
    controller: Arc<dyn MediaController>,
    audit: Arc<dyn AuditLog>,
    max_volume: VolumePct,
}

impl MediaApi {
    pub fn new(
        controller: Arc<dyn MediaController>,
        audit: Arc<dyn AuditLog>,
        max_volume: VolumePct,
    ) -> Self {
        Self {
            controller,
            audit,
            max_volume,
        }
    }
}

/// `GET /api/v1/media/state`. An unavailable controller (no session bus, media
/// disabled) is a **200 with `available: false`**, not an error: "nothing to
/// control" is a normal state the bar renders by hiding itself, and a 503 here
/// would make an ordinary desktop look broken.
pub async fn get_state(State(api): State<MediaApi>) -> Result<Json<MediaStateResponse>, Response> {
    match api.controller.snapshot(CancellationToken::new()).await {
        Ok(snapshot) => Ok(Json(MediaStateResponse {
            state: MediaStateDto::from_snapshot(&snapshot, api.max_volume),
            available: true,
        })),
        Err(MediaError::Unavailable) => Ok(Json(MediaStateResponse {
            state: MediaStateDto {
                max_volume_pct: api.max_volume.get(),
                ..MediaStateDto::default()
            },
            available: false,
        })),
        Err(e) => Err(media_problem(e)),
    }
}

/// `POST /api/v1/media/command` (exit evidence #4).
pub async fn post_command(
    State(api): State<MediaApi>,
    Extension(device): Extension<DeviceContext>,
    Json(req): Json<MediaCommandRequest>,
) -> Result<Json<MediaCommandResponse>, Response> {
    let cancel = CancellationToken::new();
    let snapshot = api
        .controller
        .snapshot(cancel.clone())
        .await
        .map_err(media_problem)?;

    // Resolve the target BEFORE deciding what to do with it: an ambiguous or
    // absent player is a client error, and nothing should be applied.
    let (player, identity) =
        resolve_target(&snapshot, req.player.as_deref()).map_err(fault_response)?;

    // `set_volume` is the one verb whose argument decides whether this surface
    // may act at all.
    if req.command == "set_volume" {
        let requested = match req.volume_pct.map(VolumePct::from_i64) {
            Some(Ok(volume)) => volume,
            Some(Err(_)) => return Err(bad_request("volumePct must be 0..=100")),
            None => return Err(bad_request("set_volume requires volumePct")),
        };
        if !requested.within_cap(api.max_volume) {
            return Err(problem(
                StatusCode::CONFLICT,
                ErrorCode::PolicyDenied,
                &format!(
                    "{requested} is above the {} volume cap; raising it further needs an \
                     approved media.volume_boost",
                    api.max_volume
                ),
                None,
            ));
        }
        audit_command(
            &api,
            &device,
            &player,
            "set_volume",
            serde_json::json!({ "volumePct": requested.get() }),
        )
        .await
        .map_err(repository_problem)?;
        api.controller
            .set_volume(&player, requested, cancel.clone())
            .await
            .map_err(media_problem)?;
        return Ok(Json(MediaCommandResponse {
            command: "set_volume".to_owned(),
            player: player.to_string(),
            state: state_after(&api, cancel, &snapshot).await,
        }));
    }

    let command = TransportCommand::parse(&req.command, req.offset_secs)
        .map_err(|e| bad_request(&e.to_string()))?;
    // Refuse what the player itself says it cannot do, rather than issuing a
    // call that silently does nothing.
    if let Some(state) = snapshot.get(&player) {
        let supported = match command {
            TransportCommand::Play => state.can_play,
            TransportCommand::Pause => state.can_pause,
            TransportCommand::Next => state.can_go_next,
            TransportCommand::Previous => state.can_go_previous,
            TransportCommand::Seek { .. } => state.can_seek,
            // Every MPRIS player accepts these; `Stop` has no capability flag
            // and `PlayPause` is covered by play/pause.
            TransportCommand::PlayPause => state.can_play || state.can_pause,
            TransportCommand::Stop => true,
        };
        if !supported {
            return Err(problem(
                StatusCode::CONFLICT,
                ErrorCode::ValidationFailed,
                &format!("{identity} does not support that control"),
                None,
            ));
        }
    }

    audit_command(
        &api,
        &device,
        &player,
        command.as_str(),
        match command {
            TransportCommand::Seek { offset_secs } => {
                serde_json::json!({ "offsetSecs": offset_secs })
            }
            _ => serde_json::Value::Null,
        },
    )
    .await
    .map_err(repository_problem)?;

    api.controller
        .transport(&player, command, cancel.clone())
        .await
        .map_err(media_problem)?;

    Ok(Json(MediaCommandResponse {
        command: command.as_str().to_owned(),
        player: player.to_string(),
        state: state_after(&api, cancel, &snapshot).await,
    }))
}

/// Re-read state after applying a command so the bar re-renders immediately
/// rather than waiting for the D-Bus signal to land. A failed re-read falls back
/// to the pre-command snapshot: the command already succeeded, and reporting a
/// slightly stale state beats failing a request that had its effect.
async fn state_after(
    api: &MediaApi,
    cancel: CancellationToken,
    before: &MediaSnapshot,
) -> MediaStateDto {
    match api.controller.snapshot(cancel).await {
        Ok(snapshot) => MediaStateDto::from_snapshot(&snapshot, api.max_volume),
        Err(e) => {
            tracing::debug!(error = %e, "post-command media snapshot failed; returning prior state");
            MediaStateDto::from_snapshot(before, api.max_volume)
        }
    }
}

/// Durable audit **before** the effect (invariant 6, same stricter reading as
/// the display placement in F3a.4). The payload names the verb and its
/// parameters only — never player-published track text.
async fn audit_command(
    api: &MediaApi,
    device: &DeviceContext,
    player: &PlayerId,
    verb: &str,
    detail: serde_json::Value,
) -> Result<(), RepositoryError> {
    let event = AuditEvent {
        occurred_at: SystemTime::now(),
        actor: format!("device:{}", device.device_id),
        event_type: "media.command".to_owned(),
        target: format!("player:{player}"),
        correlation_id: None,
        payload_json: serde_json::json!({ "command": verb, "detail": detail }).to_string(),
    };
    api.audit.record(&event).await
}

/// Resolve the target player for a request. An explicit name must be a
/// well-formed MPRIS bus name **and** present on the bus; without one, only an
/// unambiguous active player is accepted.
fn resolve_target(
    snapshot: &MediaSnapshot,
    requested: Option<&str>,
) -> Result<(PlayerId, String), MediaFault> {
    if let Some(raw) = requested {
        let id = PlayerId::new(raw).map_err(|_| MediaFault::MalformedPlayer)?;
        let state = snapshot.get(&id).ok_or(MediaFault::PlayerNotRunning)?;
        return Ok((id, state.identity.clone()));
    }
    match snapshot.target() {
        TargetSelection::One(id) => {
            let label = snapshot
                .get(&id)
                .map(|s| s.identity.clone())
                .unwrap_or_else(|| id.short_name().to_owned());
            Ok((id, label))
        }
        TargetSelection::None => Err(MediaFault::NothingPlaying),
        TargetSelection::Ambiguous(_) => Err(MediaFault::Ambiguous),
    }
}

/// Why a media request could not be resolved to an effect. A small enum rather
/// than a prebuilt `Response`: an axum `Response` is a large value, and
/// returning one in a `Result::Err` from every helper makes each of those
/// results enormous (clippy's `result_large_err`). Helpers name the *fault*;
/// [`fault_response`] renders it once at the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaFault {
    /// `player` is not a well-formed MPRIS bus name.
    MalformedPlayer,
    /// A named player is not on the bus.
    PlayerNotRunning,
    /// Nothing is playing and no player was named.
    NothingPlaying,
    /// Two or more players are active — the server never picks one.
    Ambiguous,
}

fn fault_response(fault: MediaFault) -> Response {
    match fault {
        MediaFault::MalformedPlayer => bad_request("player is not a valid MPRIS name"),
        MediaFault::PlayerNotRunning => problem(
            StatusCode::NOT_FOUND,
            ErrorCode::ResourceNotFound,
            "that player is no longer running",
            None,
        ),
        MediaFault::NothingPlaying => problem(
            StatusCode::CONFLICT,
            ErrorCode::ResourceVersionConflict,
            "nothing is playing",
            None,
        ),
        MediaFault::Ambiguous => problem(
            StatusCode::CONFLICT,
            ErrorCode::ResourceVersionConflict,
            "more than one player is active: name one in `player`",
            None,
        ),
    }
}

fn bad_request(detail: &str) -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        ErrorCode::ValidationFailed,
        detail,
        None,
    )
}

fn media_problem(error: MediaError) -> Response {
    match error {
        MediaError::PlayerGone => problem(
            StatusCode::CONFLICT,
            ErrorCode::ResourceVersionConflict,
            "that player is no longer running",
            None,
        ),
        MediaError::Unsupported => problem(
            StatusCode::CONFLICT,
            ErrorCode::ValidationFailed,
            "the player does not support that control",
            None,
        ),
        MediaError::Unavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::ProviderUnavailable,
            "media control is unavailable",
            None,
        ),
        MediaError::Cancelled => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::ProviderUnavailable,
            "media control was cancelled",
            None,
        ),
        MediaError::Failed(detail) => {
            tracing::warn!(error = %detail, "media command failed");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "media control failed",
                None,
            )
        }
    }
}

fn repository_problem(error: RepositoryError) -> Response {
    match error {
        RepositoryError::Conflict(_) | RepositoryError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            ErrorCode::ResourceVersionConflict,
            "media command conflict",
            None,
        ),
        RepositoryError::Storage(e) => {
            tracing::error!(error = %e, "media command audit failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "storage unavailable",
                None,
            )
        }
    }
}

/// Publishes media snapshots to WS clients as the transient `media.state`
/// event. Holds the cap because the wire DTO carries it (the bar clamps its own
/// slider); the hub itself stays media-agnostic.
pub struct MediaBroadcaster {
    hub: Arc<crate::ws::WsHub>,
    max_volume: VolumePct,
}

impl MediaBroadcaster {
    pub fn new(hub: Arc<crate::ws::WsHub>, max_volume: VolumePct) -> Self {
        Self { hub, max_volume }
    }
}

#[async_trait::async_trait]
impl jarvis_application::ports::MediaStateSink for MediaBroadcaster {
    async fn publish(&self, snapshot: &MediaSnapshot) {
        self.hub
            .broadcast_media_state(MediaStateDto::from_snapshot(snapshot, self.max_volume));
    }
}
