//! F3a.4: display placement through the production router (docs/05 §1,
//! FR-09/10) — `POST /api/v1/artifacts/{id}/open` places the artifact canvas on
//! a selected monitor (exit evidence #2). Fake stores + a capturing directive
//! sink drive the full middleware path. Covers: resolve via request override,
//! resolve via profile default, fail-closed when no monitor resolves, malformed
//! monitor rejected, unknown artifact 404, audit-before-dispatch (fail-closed if
//! audit fails), auth required, and the dispatched directive's contents.

mod identity_fixture;
use identity_fixture::InMemoryIdentityStore;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use jarvis_application::ports::{ArtifactStore, AuditLog, DisplayDirectiveSink, RepositoryError};
use jarvis_domain::artifact::{
    ArtifactContent, ArtifactKind, ArtifactManifest, ArtifactSource, ArtifactVersion,
    BuildProvenance, MediaType,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::display::{DisplayProfile, MonitorId, Surface, SurfacePlacement};
use jarvis_domain::ids::{ArtifactId, RunId};
use jarvis_domain::location::Sensitivity;
use jarvisd::api::{AppState, Wiring, router_with};
use jarvisd::auth::AuthState;
use jarvisd::display::DisplayApi;
use tower::ServiceExt;

const ARTIFACT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";

// --- fakes --------------------------------------------------------------

#[derive(Default)]
struct FakeArtifactStore {
    manifests: Mutex<Vec<ArtifactManifest>>,
}

#[async_trait::async_trait]
impl ArtifactStore for FakeArtifactStore {
    async fn create_version(
        &self,
        manifest: &ArtifactManifest,
        _audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        self.manifests.lock().unwrap().push(manifest.clone());
        Ok(())
    }
    async fn get(
        &self,
        id: &ArtifactId,
        version: ArtifactVersion,
    ) -> Result<Option<ArtifactManifest>, RepositoryError> {
        Ok(self
            .manifests
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id() == id && e.version() == version)
            .cloned())
    }
    async fn latest(&self, id: &ArtifactId) -> Result<Option<ArtifactManifest>, RepositoryError> {
        Ok(self
            .manifests
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.id() == id)
            .max_by_key(|e| e.version().get())
            .cloned())
    }
    async fn list_versions(
        &self,
        id: &ArtifactId,
    ) -> Result<Vec<ArtifactManifest>, RepositoryError> {
        Ok(self
            .manifests
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.id() == id)
            .cloned()
            .collect())
    }
}

/// Captures recorded audit events; can be told to fail to exercise the
/// fail-closed audit-before-dispatch path.
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

/// Captures dispatched placements; `connected` controls the returned bool.
struct FakeSink {
    placements: Mutex<Vec<SurfacePlacement>>,
    connected: bool,
}

#[async_trait::async_trait]
impl DisplayDirectiveSink for FakeSink {
    async fn dispatch(&self, placement: &SurfacePlacement, _target: Option<&str>) -> bool {
        self.placements.lock().unwrap().push(placement.clone());
        self.connected
    }
}

// --- harness ------------------------------------------------------------

struct Harness {
    app: Router,
    token: String,
    audit: Arc<FakeAuditLog>,
    sink: Arc<FakeSink>,
    store: Arc<FakeArtifactStore>,
}

async fn harness(profile: DisplayProfile, audit: FakeAuditLog, connected: bool) -> Harness {
    let identity = Arc::new(InMemoryIdentityStore::default());
    let auth = AuthState::bootstrap(identity).await;
    let code = auth.current_pairing_code().unwrap();

    let store = Arc::new(FakeArtifactStore::default());
    let audit = Arc::new(audit);
    let sink = Arc::new(FakeSink {
        placements: Mutex::new(Vec::new()),
        connected,
    });
    let display = DisplayApi::new(
        store.clone(),
        Arc::new(profile),
        audit.clone(),
        sink.clone(),
    );

    let app = router_with(
        AppState::new().with_auth(auth),
        Wiring {
            display: Some(display),
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
        sink,
        store,
    }
}

fn seed(store: &FakeArtifactStore) {
    let content = ArtifactContent {
        sha256: jarvis_domain::grants::Sha256::from_bytes([7u8; 32]),
        media_type: "text/markdown".parse::<MediaType>().unwrap(),
        kind: ArtifactKind::MarkdownHtml,
        sources: vec![ArtifactSource::Run(RUN.parse::<RunId>().unwrap())],
        sensitivity: Sensitivity::Normal,
        build: BuildProvenance::none(),
        capabilities: vec![],
    };
    let manifest =
        ArtifactManifest::initial(ARTIFACT.parse().unwrap(), RUN.parse().unwrap(), content);
    store.manifests.lock().unwrap().push(manifest);
}

async fn open(app: &Router, token: &str, id: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/artifacts/{id}/open"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

fn profile_with_canvas(monitor: &str) -> DisplayProfile {
    DisplayProfile::new([(Surface::ArtifactCanvas, MonitorId::new(monitor).unwrap())])
}

// --- tests --------------------------------------------------------------

/// Exit evidence #2: place the canvas on a chosen monitor. The request names
/// the monitor, it is audited, the directive is dispatched to the agent, and the
/// captured placement carries ArtifactCanvas + that monitor.
#[tokio::test]
async fn open_places_canvas_on_the_requested_monitor() {
    let h = harness(DisplayProfile::default(), FakeAuditLog::default(), true).await;
    seed(&h.store);

    let (status, body) = open(&h.app, &h.token, ARTIFACT, r#"{"display":"DP-1"}"#).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["surface"], "artifact_canvas");
    assert_eq!(body["monitor"], "DP-1");
    assert_eq!(body["dispatched"], true);

    // Audited before dispatch, payload names only surface + monitor.
    let events = h.audit.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "display.surface_placed");
    assert!(events[0].target.contains(ARTIFACT));
    // Attribution: the acting device, not an anonymous "user".
    assert!(
        events[0].actor.starts_with("device:"),
        "audit actor must name the authenticated device, got {:?}",
        events[0].actor
    );

    // The agent received a placement for the canvas on DP-1.
    let placements = h.sink.placements.lock().unwrap();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].surface, Surface::ArtifactCanvas);
    assert_eq!(placements[0].monitor.as_str(), "DP-1");
}

