//! F0.7: pairing bootstrap + bearer middleware (docs/05 §6, invariant 5).
//! Fixture-driven: a fake in-memory IdentityStore, no database.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use axum::{Extension, Router};
use http_body_util::BodyExt;
use jarvis_application::ports::{IdentityStore, RepositoryError};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::identity::Device;
use jarvisd::auth::{AuthState, DeviceContext, pair, require_device};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

#[derive(Default)]
struct FakeIdentityStore {
    devices: Mutex<Vec<Device>>,
    audits: Mutex<Vec<AuditEvent>>,
    fail: bool,
}

impl FakeIdentityStore {
    fn failing() -> Self {
        Self {
            fail: true,
            ..Self::default()
        }
    }
}

#[async_trait::async_trait]
impl IdentityStore for FakeIdentityStore {
    async fn device_count(&self) -> Result<u64, RepositoryError> {
        if self.fail {
            return Err(RepositoryError::Storage("unreachable".into()));
        }
        Ok(self.devices.lock().unwrap().len() as u64)
    }

    async fn pair_device(
        &self,
        _owner_name: &str,
        device: &Device,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        if self.fail {
            return Err(RepositoryError::Storage("unreachable".into()));
        }
        self.devices.lock().unwrap().push(device.clone());
        self.audits.lock().unwrap().push(audit.clone());
        Ok(())
    }

    async fn find_active_device_by_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<Device>, RepositoryError> {
        if self.fail {
            return Err(RepositoryError::Storage("unreachable".into()));
        }
        Ok(self
            .devices
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.token_hash == token_hash && d.is_active())
            .cloned())
    }
}

async fn bootstrapped() -> (Arc<FakeIdentityStore>, AuthState, String) {
    let store = Arc::new(FakeIdentityStore::default());
    let auth = AuthState::bootstrap(store.clone()).await;
    let code = auth
        .current_pairing_code()
        .expect("empty store must open a pairing window");
    (store, auth, code)
}

fn pair_router(auth: AuthState) -> Router {
    Router::new().route("/pair", post(pair).with_state(auth))
}

async fn do_pair(router: &Router, code: &str, name: &str) -> (StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(
            Request::post("/pair")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"pairingCode":"{code}","deviceName":"{name}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn pairing_succeeds_once_and_stores_only_the_hash() {
    let (store, auth, code) = bootstrapped().await;
    let router = pair_router(auth.clone());

    let (status, body) = do_pair(&router, &code, "owner-laptop").await;
    assert_eq!(status, StatusCode::OK);
    let token = body["deviceToken"].as_str().unwrap();
    assert_eq!(token.len(), 64, "256-bit hex token");
    assert_eq!(body["scopes"], serde_json::json!(["ui"]));

    // invariant 5: the store holds a hash, never the token value.
    let devices = store.devices.lock().unwrap();
    assert_eq!(devices.len(), 1);
    assert_ne!(devices[0].token_hash, token);
    assert_eq!(devices[0].token_hash.len(), 64);
    // Audit written through the same port call (invariant 6 wiring).
    assert_eq!(store.audits.lock().unwrap()[0].event_type, "device.paired");
    // Window consumed.
    assert_eq!(auth.current_pairing_code(), None);
}

#[tokio::test]
async fn second_pair_attempt_fails_with_pairing_invalid() {
    let (_store, auth, code) = bootstrapped().await;
    let router = pair_router(auth);
    let (first, _) = do_pair(&router, &code, "laptop").await;
    assert_eq!(first, StatusCode::OK);

    let (second, body) = do_pair(&router, &code, "phone").await;
    assert_eq!(second, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "auth.pairing_invalid");
}

#[tokio::test]
async fn wrong_code_fails_and_does_not_consume_the_window() {
    let (_store, auth, code) = bootstrapped().await;
    let router = pair_router(auth.clone());

    let (status, body) = do_pair(&router, "000-000", "laptop").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "auth.pairing_invalid");
    assert_eq!(auth.current_pairing_code(), Some(code));
}

#[tokio::test]
async fn empty_device_name_is_validation_failed() {
    let (_store, auth, code) = bootstrapped().await;
    let router = pair_router(auth);
    let (status, body) = do_pair(&router, &code, " ").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "validation.failed");
}

