//! F1.5: the run REST surface through the production router (docs/05 §1,
//! FR-01/06/07) — auth, validation, ack, snapshot, cancellation. Fake stores +
//! the real engine (a scripted `FakeModel`); no database. The end-to-end
//! streaming + resync path (real Postgres + WebSocket) is `ws_stream.rs`.

mod identity_fixture;
use identity_fixture::InMemoryIdentityStore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use jarvis_application::orchestrator::{CheckpointError, Checkpointer};
use jarvis_application::ports::{
    BlobStoreError, CreateOutcome, MessageStore, RepositoryError, RunStore, RunView, SessionStore,
};
use jarvis_application::testing::FakeModel;
use jarvis_domain::artifact::{ArtifactManifest, ArtifactVersion};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::conversations::{Message, Session};
use jarvis_domain::grants::Sha256;
use jarvis_domain::ids::{ArtifactId, RunId, SessionId};
use jarvis_domain::run::{Run, RunBudget, RunEvent};
use jarvis_infra::dispatcher::OutboxRecord;
use jarvisd::api::{AppState, RunWiring, Wiring, router_with};
use jarvisd::auth::AuthState;
use jarvisd::orchestrator_ports::{PassthroughAssembler, SystemClock};
use jarvisd::runs::{RunApi, RunEngine};
use jarvisd::ws::{EventReader, WsHub, WsState};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";
const TERMINAL_RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

// --- fakes -----------------------------------------------------------------

struct FakeSessionStore {
    known: SessionId,
}

#[async_trait::async_trait]
impl SessionStore for FakeSessionStore {
    async fn create(
        &self,
        _s: &Session,
        _k: Option<&str>,
        _a: &AuditEvent,
    ) -> Result<CreateOutcome, RepositoryError> {
        unimplemented!("not exercised")
    }
    async fn get(&self, id: &SessionId) -> Result<Option<Session>, RepositoryError> {
        Ok((id == &self.known)
            .then(|| Session::new(self.known.clone(), None, std::time::SystemTime::UNIX_EPOCH)))
    }
    async fn list(&self, _limit: u32) -> Result<Vec<Session>, RepositoryError> {
        Ok(vec![])
    }
}

#[derive(Default)]
struct FakeMessageStore {
    appended: Mutex<Vec<Message>>,
}

#[async_trait::async_trait]
impl MessageStore for FakeMessageStore {
    async fn append(&self, message: &Message) -> Result<(), RepositoryError> {
        self.appended.lock().unwrap().push(message.clone());
        Ok(())
    }
    async fn list_by_session(
        &self,
        _session: &SessionId,
        _limit: u32,
    ) -> Result<Vec<Message>, RepositoryError> {
        Ok(vec![])
    }
}

/// A run store seeded with fixed `view` answers; `create` records ids.
#[derive(Default)]
struct FakeRunStore {
    created: Mutex<Vec<RunId>>,
    views: Mutex<HashMap<String, RunView>>,
}

#[async_trait::async_trait]
impl RunStore for FakeRunStore {
    async fn create(&self, run: &Run) -> Result<(), RepositoryError> {
        self.created.lock().unwrap().push(run.id.clone());
        Ok(())
    }
    async fn load(&self, id: &RunId) -> Result<Option<Run>, RepositoryError> {
        Ok(self
            .views
            .lock()
            .unwrap()
            .get(id.as_str())
            .map(|v| v.run.clone()))
    }
    async fn view(&self, id: &RunId) -> Result<Option<RunView>, RepositoryError> {
        Ok(self.views.lock().unwrap().get(id.as_str()).cloned())
    }
    async fn load_unfinished(&self) -> Result<Vec<Run>, RepositoryError> {
        Ok(vec![])
    }
}

struct EmptyEventReader;

#[async_trait::async_trait]
impl EventReader for EmptyEventReader {
    async fn since(&self, _since: i64, _limit: i64) -> Result<Vec<OutboxRecord>, RepositoryError> {
        Ok(vec![])
    }
    async fn timeline(
        &self,
        _session: &str,
        _since: i64,
        _limit: i64,
    ) -> Result<Vec<OutboxRecord>, RepositoryError> {
        Ok(vec![])
    }
}