/// With no `display` in the request, the profile default resolves the monitor.
#[tokio::test]
async fn open_falls_back_to_the_profile_default() {
    let h = harness(profile_with_canvas("eDP-1"), FakeAuditLog::default(), true).await;
    seed(&h.store);

    let (status, body) = open(&h.app, &h.token, ARTIFACT, "{}").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["monitor"], "eDP-1");
}

/// No request monitor and no profile assignment ⇒ fail closed (409), never place
/// on an arbitrary monitor. Nothing is audited or dispatched.
#[tokio::test]
async fn open_fails_closed_when_no_monitor_resolves() {
    let h = harness(DisplayProfile::default(), FakeAuditLog::default(), true).await;
    seed(&h.store);

    let (status, _) = open(&h.app, &h.token, ARTIFACT, "{}").await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(h.audit.events.lock().unwrap().is_empty());
    assert!(h.sink.placements.lock().unwrap().is_empty());
}

/// A monitor id carrying a control character is rejected 400 — this is the guard
/// that stops a newline smuggling into a Hyprland dispatch line at the agent.
#[tokio::test]
async fn open_rejects_a_malformed_monitor() {
    let h = harness(DisplayProfile::default(), FakeAuditLog::default(), true).await;
    seed(&h.store);

    let (status, _) = open(
        &h.app,
        &h.token,
        ARTIFACT,
        "{\"display\":\"DP-1\\ndispatch exec x\"}",
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(h.sink.placements.lock().unwrap().is_empty());
}

/// Opening an artifact that does not exist is a 404, before any placement.
#[tokio::test]
async fn open_unknown_artifact_is_404() {
    let h = harness(profile_with_canvas("DP-1"), FakeAuditLog::default(), true).await;
    // no seed

    let (status, _) = open(&h.app, &h.token, ARTIFACT, r#"{"display":"DP-1"}"#).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(h.sink.placements.lock().unwrap().is_empty());
}

/// If the audit write fails, the directive is NOT dispatched (invariant 6:
/// a placement that cannot be recorded must not be issued).
#[tokio::test]
async fn open_does_not_dispatch_when_audit_fails() {
    let audit = FakeAuditLog {
        events: Mutex::new(Vec::new()),
        fail: true,
    };
    let h = harness(profile_with_canvas("DP-1"), audit, true).await;
    seed(&h.store);

    let (status, _) = open(&h.app, &h.token, ARTIFACT, "{}").await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        h.sink.placements.lock().unwrap().is_empty(),
        "no dispatch after a failed audit"
    );
}

/// A disconnected agent means audited-but-undelivered: 200 with dispatched=false.
#[tokio::test]
async fn open_reports_undelivered_when_no_agent_connected() {
    let h = harness(profile_with_canvas("DP-1"), FakeAuditLog::default(), false).await;
    seed(&h.store);

    let (status, body) = open(&h.app, &h.token, ARTIFACT, "{}").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["dispatched"], false);
    assert_eq!(h.audit.events.lock().unwrap().len(), 1, "still audited");
}

