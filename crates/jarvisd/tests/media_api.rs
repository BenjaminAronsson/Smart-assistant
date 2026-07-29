//! F3a.7: the media surface through the production router (FR-22, docs/02 §11a)
//! — **exit evidence #4**, "pause whatever is playing from the media bar".
//!
//! A fake `MediaController` + fake audit log drive the full middleware path.
//! Covered: pause-the-active-player end to end, audit-before-effect (fail closed
//! when audit fails), the volume cap holding on the owner-driven surface,
//! ambiguity refused rather than guessed, a vanished player, an unsupported
//! control, unavailable media reported as `available: false` rather than an
//! error, and auth required.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use jarvis_application::ports::{
    AuditLog, IdentityStore, MediaController, MediaError, RepositoryError,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::identity::Device;
use jarvis_domain::media::{
    MPRIS_NAME_PREFIX, MediaSnapshot, PlaybackStatus, PlayerId, PlayerState, TrackMetadata,
    TransportCommand, VolumePct,
};
use jarvisd::api::{AppState, Wiring, router_with};
use jarvisd::auth::AuthState;
use jarvisd::media::MediaApi;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

// --- fakes --------------------------------------------------------------

#[derive(Default)]
struct FakeIdentityStore {
    devices: Mutex<Vec<Device>>,
}

#[async_trait::async_trait]
impl IdentityStore for FakeIdentityStore {
    async fn device_count(&self) -> Result<u64, RepositoryError> {
        Ok(self.devices.lock().unwrap().len() as u64)
    }
    async fn pair_device(
        &self,
        _owner_name: &str,
        device: &Device,
        _audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        self.devices.lock().unwrap().push(device.clone());
        Ok(())
    }
    async fn find_active_device_by_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<Device>, RepositoryError> {
        Ok(self
            .devices
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.token_hash == token_hash && d.is_active())
            .cloned())
    }
}

#[derive(Default)]
struct FakeAuditLog {
    events: Mutex<Vec<AuditEvent>>,
    fail: bool,
}

