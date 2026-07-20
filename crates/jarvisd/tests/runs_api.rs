//! F1.5: the run REST surface through the production router (docs/05 §1,
//! FR-01/06/07) — auth, validation, ack, snapshot, cancellation. Fake stores +
//! the real engine (a scripted `FakeModel`); no database. The end-to-end
//! streaming + resync path (real Postgres + WebSocket) is `ws_stream.rs`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use jarvis_application::orchestrator::{CheckpointError, Checkpointer};
use jarvis_application::ports::{
    CreateOutcome, IdentityStore, MessageStore, RepositoryError, RunStore, RunView, SessionStore,
};
use jarvis_application::testing::FakeModel;
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::conversations::{Message, Session};
use jarvis_domain::identity::Device;
use jarvis_domain::ids::{RunId, SessionId};
use jarvis_domain::run::{Run, RunBudget, RunEvent};
use jarvis_infra::dispatcher::OutboxRecord;
use jarvisd::api::{AppState, RunWiring, router_with};
use jarvisd::auth::AuthState;
use jarvisd::runs::{PassthroughAssembler, RunApi, RunEngine, SystemClock};
use jarvisd::ws::{EventReader, WsHub, WsState};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";
const TERMINAL_RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

// --- fakes -----------------------------------------------------------------

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
        _owner: &str,
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

async fn app_with_token(model: FakeModel, run_store: Arc<FakeRunStore>) -> (Router, String) {
    let identity = Arc::new(FakeIdentityStore::default());
    let auth = AuthState::bootstrap(identity).await;
    let code = auth.current_pairing_code().unwrap();

    let messages = Arc::new(FakeMessageStore::default());
    let engine = RunEngine::new(
        Arc::new(model),
        Arc::new(PassthroughAssembler),
        Arc::new(NoopCheckpointer),
        messages.clone(),
        WsHub::new(),
        Arc::new(SystemClock),
        CancellationToken::new(),
    );
    let run_api = RunApi::new(
        Arc::new(FakeSessionStore {
            known: SESSION.parse().unwrap(),
        }),
        messages,
        run_store,
        Arc::new(EmptyEventReader),
        engine,
    );
    let ws = WsState {
        hub: WsHub::new(),
        events: Arc::new(EmptyEventReader),
        shutdown: CancellationToken::new(),
    };

    let app = router_with(
        AppState::new().with_auth(auth.clone()),
        None,
        Some(RunWiring { runs: run_api, ws }),
        None,
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
    (app, token)
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
    let (app, _token) = app_with_token(FakeModel::streaming(["hi"]), Arc::default()).await;
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
    ] {
        let (status, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], "auth.invalid_token");
    }
}

#[tokio::test]
async fn submit_message_acknowledges_a_received_run() {
    let runs = Arc::new(FakeRunStore::default());
    let (app, token) = app_with_token(FakeModel::streaming(["hello"]), runs.clone()).await;

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
    let (app, token) = app_with_token(FakeModel::streaming(["hi"]), Arc::default()).await;
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
    let (app, token) = app_with_token(FakeModel::streaming(["hi"]), Arc::default()).await;
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
    let (app, token) = app_with_token(FakeModel::streaming(["hi"]), Arc::default()).await;
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
    let (app, token) = app_with_token(FakeModel::streaming(["hi"]), runs).await;

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
    let (app, token) = app_with_token(FakeModel::hangs_after(["thinking"]), runs).await;

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