struct NoopCheckpointer;

#[async_trait::async_trait]
impl Checkpointer for NoopCheckpointer {
    async fn save(&self, _run: &Run) -> Result<(), CheckpointError> {
        Ok(())
    }
}

fn terminal_view() -> RunView {
    let mut run = Run::new(
        TERMINAL_RUN.parse().unwrap(),
        SESSION.parse().unwrap(),
        RunBudget::default_interactive(),
    );
    run.apply(RunEvent::ContextAssembled).unwrap();
    run.apply(RunEvent::ModelInvoked).unwrap();
    run.apply(RunEvent::FinalResponseReceived).unwrap();
    run.apply(RunEvent::ResponseCommitted).unwrap();
    RunView {
        run,
        created_at: std::time::SystemTime::UNIX_EPOCH,
        updated_at: std::time::SystemTime::UNIX_EPOCH,
    }
}

/// Blob + artifact doubles for the deep-dive wiring (F3b.6). `latest` always
/// answers `None`, which is the first-promotion path: re-promotion (versioning
/// the same document rather than minting a rival) is asserted where the
/// versioning logic lives, in the `jarvis-application` deep-dive tests.
#[derive(Default)]
struct FakeBlobs;

#[async_trait::async_trait]
impl jarvis_application::ports::BlobStore for FakeBlobs {
    async fn put(&self, bytes: &[u8]) -> Result<Sha256, BlobStoreError> {
        let mut key = [0u8; 32];
        key[31] = bytes.len() as u8;
        Ok(Sha256::from_bytes(key))
    }
    async fn get(&self, _hash: &Sha256) -> Result<Option<Vec<u8>>, BlobStoreError> {
        Ok(None)
    }
    async fn open(
        &self,
        _hash: &Sha256,
        _max_bytes: u64,
    ) -> Result<Option<jarvis_application::ports::BlobRead>, BlobStoreError> {
        Ok(None)
    }
    async fn contains(&self, _hash: &Sha256) -> Result<bool, BlobStoreError> {
        Ok(false)
    }
}

#[derive(Default)]
struct FakeArtifacts {
    versions: Mutex<Vec<ArtifactManifest>>,
}

#[async_trait::async_trait]
impl jarvis_application::ports::ArtifactStore for FakeArtifacts {
    async fn create_version(
        &self,
        manifest: &ArtifactManifest,
        _audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        self.versions.lock().unwrap().push(manifest.clone());
        Ok(())
    }
    async fn get(
        &self,
        _id: &ArtifactId,
        _version: ArtifactVersion,
    ) -> Result<Option<ArtifactManifest>, RepositoryError> {
        Ok(None)
    }
    async fn latest(&self, _id: &ArtifactId) -> Result<Option<ArtifactManifest>, RepositoryError> {
        Ok(None)
    }
    async fn list_versions(
        &self,
        _id: &ArtifactId,
    ) -> Result<Vec<ArtifactManifest>, RepositoryError> {
        Ok(Vec::new())
    }
}

