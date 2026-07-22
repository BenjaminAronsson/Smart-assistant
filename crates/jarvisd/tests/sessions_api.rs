//! F0.8: session REST surface through the production router (docs/05 §1,
//! FR-02) — fake stores, full middleware path, problem-body contract.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use jarvis_application::ports::{CreateOutcome, IdentityStore, RepositoryError, SessionStore};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::conversations::Session;
use jarvis_domain::identity::Device;
use jarvis_domain::ids::SessionId;
use jarvisd::api::{AppState, router_with};
use jarvisd::auth::AuthState;
use jarvisd::sessions::SessionApi;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

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

/// In-memory SessionStore mirroring the contract of PgSessionStore
/// (which has its own DB-backed tests in jarvis-infra).
#[derive(Default)]
struct FakeSessionStore {
    sessions: Mutex<Vec<(Session, Option<String>)>>,
    audits: Mutex<Vec<AuditEvent>>,
}

#[async_trait::async_trait]
impl SessionStore for FakeSessionStore {
    async fn create(
        &self,
        session: &Session,
        idempotency_key: Option<&str>,
        audit: &AuditEvent,
    ) -> Result<CreateOutcome, RepositoryError> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(key) = idempotency_key
            && let Some((existing, _)) = sessions.iter().find(|(_, k)| k.as_deref() == Some(key))
        {
            if existing.title == session.title {
                return Ok(CreateOutcome::AlreadyExists(existing.clone()));
            }
            return Err(RepositoryError::IdempotencyConflict);
        }
        sessions.push((session.clone(), idempotency_key.map(str::to_owned)));
        self.audits.lock().unwrap().push(audit.clone());
        Ok(CreateOutcome::Created(session.clone()))
    }

    async fn get(&self, id: &SessionId) -> Result<Option<Session>, RepositoryError> {
        Ok(self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(s, _)| s)
            .find(|s| &s.id == id)
            .cloned())
    }

    async fn list(&self, limit: u32) -> Result<Vec<Session>, RepositoryError> {
        let mut sessions: Vec<Session> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(s, _)| s.clone())
            .collect();
        sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        sessions.truncate(limit as usize);
        Ok(sessions)
    }
}

async fn app_with_token() -> (Router, Arc<FakeSessionStore>, String) {
    let identity = Arc::new(FakeIdentityStore::default());
    let auth = AuthState::bootstrap(identity).await;
    let code = auth.current_pairing_code().unwrap();
    let store = Arc::new(FakeSessionStore::default());
    let app = router_with(
        AppState::new().with_auth(auth),
        Some(SessionApi::new(store.clone())),
        None,
        None,
        None,
    );
    // Pair through the real endpoint to get a live token.
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
    (app, store, token)
}

async fn request(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
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

fn create_request(token: &str, body: &str, idempotency_key: Option<&str>) -> Request<Body> {
    let mut builder = Request::post("/api/v1/sessions")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = idempotency_key {
        builder = builder.header("Idempotency-Key", key);
    }
    builder.body(Body::from(body.to_owned())).unwrap()
}

#[tokio::test]
async fn session_endpoints_require_a_token() {
    let (app, _store, _token) = app_with_token().await;
    let (status, body) = request(
        &app,
        Request::get("/api/v1/sessions")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "auth.invalid_token");
}

#[tokio::test]
async fn create_get_list_round_trip() {
    let (app, store, token) = app_with_token().await;

    let (status, created) = request(
        &app,
        create_request(&token, r#"{"title":"morning plans"}"#, None),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["title"], "morning plans");
    assert_eq!(created["status"], "active");
    let id = created["id"].as_str().unwrap().to_owned();

    // Audit event carries the acting device (NFR-07). Scoped so the guard
    // never lives across an await point.
    {
        let audits = store.audits.lock().unwrap();
        assert_eq!(audits[0].event_type, "session.created");
        assert!(audits[0].actor.starts_with("device:"));
    }

    let (status, fetched) = request(
        &app,
        Request::get(format!("/api/v1/sessions/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched, created);

    let (status, list) = request(
        &app,
        Request::get("/api/v1/sessions?limit=10")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn unknown_session_is_a_404_problem() {
    let (app, _store, token) = app_with_token().await;
    let (status, body) = request(
        &app,
        Request::get("/api/v1/sessions/01ARZ3NDEKTSV4RRFFQ69G5FAV")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "resource.not_found");
}

#[tokio::test]
async fn malformed_session_id_is_404_not_500() {
    let (app, _store, token) = app_with_token().await;
    let (status, body) = request(
        &app,
        Request::get("/api/v1/sessions/not-a-ulid")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "resource.not_found");
}

#[tokio::test]
async fn idempotent_replay_returns_the_original_with_200() {
    let (app, _store, token) = app_with_token().await;

    let (first_status, first) = request(
        &app,
        create_request(&token, r#"{"title":"t"}"#, Some("key-1")),
    )
    .await;
    assert_eq!(first_status, StatusCode::CREATED);

    let (replay_status, replay) = request(
        &app,
        create_request(&token, r#"{"title":"t"}"#, Some("key-1")),
    )
    .await;
    assert_eq!(replay_status, StatusCode::OK, "replay is 200, not 201");
    assert_eq!(replay["id"], first["id"]);
}

#[tokio::test]
async fn idempotency_key_reuse_with_different_payload_is_409() {
    let (app, _store, token) = app_with_token().await;
    request(
        &app,
        create_request(&token, r#"{"title":"one"}"#, Some("key-1")),
    )
    .await;

    let (status, body) = request(
        &app,
        create_request(&token, r#"{"title":"two"}"#, Some("key-1")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "idempotency.conflict");
}