#[async_trait::async_trait]
impl AuditLog for FakeAuditLog {
    async fn record(&self, audit: &AuditEvent) -> Result<(), RepositoryError> {
        if self.fail {
            return Err(RepositoryError::Storage("audit forced failure".into()));
        }
        self.events.lock().unwrap().push(audit.clone());
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Applied {
    Transport(String, TransportCommand),
    Volume(String, u8),
}

struct FakeController {
    snapshot: Result<MediaSnapshot, MediaError>,
    apply_error: Option<MediaError>,
    applied: Mutex<Vec<Applied>>,
}

impl FakeController {
    fn showing(snapshot: MediaSnapshot) -> Self {
        Self {
            snapshot: Ok(snapshot),
            apply_error: None,
            applied: Mutex::new(Vec::new()),
        }
    }
    fn unavailable() -> Self {
        Self {
            snapshot: Err(MediaError::Unavailable),
            apply_error: None,
            applied: Mutex::new(Vec::new()),
        }
    }
    fn failing(snapshot: MediaSnapshot, error: MediaError) -> Self {
        Self {
            snapshot: Ok(snapshot),
            apply_error: Some(error),
            applied: Mutex::new(Vec::new()),
        }
    }
    fn applied(&self) -> Vec<Applied> {
        std::mem::take(&mut self.applied.lock().unwrap())
    }
}

#[async_trait::async_trait]
impl MediaController for FakeController {
    async fn snapshot(&self, _cancel: CancellationToken) -> Result<MediaSnapshot, MediaError> {
        self.snapshot.clone()
    }
    async fn transport(
        &self,
        player: &PlayerId,
        command: TransportCommand,
        _cancel: CancellationToken,
    ) -> Result<(), MediaError> {
        if let Some(e) = &self.apply_error {
            return Err(e.clone());
        }
        self.applied
            .lock()
            .unwrap()
            .push(Applied::Transport(player.to_string(), command));
        Ok(())
    }
    async fn set_volume(
        &self,
        player: &PlayerId,
        volume: VolumePct,
        _cancel: CancellationToken,
    ) -> Result<(), MediaError> {
        if let Some(e) = &self.apply_error {
            return Err(e.clone());
        }
        self.applied
            .lock()
            .unwrap()
            .push(Applied::Volume(player.to_string(), volume.get()));
        Ok(())
    }
}

// --- fixtures -----------------------------------------------------------

fn player(name: &str) -> PlayerId {
    PlayerId::new(format!("{MPRIS_NAME_PREFIX}{name}")).unwrap()
}

fn state(name: &str, identity: &str, status: PlaybackStatus) -> PlayerState {
    PlayerState::new(
        player(name),
        Some(identity),
        status,
        TrackMetadata::sanitized(Some("Dancing Queen"), Some("ABBA"), None, None, None),
        Some(VolumePct::new(40).unwrap()),
    )
}

fn one_playing() -> MediaSnapshot {
    MediaSnapshot::new([state("spotify", "Spotify", PlaybackStatus::Playing)])
}

fn two_playing() -> MediaSnapshot {
    MediaSnapshot::new([
        state("spotify", "Spotify", PlaybackStatus::Playing),
        state("chromium", "Chromium", PlaybackStatus::Playing),
    ])
}

// --- harness ------------------------------------------------------------

struct Harness {
    app: Router,
    token: String,
    audit: Arc<FakeAuditLog>,
    controller: Arc<FakeController>,
}

async fn harness(controller: FakeController, audit: FakeAuditLog) -> Harness {
    let identity = Arc::new(FakeIdentityStore::default());
    let auth = AuthState::bootstrap(identity).await;
    let code = auth.current_pairing_code().unwrap();

    let controller = Arc::new(controller);
    let audit = Arc::new(audit);
    let media = MediaApi::new(
        controller.clone(),
        audit.clone(),
        VolumePct::new(70).unwrap(),
    );

    let app = router_with(
        AppState::new().with_auth(auth),
        Wiring {
            media: Some(media),
            ..Wiring::default()
        },
    );
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/pair")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"pairingCode":"{code}","deviceName":"laptop"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let token = body["deviceToken"].as_str().unwrap().to_owned();

    Harness {
        app,
        token,
        audit,
        controller,
    }
}

impl Harness {
    async fn command(&self, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::post("/api/v1/media/command")
                    .header(header::AUTHORIZATION, format!("Bearer {}", self.token))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn get_state(&self) -> (StatusCode, serde_json::Value) {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::get("/api/v1/media/state")
                    .header(header::AUTHORIZATION, format!("Bearer {}", self.token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }
}

// --- tests --------------------------------------------------------------

#[tokio::test]
async fn pause_from_the_media_bar_pauses_the_active_player() {
    // Exit evidence #4, end to end through the production router: the bar sends
    // `pause` with no player named, and the one playing player is paused.
    let h = harness(
        FakeController::showing(one_playing()),
        FakeAuditLog::default(),
    )
    .await;

    let (status, body) = h.command(serde_json::json!({ "command": "pause" })).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["command"], "pause");
    assert_eq!(body["player"], "org.mpris.MediaPlayer2.spotify");
    assert_eq!(
        h.controller.applied(),
        vec![Applied::Transport(
            "org.mpris.MediaPlayer2.spotify".into(),
            TransportCommand::Pause
        )]
    );
    // The response carries fresh state so the bar re-renders without waiting
    // for the D-Bus signal.
    assert_eq!(body["state"]["players"][0]["identity"], "Spotify");
    assert_eq!(body["state"]["maxVolumePct"], 70);
}

#[tokio::test]
async fn a_command_is_audited_before_it_is_applied() {
    let h = harness(
        FakeController::showing(one_playing()),
        FakeAuditLog::default(),
    )
    .await;
    h.command(serde_json::json!({ "command": "pause" })).await;

    let events = h.audit.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.event_type, "media.command");
    assert_eq!(event.target, "player:org.mpris.MediaPlayer2.spotify");
    assert!(
        event.actor.starts_with("device:"),
        "the authenticated device is the actor, got {}",
        event.actor
    );
    let payload: serde_json::Value = serde_json::from_str(&event.payload_json).unwrap();
    assert_eq!(payload["command"], "pause");
    // Player-published track text must not be copied into the audit row.
    assert!(!event.payload_json.contains("Dancing Queen"));
}

#[tokio::test]
async fn a_command_that_cannot_be_audited_is_not_applied() {
    // Invariant 6, stricter reading (same as F3a.4 placement): no audit, no
    // effect.
    let h = harness(
        FakeController::showing(one_playing()),
        FakeAuditLog {
            fail: true,
            ..FakeAuditLog::default()
        },
    )
    .await;

    let (status, _) = h.command(serde_json::json!({ "command": "pause" })).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        h.controller.applied().is_empty(),
        "an unauditable command must have no effect"
    );
}

#[tokio::test]
async fn two_playing_players_are_refused_not_guessed() {
    let h = harness(
        FakeController::showing(two_playing()),
        FakeAuditLog::default(),
    )
    .await;

    let (status, body) = h.command(serde_json::json!({ "command": "pause" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(h.controller.applied().is_empty());

    // Naming one resolves it.
    let (status, _) = h
        .command(serde_json::json!({
            "command": "pause",
            "player": "org.mpris.MediaPlayer2.chromium"
        }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        h.controller.applied(),
        vec![Applied::Transport(
            "org.mpris.MediaPlayer2.chromium".into(),
            TransportCommand::Pause
        )]
    );
}

#[tokio::test]
async fn the_volume_cap_holds_on_the_owner_driven_surface() {
    // Hearing protection is enforced on EVERY path, not just the model's: the
    // media bar cannot exceed the cap at all (above-cap is the R2 approved
    // tool, deliberately not a UI button).
    let h = harness(
        FakeController::showing(one_playing()),
        FakeAuditLog::default(),
    )
    .await;

    let (status, body) = h
        .command(serde_json::json!({ "command": "set_volume", "volumePct": 85 }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "policy.denied");
    assert!(h.controller.applied().is_empty());
    assert!(
        h.audit.events.lock().unwrap().is_empty(),
        "a refused command is not audited as applied"
    );

    // At the cap is allowed.
    let (status, _) = h
        .command(serde_json::json!({ "command": "set_volume", "volumePct": 70 }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        h.controller.applied(),
        vec![Applied::Volume("org.mpris.MediaPlayer2.spotify".into(), 70)]
    );
}

#[tokio::test]
async fn out_of_range_and_unknown_inputs_are_client_errors() {
    let h = harness(
        FakeController::showing(one_playing()),
        FakeAuditLog::default(),
    )
    .await;

    for body in [
        serde_json::json!({ "command": "set_volume", "volumePct": 900 }),
        serde_json::json!({ "command": "set_volume" }),
        serde_json::json!({ "command": "exec" }),
        serde_json::json!({ "command": "seek" }),
        serde_json::json!({ "command": "pause", "player": "not.an.mpris.name" }),
    ] {
        let (status, _) = h.command(body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "for {body}");
    }
    assert!(h.controller.applied().is_empty());
}

#[tokio::test]
async fn a_player_that_is_not_running_is_a_404_and_a_vanished_one_a_409() {
    let h = harness(
        FakeController::showing(one_playing()),
        FakeAuditLog::default(),
    )
    .await;
    let (status, _) = h
        .command(serde_json::json!({
            "command": "pause",
            "player": "org.mpris.MediaPlayer2.vlc"
        }))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Present in the snapshot but gone by the time the command lands.
    let h = harness(
        FakeController::failing(one_playing(), MediaError::PlayerGone),
        FakeAuditLog::default(),
    )
    .await;
    let (status, _) = h.command(serde_json::json!({ "command": "pause" })).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_control_the_player_does_not_support_is_refused() {
    let no_seek = MediaSnapshot::new([state("spotify", "Spotify", PlaybackStatus::Playing)
        .with_capabilities(true, true, true, true, false)]);
    let h = harness(FakeController::showing(no_seek), FakeAuditLog::default()).await;

    let (status, _) = h
        .command(serde_json::json!({ "command": "seek", "offsetSecs": 30 }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(h.controller.applied().is_empty());
}

#[tokio::test]
async fn state_reports_players_and_the_cap() {
    let h = harness(
        FakeController::showing(one_playing()),
        FakeAuditLog::default(),
    )
    .await;
    let (status, body) = h.get_state().await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["available"], true);
    assert_eq!(
        body["state"]["activePlayer"],
        "org.mpris.MediaPlayer2.spotify"
    );
    assert_eq!(
        body["state"]["players"][0]["metadata"]["title"],
        "Dancing Queen"
    );
    assert_eq!(body["state"]["maxVolumePct"], 70);
}

#[tokio::test]
async fn unavailable_media_is_a_normal_state_not_an_error() {
    // No session bus is an ordinary desktop condition; the bar hides itself.
    // A 5xx here would make a working machine look broken.
    let h = harness(FakeController::unavailable(), FakeAuditLog::default()).await;
    let (status, body) = h.get_state().await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["available"], false);
    assert_eq!(body["state"]["players"].as_array().unwrap().len(), 0);
    assert_eq!(body["state"]["maxVolumePct"], 70);
}

#[tokio::test]
async fn the_media_surface_requires_authentication() {
    let h = harness(
        FakeController::showing(one_playing()),
        FakeAuditLog::default(),
    )
    .await;

    for request in [
        Request::get("/api/v1/media/state")
            .body(Body::empty())
            .unwrap(),
        Request::post("/api/v1/media/command")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"command":"pause"}"#))
            .unwrap(),
    ] {
        let response = h.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(h.controller.applied().is_empty());
}
