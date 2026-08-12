//! F7.1 (FR-19): the device management surface — `GET /api/v1/devices` and
//! `POST /api/v1/devices/{id}/revoke` — through the production router, with
//! the real bearer middleware in front of it.
//!
//! The assertions that matter here are the negative ones. A room satellite is
//! a *paired, authenticated* device; everything it must not be able to do is
//! something an authenticated request could otherwise do.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use jarvis_domain::identity::DeviceClass;
use jarvisd::api::{AppState, Wiring, router_with};
use jarvisd::auth::AuthState;
mod identity_fixture;
use identity_fixture::{InMemoryIdentityStore, device};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tower::ServiceExt;

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

const OWNER_TOKEN: &str = "owner-token";
const NODE_TOKEN: &str = "node-token";

/// A store holding one owner device and one kitchen room node, both paired.
fn two_devices() -> Arc<InMemoryIdentityStore> {
    Arc::new(
        InMemoryIdentityStore::new()
            .with_device(device(
                "owner shell",
                DeviceClass::OwnerUi,
                &token_hash(OWNER_TOKEN),
            ))
            .with_device(device(
                "kitchen screen",
                DeviceClass::RoomNode,
                &token_hash(NODE_TOKEN),
            )),
    )
}

async fn router_for(store: Arc<InMemoryIdentityStore>) -> (axum::Router, AuthState) {
    let auth = AuthState::bootstrap(store).await;
    let state = AppState::new().with_auth(auth.clone());
    (router_with(state, Wiring::default()), auth)
}