/// The router, a live device token, and the hub the deep-dive router publishes
/// canvas instructions on (F3b.6) — one hub, shared with the run surface, so a
/// test can watch what a real message submission actually broadcasts.
async fn app_with_token(
    model: FakeModel,
    run_store: Arc<FakeRunStore>,
) -> (Router, String, Arc<WsHub>) {
    let identity = Arc::new(InMemoryIdentityStore::default());
    let auth = AuthState::bootstrap(identity).await;
    let code = auth.current_pairing_code().unwrap();

    let messages = Arc::new(FakeMessageStore::default());
    let hub = WsHub::new();
    let engine = RunEngine::new(
        Arc::new(model),
        Arc::new(PassthroughAssembler),
        Arc::new(NoopCheckpointer),
        messages.clone(),
        hub.clone(),
        Arc::new(SystemClock),
        CancellationToken::new(),
        None, // text-only path: the run REST surface tests wire no tool plane.
    );
    // A lazy pool that never connects: these tests exercise the run REST surface,
    // not the approval endpoint, so the gate is constructed but never used.
    let pool = jarvis_infra::db::connect_lazy(
        "postgres://jarvis:jarvis-dev-only@127.0.0.1:5432/jarvis",
        1,
    )
    .expect("lazy pool");
    let sessions = Arc::new(FakeSessionStore {
        known: SESSION.parse().unwrap(),
    });
    let deepdive = jarvisd::deepdive::DeepDiveApi::new(
        Arc::new(jarvis_application::deepdive::DeepDiveService::new(
            Arc::new(FakeBlobs),
            Arc::new(FakeArtifacts::default()),
            3,
            "user:owner",
            Arc::new(SystemClock),
        )),
        sessions.clone(),
        hub.clone(),
    );
    let run_api = RunApi::new(
        sessions,
        messages,
        run_store,
        Arc::new(EmptyEventReader),
        engine,
        jarvisd::approvals::JarvisApprovalGate::new(pool),
        Some(deepdive.clone()),
    );
    let ws = WsState {
        identity: None,
        connected: Default::default(),
        surfaces: Default::default(),
        audit: None,
        revocations: Default::default(),
        hub: hub.clone(),
        events: Arc::new(EmptyEventReader),
        shutdown: CancellationToken::new(),
        transcriber: None,
        synthesizer: None,
        runs: None,
    };

    let app = router_with(
        AppState::new().with_auth(auth.clone()),
        Wiring {
            runs: Some(RunWiring { runs: run_api, ws }),
            deepdive: Some(deepdive),
            ..Wiring::default()
        },
    );
    // Pair through the real endpoint for a live token.
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
    (app, token, hub)
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

fn post_message(token: &str, session: &str, body: &str) -> Request<Body> {
    Request::post(format!("/api/v1/sessions/{session}/messages"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

// --- tests -----------------------------------------------------------------

#[tokio::test]
async fn run_routes_require_a_token() {
    let (app, _token, _hub) = app_with_token(FakeModel::streaming(["hi"]), Arc::default()).await;
    for request in [
        Request::post(format!("/api/v1/sessions/{SESSION}/messages"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"content":[{"type":"text","text":"x"}]}"#))
            .unwrap(),
        Request::get(format!("/api/v1/runs/{TERMINAL_RUN}"))
            .body(Body::empty())
            .unwrap(),
        Request::post(format!("/api/v1/runs/{TERMINAL_RUN}/cancel"))
            .body(Body::empty())
            .unwrap(),
        Request::get(format!("/api/v1/sessions/{SESSION}/timeline"))
            .body(Body::empty())
            .unwrap(),
        // The deep-dive surface is authenticated like every other one (F3b.6).
        Request::post(format!("/api/v1/sessions/{SESSION}/deepdive/findings"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
        Request::post(format!("/api/v1/sessions/{SESSION}/deepdive/promote"))
            .body(Body::empty())
            .unwrap(),
    ] {
        let (status, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], "auth.invalid_token");
    }
}

#[tokio::test]
async fn submit_message_acknowledges_a_received_run() {
    let runs = Arc::new(FakeRunStore::default());
    let (app, token, _hub) = app_with_token(FakeModel::streaming(["hello"]), runs.clone()).await;

    let (status, ack) = send(
        &app,
        post_message(
            &token,
            SESSION,
            r#"{"content":[{"type":"text","text":"hi"}]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(ack["sessionId"], SESSION);
    assert_eq!(ack["state"], "received");
    assert!(ack["runId"].as_str().unwrap().len() == 26);
    // The run was durably created before the ack (so it is recoverable).
    assert_eq!(runs.created.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn submit_to_unknown_session_is_404() {
    let (app, token, _hub) = app_with_token(FakeModel::streaming(["hi"]), Arc::default()).await;
    let (status, body) = send(
        &app,
        post_message(
            &token,
            "01BX5ZZKBKACTAV9WEVGEMMVRZ",
            r#"{"content":[{"type":"text","text":"hi"}]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "resource.not_found");
}

#[tokio::test]
async fn empty_content_is_a_validation_error() {
    let (app, token, _hub) = app_with_token(FakeModel::streaming(["hi"]), Arc::default()).await;
    let (status, body) = send(
        &app,
        post_message(
            &token,
            SESSION,
            r#"{"content":[{"type":"text","text":"   "}]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "validation.failed");
}

#[tokio::test]
async fn get_unknown_run_is_404() {
    let (app, token, _hub) = app_with_token(FakeModel::streaming(["hi"]), Arc::default()).await;
    let (status, body) = send(
        &app,
        Request::get(format!("/api/v1/runs/{TERMINAL_RUN}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "resource.not_found");
}

#[tokio::test]
async fn get_run_projects_the_domain_state() {
    let runs = Arc::new(FakeRunStore::default());
    runs.views
        .lock()
        .unwrap()
        .insert(TERMINAL_RUN.to_owned(), terminal_view());
    let (app, token, _hub) = app_with_token(FakeModel::streaming(["hi"]), runs).await;

    let (status, dto) = send(
        &app,
        Request::get(format!("/api/v1/runs/{TERMINAL_RUN}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dto["id"], TERMINAL_RUN);
    assert_eq!(dto["state"], "completed");
    assert_eq!(dto["outcome"]["kind"], "completed");
    assert_eq!(dto["budget"]["maxModelTurns"], 8);
}

#[tokio::test]
async fn cancel_active_run_is_accepted_but_terminal_is_conflict() {
    let runs = Arc::new(FakeRunStore::default());
    runs.views
        .lock()
        .unwrap()
        .insert(TERMINAL_RUN.to_owned(), terminal_view());
    // A hanging model keeps the submitted run active in the registry.
    let (app, token, _hub) = app_with_token(FakeModel::hangs_after(["thinking"]), runs).await;

    // Start a run and cancel it while it is active → 202.
    let (_s, ack) = send(
        &app,
        post_message(
            &token,
            SESSION,
            r#"{"content":[{"type":"text","text":"hi"}]}"#,
        ),
    )
    .await;
    let active = ack["runId"].as_str().unwrap();
    let (status, _b) = send(
        &app,
        Request::post(format!("/api/v1/runs/{active}/cancel"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // A terminal (not-active) run → 409 run.not_cancellable.
    let (status, body) = send(
        &app,
        Request::post(format!("/api/v1/runs/{TERMINAL_RUN}/cancel"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "run.not_cancellable");

    // An unknown run → 404.
    let (status, _b) = send(
        &app,
        Request::post("/api/v1/runs/01BX5ZZKBKACTAV9WEVGEMMVRZ/cancel")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --- deep-dive wiring (F3b.6, FR-27, ADR-017) ------------------------------

/// Drain the hub for the `hud.canvas` payloads broadcast so far, checking the
/// envelope discipline on the way past: the discriminator and the contract
/// version are the hub's job, never the payload author's (docs/05 §3).
fn canvases(
    rx: &mut tokio::sync::broadcast::Receiver<Arc<jarvis_contracts::envelope::EventEnvelope>>,
) -> Vec<serde_json::Value> {
    let mut seen = Vec::new();
    while let Ok(envelope) = rx.try_recv() {
        if envelope.event_type == "hud.canvas" {
            assert_eq!(envelope.v, jarvis_contracts::CONTRACT_VERSION);
            assert_eq!(
                envelope.channel,
                jarvis_contracts::envelope::Channel::Session
            );
            // The payload carries the event's own fields with the tag split
            // out onto the envelope, so the instruction sits under `canvas` —
            // the same shape as `media.state`'s `state`.
            seen.push(envelope.payload["canvas"].clone());
        }
    }
    seen
}

#[tokio::test]
async fn submitting_a_message_routes_the_deep_dive_turn_onto_the_canvas() {
    // The finding this test exists for: F3b.6 must be reachable from the
    // running system, not only from its own unit tests. An ordinary
    // `POST /sessions/{id}/messages` — the normal turn on the Run Spine,
    // docs/12 §2.5 — has to produce a canvas instruction on the WS stream.
    let (app, token, hub) = app_with_token(FakeModel::streaming(["hi"]), Arc::default()).await;
    let mut rx = hub.subscribe();

    let (status, _ack) = send(
        &app,
        post_message(
            &token,
            SESSION,
            r#"{"content":[{"type":"text","text":"ramen places near Kreuzberg"}]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let first = canvases(&mut rx);
    assert_eq!(first.len(), 1, "the turn published one canvas instruction");
    // Nothing to continue yet, so the first query of a session is a new topic.
    assert_eq!(first[0]["action"], "shelve");
    assert_eq!(first[0]["sessionId"], SESSION);

    // A follow-up on the same thread EXTENDS: it must not shelve the canvas the
    // answer is sitting on (FR-27 — the whole point of the feature).
    let (status, _ack) = send(
        &app,
        post_message(
            &token,
            SESSION,
            r#"{"content":[{"type":"text","text":"tell me more about that"}]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let second = canvases(&mut rx);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0]["action"], "extend");
}

#[tokio::test]
async fn filing_findings_puts_the_sources_card_on_the_wire() {
    let (app, token, hub) = app_with_token(FakeModel::streaming(["hi"]), Arc::default()).await;

    let (status, _ack) = send(
        &app,
        post_message(
            &token,
            SESSION,
            r#"{"content":[{"type":"text","text":"ramen places near Kreuzberg"}]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let mut rx = hub.subscribe();
    let (status, body) = send(
        &app,
        Request::post(format!("/api/v1/sessions/{SESSION}/deepdive/findings"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "facts": ["Kome opens at noon and is rated 4.7."],
                    "sources": [
                        { "title": "Ramen — Wikipedia", "url": "https://en.wikipedia.org/wiki/Ramen" },
                        { "title": "Definitely fine", "url": "javascript:alert(1)" }
                    ],
                    "images": [{
                        "alt": "a bowl of shoyu ramen",
                        "url": "https://cdn.example/one.jpg",
                        "sourceUrl": "https://kome.example/menu"
                    }]
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["facts"], 1);
    assert_eq!(body["sources"], 1);
    assert_eq!(body["images"], 1);
    // The `javascript:` URL was refused by the recorder, not filed (B1).
    assert_eq!(body["refused"].as_array().unwrap().len(), 1);

    let published = canvases(&mut rx);
    assert_eq!(published.len(), 1);
    let cards = published[0]["cards"].as_array().unwrap();
    let types: Vec<&str> = cards.iter().map(|c| c["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["card.sources", "card.gallery"]);
    // The chip label is computed server-side from the parsed host, and the
    // refused URL is nowhere on the wire.
    let items = cards[0]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["domain"], "en.wikipedia.org");
    assert!(!published[0].to_string().contains("javascript:"));
}

#[tokio::test]
async fn promoting_a_thread_with_nothing_in_it_is_refused_with_its_own_code() {
    let (app, token, _hub) = app_with_token(FakeModel::streaming(["hi"]), Arc::default()).await;
    let (status, body) = send(
        &app,
        Request::post(format!("/api/v1/sessions/{SESSION}/deepdive/promote"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "deepdive.nothing_to_promote");
}

#[tokio::test]
async fn accepting_the_offer_writes_the_document_over_rest() {
    // Bullet 4 of the wiring end to end: the offer is an offer, and *accepting*
    // it is what writes the versioned markdown artifact through the F3a.2 ports.
    let (app, token, _hub) = app_with_token(FakeModel::streaming(["hi"]), Arc::default()).await;

    let (status, _ack) = send(
        &app,
        post_message(
            &token,
            SESSION,
            r#"{"content":[{"type":"text","text":"ramen places near Kreuzberg"}]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, _filed) = send(
        &app,
        Request::post(format!("/api/v1/sessions/{SESSION}/deepdive/findings"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "facts": ["Kome opens at noon and is rated 4.7."],
                    "sources": [{ "title": "Guide", "url": "https://guide.example/ramen" }]
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, promoted) = send(
        &app,
        Request::post(format!("/api/v1/sessions/{SESSION}/deepdive/promote"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{promoted}");
    assert_eq!(promoted["version"], 1);
    assert_eq!(promoted["firstPromotion"], true);
    assert!(promoted["artifactId"].as_str().unwrap().len() == 26);
    assert!(promoted["sha256"].as_str().unwrap().len() == 64);
}