/// The endpoint is behind the bearer middleware — no token, no placement.
#[tokio::test]
async fn open_requires_auth() {
    let h = harness(profile_with_canvas("DP-1"), FakeAuditLog::default(), true).await;
    seed(&h.store);

    let response = h
        .app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/artifacts/{ARTIFACT}/open"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// --- F7.5: addressable surfaces -----------------------------------------

/// A placement can name a **node** — by room alias or device id — and every
/// way that can go wrong is a visible refusal rather than a quiet local
/// fallback. "Put it on the kitchen screen" must not look like it worked when
/// the kitchen screen is unplugged.
mod node_targeting {
    use super::*;
    use jarvis_domain::identity::DeviceClass;
    use jarvisd::devices::ConnectedDevices;
    use jarvisd::display::NodeTargets;

    const OWNER_TOKEN: &str = "owner-token";

    fn hash(token: &str) -> String {
        use sha2::Digest as _;
        hex::encode(sha2::Sha256::digest(token.as_bytes()))
    }

    struct NodeHarness {
        app: Router,
        sink: Arc<FakeSink>,
        kitchen_id: String,
        speaker_id: String,
        gone_id: String,
    }

    /// Owner + a connected kitchen screen + a connected screenless speaker +
    /// a revoked screen, with `kitchen` aliased the way an owner would say it.
    async fn node_harness() -> NodeHarness {
        let store_identity = Arc::new(InMemoryIdentityStore::new().with_device(
            identity_fixture::device("owner laptop", DeviceClass::OwnerUi, &hash(OWNER_TOKEN)),
        ));
        let kitchen = identity_fixture::device("kitchen screen", DeviceClass::RoomNode, "k");
        let speaker = identity_fixture::device("hall speaker", DeviceClass::VoiceNode, "s");
        let mut gone = identity_fixture::device("old tablet", DeviceClass::DisplayNode, "g");
        gone.revoked_at = Some(std::time::SystemTime::UNIX_EPOCH);
        let (kitchen_id, speaker_id, gone_id) = (
            kitchen.id.to_string(),
            speaker.id.to_string(),
            gone.id.to_string(),
        );
        store_identity.add_device(kitchen);
        store_identity.add_device(speaker);
        store_identity.add_device(gone);

        let connected = ConnectedDevices::new();
        // The kitchen screen and the speaker are here; the revoked tablet is not.
        std::mem::forget(connected.mark_present(kitchen_id.parse().expect("ulid")));
        std::mem::forget(connected.mark_present(speaker_id.parse().expect("ulid")));

        let artifacts = Arc::new(FakeArtifactStore::default());
        seed(&artifacts);
        let sink = Arc::new(FakeSink {
            placements: Mutex::new(Vec::new()),
            connected: true,
        });
        let display = DisplayApi::new(
            artifacts,
            Arc::new(profile_with_canvas("DP-1")),
            Arc::new(FakeAuditLog::default()),
            sink.clone(),
        )
        .with_nodes(NodeTargets {
            aliases: [("kitchen".to_owned(), kitchen_id.clone())]
                .into_iter()
                .collect(),
            identity: store_identity.clone(),
            connected,
        });

        let auth = AuthState::bootstrap(store_identity).await;
        let app = router_with(
            AppState::new().with_auth(auth),
            Wiring {
                display: Some(display),
                ..Wiring::default()
            },
        );
        NodeHarness {
            app,
            sink,
            kitchen_id,
            speaker_id,
            gone_id,
        }
    }

    async fn open_on(app: &Router, node: &str) -> (StatusCode, serde_json::Value) {
        let response = app
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/artifacts/{ARTIFACT}/open"))
                    .header(header::AUTHORIZATION, format!("Bearer {OWNER_TOKEN}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::json!({ "node": node }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn a_room_alias_places_the_surface_on_that_node() {
        let h = node_harness().await;
        let (status, body) = open_on(&h.app, "kitchen").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["targetDeviceId"], h.kitchen_id);
        assert_eq!(h.sink.placements.lock().unwrap().len(), 1);

        // The device id works too — the UI has the list in front of it.
        let (status, body) = open_on(&h.app, &h.kitchen_id).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn every_unreachable_target_fails_visibly_and_dispatches_nothing() {
        let h = node_harness().await;
        for (node, why) in [
            ("bathroom", "a room nobody configured"),
            (
                "01ARZ3NDEKTSV4RRFFQ69G5FZ9",
                "a device that was never paired",
            ),
            ("not-a-ulid-or-alias", "neither an alias nor an id"),
        ] {
            let (status, body) = open_on(&h.app, node).await;
            assert_eq!(status, StatusCode::CONFLICT, "{why}: {body}");
            assert_eq!(body["code"], "display.node_unavailable", "{why}");
        }

        // A device with no screen.
        let (status, body) = open_on(&h.app, &h.speaker_id).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "display.node_unavailable");
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("present"),
            "the owner should learn WHY: {body}"
        );

        // A revoked device — and it is also not connected.
        let (status, body) = open_on(&h.app, &h.gone_id).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "display.node_unavailable");

        assert!(
            h.sink.placements.lock().unwrap().is_empty(),
            "a refused placement must dispatch nothing at all"
        );
    }

    /// Omitting `node` keeps the pre-node behaviour exactly: the local agent.
    #[tokio::test]
    async fn an_untargeted_placement_still_goes_to_the_local_agent() {
        let h = node_harness().await;
        let response = h
            .app
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/artifacts/{ARTIFACT}/open"))
                    .header(header::AUTHORIZATION, format!("Bearer {OWNER_TOKEN}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["targetDeviceId"].is_null(), "no target named: {body}");
        assert_eq!(h.sink.placements.lock().unwrap().len(), 1);
    }
}