async fn send(router: &axum::Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

fn get_devices(token: &str) -> Request<Body> {
    Request::builder()
        .uri("/api/v1/devices")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request")
}

fn revoke(token: &str, id: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/api/v1/devices/{id}/revoke"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .expect("request")
}

#[tokio::test]
async fn the_owner_sees_every_device_with_its_class_and_scopes() {
    let store = two_devices();
    let (router, _auth) = router_for(store).await;

    let (status, body) = send(&router, get_devices(OWNER_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    let devices = body["devices"].as_array().expect("devices array");
    assert_eq!(devices.len(), 2);

    let node = devices
        .iter()
        .find(|d| d["deviceClass"] == "room-node")
        .expect("the node is listed");
    assert_eq!(node["executesTools"], false);
    let node_scopes: Vec<&str> = node["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .map(|s| s.as_str().expect("string"))
        .collect();
    assert_eq!(node_scopes, vec!["display-agent", "voice-capture"]);

    let owner = devices
        .iter()
        .find(|d| d["deviceClass"] == "owner-ui")
        .expect("the owner is listed");
    assert_eq!(owner["executesTools"], true);
    assert!(
        owner["scopes"]
            .as_array()
            .expect("scopes")
            .iter()
            .any(|s| s == "home:control"),
        "the owner device holds tool scopes"
    );

    // A device list is a management read, never a credential dump.
    let raw = body.to_string();
    assert!(
        !raw.contains("tokenHash"),
        "token hashes must not be listed"
    );
    assert!(!raw.contains(&token_hash(OWNER_TOKEN)), "no hash leaks");
}

/// The core F7.1 assertion: a paired satellite is authenticated, and still
/// cannot see or touch device management.
#[tokio::test]
async fn a_room_node_can_neither_list_nor_revoke() {
    let store = two_devices();
    let owner_id = store.devices()[0].id.to_string();
    let (router, _auth) = router_for(store).await;

    let (status, body) = send(&router, get_devices(NODE_TOKEN)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "auth.scope_missing");

    let (status, body) = send(&router, revoke(NODE_TOKEN, &owner_id, "{}")).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a node must not be able to revoke the owner's device"
    );
    assert_eq!(body["code"], "auth.scope_missing");
}

#[tokio::test]
async fn revoking_a_node_is_audited_idempotent_and_fails_its_token_closed() {
    let store = two_devices();
    let node_id = store.devices()[1].id.to_string();
    let (router, _auth) = router_for(store.clone()).await;

    let (status, body) = send(
        &router,
        revoke(OWNER_TOKEN, &node_id, r#"{"reason":"left the house"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deviceClass"], "room-node");
    assert!(body["revokedAt"].is_string(), "revocation is visible");
    assert_eq!(body["revokedReason"], "left the house");

    // Written in the same call as the domain change (invariant 6).
    let audit = store
        .audits()
        .into_iter()
        .find(|a| a.event_type == "device.revoked")
        .expect("device.revoked audited");
    assert_eq!(audit.target, format!("device:{node_id}"));
    assert!(audit.payload_json.contains("left the house"));

    // Fails closed on the very next request.
    let (status, _) = send(&router, get_devices(NODE_TOKEN)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Idempotent: revoking again is not an error and mints no second audit row.
    let (status, _) = send(&router, revoke(OWNER_TOKEN, &node_id, "{}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        store
            .audits()
            .iter()
            .filter(|a| a.event_type == "device.revoked")
            .count(),
        1,
        "an already-revoked device is not re-audited"
    );
}

#[tokio::test]
async fn the_last_owner_device_cannot_be_revoked() {
    let store = two_devices();
    let owner_id = store.devices()[0].id.to_string();
    let (router, _auth) = router_for(store).await;

    let (status, body) = send(&router, revoke(OWNER_TOKEN, &owner_id, "{}")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "identity.last_owner_device");

    // And the owner is still able to work.
    let (status, _) = send(&router, get_devices(OWNER_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_second_owner_device_makes_the_first_revocable() {
    let store = Arc::new(
        InMemoryIdentityStore::new()
            .with_device(device(
                "old laptop",
                DeviceClass::OwnerUi,
                &token_hash(OWNER_TOKEN),
            ))
            .with_device(device(
                "new laptop",
                DeviceClass::OwnerUi,
                &token_hash("second-owner"),
            )),
    );
    let old_id = store.devices()[0].id.to_string();
    let (router, _auth) = router_for(store).await;

    let (status, _) = send(
        &router,
        revoke("second-owner", &old_id, r#"{"reason":"replaced"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&router, get_devices(OWNER_TOKEN)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the old laptop is out");
}

#[tokio::test]
async fn unknown_and_malformed_device_ids_are_rejected_distinctly() {
    let store = two_devices();
    let (router, _auth) = router_for(store).await;

    let (status, body) = send(&router, revoke(OWNER_TOKEN, "not-a-ulid", "{}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "validation.failed");

    let (status, body) = send(
        &router,
        revoke(OWNER_TOKEN, "01ARZ3NDEKTSV4RRFFQ69G5FAV", "{}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "resource.not_found");
}

#[tokio::test]
async fn a_revocation_reason_is_bounded_and_control_character_free() {
    let store = two_devices();
    let node_id = store.devices()[1].id.to_string();
    let (router, _auth) = router_for(store.clone()).await;

    let long = "x".repeat(201);
    let (status, body) = send(
        &router,
        revoke(
            OWNER_TOKEN,
            &node_id,
            &serde_json::json!({ "reason": long }).to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "validation.failed");

    let (status, body) = send(
        &router,
        revoke(
            OWNER_TOKEN,
            &node_id,
            &serde_json::json!({ "reason": "line\u{0007}break" }).to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "validation.failed");

    // Neither attempt revoked anything.
    assert!(store.devices()[1].is_active());
}

#[tokio::test]
async fn device_management_needs_a_token_at_all() {
    let store = two_devices();
    let (router, _auth) = router_for(store).await;
    let request = Request::builder()
        .uri("/api/v1/devices")
        .body(Body::empty())
        .expect("request");
    let (status, body) = send(&router, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "auth.invalid_token");
}

/// **The class gate, asserted against the real router (security-auditor
/// BLOCKING-2).**
///
/// The registry-level test in `jarvisd::tools` asks whether a node's scope set
/// covers any tool's `required_scopes`. That is the wrong producer: the HTTP
/// surface is what a paired node actually reaches, and before F7.1's gate every
/// authenticated route was open to any authenticated device — including
/// `POST /runs/{id}/approvals/{approval_id}`, where a kitchen screen could have
/// supplied the human decision that mints an R2/R3 grant.
///
/// Deny-by-default: everything owner-only, `/ws/v1` the one deliberate
/// carve-out. A route added to the protected router without thinking about
/// class lands on the safe side, and this test says so.
#[tokio::test]
async fn a_room_node_is_refused_every_owner_surface_on_the_real_router() {
    let store = two_devices();
    let (router, _auth) = router_for(store).await;

    // Paths that exist on a default `Wiring` are 403 (class), and paths whose
    // surface is not mounted are 404 — never 200, and never 401 (the node IS
    // authenticated; that is the whole point).
    for (method, path) in [
        ("GET", "/api/v1/devices"),
        ("POST", "/api/v1/devices/01ARZ3NDEKTSV4RRFFQ69G5FAV/revoke"),
        ("GET", "/api/v1/sessions"),
        ("POST", "/api/v1/sessions"),
        (
            "POST",
            "/api/v1/sessions/01ARZ3NDEKTSV4RRFFQ69G5FAV/messages",
        ),
        ("POST", "/api/v1/runs/01ARZ3NDEKTSV4RRFFQ69G5FAV/cancel"),
        (
            "POST",
            "/api/v1/runs/01ARZ3NDEKTSV4RRFFQ69G5FAV/approvals/01ARZ3NDEKTSV4RRFFQ69G5FB1",
        ),
        ("POST", "/api/v1/media/command"),
        ("GET", "/api/v1/timers"),
        ("GET", "/api/v1/lists"),
        ("GET", "/api/v1/memories"),
        ("POST", "/api/v1/artifacts/01ARZ3NDEKTSV4RRFFQ69G5FAV/open"),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {NODE_TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .expect("request");
        let (status, _) = send(&router, request).await;
        assert!(
            status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
            "{method} {path} answered {status} to a room node — owner surfaces are `ui`-only"
        );
    }
}

/// The other half of deny-by-default: the owner still gets through everywhere.
/// A gate that refuses everyone passes the test above and breaks the product.
#[tokio::test]
async fn the_owner_still_reaches_the_owner_surfaces() {
    let store = two_devices();
    let (router, _auth) = router_for(store).await;

    let (status, _) = send(&router, get_devices(OWNER_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
}