#[tokio::test]
async fn bootstrap_with_devices_present_opens_no_window() {
    let (store, auth, code) = bootstrapped().await;
    do_pair(&pair_router(auth), &code, "laptop").await;

    // Restart: same store now has a device — no new pairing window.
    let rebooted = AuthState::bootstrap(store).await;
    assert_eq!(rebooted.current_pairing_code(), None);
}

#[tokio::test]
async fn bootstrap_with_unreachable_store_defers_without_failing() {
    let auth = AuthState::bootstrap(Arc::new(FakeIdentityStore::failing())).await;
    assert_eq!(auth.current_pairing_code(), None);
}

// --- middleware -------------------------------------------------------

fn protected_router(auth: AuthState) -> Router {
    Router::new()
        .route(
            "/protected",
            get(|Extension(device): Extension<DeviceContext>| async move {
                format!("device={}", device.device_id)
            }),
        )
        .layer(from_fn_with_state(auth, require_device))
}

async fn get_protected(router: &Router, token: Option<&str>) -> (StatusCode, String) {
    let mut request = Request::get("/protected");
    if let Some(t) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn missing_and_invalid_tokens_fail_closed_with_401() {
    let (_store, auth, code) = bootstrapped().await;
    do_pair(&pair_router(auth.clone()), &code, "laptop").await;
    let router = protected_router(auth);

    let (no_token, body) = get_protected(&router, None).await;
    assert_eq!(no_token, StatusCode::UNAUTHORIZED);
    assert!(body.contains("auth.invalid_token"));

    let (bad_token, _) = get_protected(&router, Some("not-a-real-token")).await;
    assert_eq!(bad_token, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn valid_token_reaches_the_handler_with_device_context() {
    let (store, auth, code) = bootstrapped().await;
    let (_, body) = do_pair(&pair_router(auth.clone()), &code, "laptop").await;
    let token = body["deviceToken"].as_str().unwrap().to_owned();
    let device_id = body["deviceId"].as_str().unwrap().to_owned();
    let router = protected_router(auth);

    let (status, response) = get_protected(&router, Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response, format!("device={device_id}"));

    // Revocation fails closed on the next request (docs/05 §6).
    store.devices.lock().unwrap()[0].revoked_at = Some(std::time::SystemTime::now());
    let (revoked, _) = get_protected(&router, Some(&token)).await;
    assert_eq!(revoked, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn repeated_wrong_codes_close_the_pairing_window() {
    let (_store, auth, code) = bootstrapped().await;
    let router = pair_router(auth.clone());

    for _ in 0..5 {
        let (status, _) = do_pair(&router, "999-999", "laptop").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    // Window is now closed — even the REAL code is refused until restart.
    assert_eq!(auth.current_pairing_code(), None);
    let (status, body) = do_pair(&router, &code, "laptop").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "auth.pairing_invalid");
}

#[tokio::test]
async fn stored_hash_is_exactly_sha256_of_the_returned_token() {
    use sha2::Digest;
    let (store, auth, code) = bootstrapped().await;
    let (_, body) = do_pair(&pair_router(auth), &code, "laptop").await;
    let token = body["deviceToken"].as_str().unwrap();
    let expected = hex::encode(sha2::Sha256::digest(token.as_bytes()));
    assert_eq!(store.devices.lock().unwrap()[0].token_hash, expected);
}

#[tokio::test]
async fn identity_outage_in_middleware_is_503_not_401() {
    let auth = AuthState::bootstrap(Arc::new(FakeIdentityStore::failing())).await;
    let router = protected_router(auth);
    let (status, body) = get_protected(&router, Some("whatever")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("provider.unavailable"));
}

// --- production router wiring (api::router) ---------------------------

#[tokio::test]
async fn health_page_shows_pairing_code_until_consumed_via_api_router() {
    let (_store, auth, code) = bootstrapped().await;
    let app = jarvisd::api::router(jarvisd::api::AppState::new().with_auth(auth));

    let health = |app: Router| async move {
        let response = app
            .oneshot(
                Request::get("/api/v1/diagnostics/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
    };

    // Window open: the code is on the loopback health page (docs/05 §6).
    assert_eq!(health(app.clone()).await["pairingCode"], code.as_str());

    // Pair through the PRODUCTION route mount.
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
    assert_eq!(response.status(), StatusCode::OK);

    // Consumed: gone from health.
    assert!(health(app).await.get("pairingCode").is_none());
}
