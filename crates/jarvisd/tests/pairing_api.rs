//! F7.2 (FR-19, ADR-031): node pairing by challenge-response, through the
//! production router.
//!
//! The happy path is one test; the rest is the threat model. Every negative
//! case here is something a LAN attacker can actually attempt: guess the code,
//! replay a challenge, sign with the wrong key, ask for authority it was not
//! offered, or flood the window.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey};
use http_body_util::BodyExt;
use jarvis_domain::identity::DeviceClass;
use jarvisd::api::{AppState, Wiring, router_with};
use jarvisd::auth::AuthState;
use jarvisd::pairing::PairingState;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tower::ServiceExt;

mod identity_fixture;
use identity_fixture::{InMemoryIdentityStore, device};

const OWNER_TOKEN: &str = "owner-token";

fn owner_hash() -> String {
    hex::encode(Sha256::digest(OWNER_TOKEN.as_bytes()))
}

/// A router with the owner already paired — a node can only join a house that
/// has an owner in it.
async fn harness() -> (Router, PairingState, Arc<InMemoryIdentityStore>) {
    let store = Arc::new(InMemoryIdentityStore::new().with_device(device(
        "owner laptop",
        DeviceClass::OwnerUi,
        &owner_hash(),
    )));
    let auth = AuthState::bootstrap(store.clone()).await;
    let pairing = PairingState::new();
    let router = router_with(
        AppState::new().with_auth(auth),
        Wiring {
            pairing: pairing.clone(),
            ..Wiring::default()
        },
    );
    (router, pairing, store)
}

async fn send(router: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

fn post_as_owner(uri: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {OWNER_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .expect("request")
}

async fn open_window(router: &Router) -> String {
    let (status, body) = send(router, post_as_owner("/api/v1/devices/pairing-window")).await;
    assert_eq!(status, StatusCode::OK, "owner opens the window: {body}");
    body["pairingCode"]
        .as_str()
        .expect("a code is returned")
        .to_owned()
}

fn a_key() -> SigningKey {
    // Deterministic per call site, but distinct keys where it matters.
    SigningKey::generate(&mut rand08_compat())
}

/// `ed25519-dalek` 2 takes a `rand_core` 0.6 RNG; the workspace is on rand 0.9.
/// A test-local OS-seeded generator keeps the two apart.
fn rand08_compat() -> rand_core6::OsRng {
    rand_core6::OsRng
}

async fn start(
    router: &Router,
    key: &SigningKey,
    class: &str,
    code: &str,
) -> (StatusCode, serde_json::Value) {
    send(
        router,
        post(
            "/api/v1/devices/pair",
            serde_json::json!({
                "publicKey": BASE64.encode(key.verifying_key().as_bytes()),
                "deviceName": "kitchen screen",
                "requestedClass": class,
                "pairingCode": code,
            }),
        ),
    )
    .await
}

async fn complete(
    router: &Router,
    key: &SigningKey,
    challenge: &serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let nonce = BASE64
        .decode(challenge["challenge"].as_str().expect("challenge"))
        .expect("base64");
    let signature = key.sign(&nonce);
    send(
        router,
        post(
            "/api/v1/devices/pair/complete",
            serde_json::json!({
                "challengeId": challenge["challengeId"],
                "signature": BASE64.encode(signature.to_bytes()),
            }),
        ),
    )
    .await
}

#[tokio::test]
async fn a_node_pairs_by_proving_it_holds_its_key() {
    let (router, _pairing, store) = harness().await;
    let code = open_window(&router).await;
    let key = a_key();

    let (status, challenge) = start(&router, &key, "room-node", &code).await;
    assert_eq!(status, StatusCode::OK, "challenge issued: {challenge}");

    let (status, body) = complete(&router, &key, &challenge).await;
    assert_eq!(status, StatusCode::OK, "pairing completes: {body}");
    assert_eq!(body["deviceClass"], "room-node");
    let scopes: Vec<&str> = body["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .map(|s| s.as_str().expect("string"))
        .collect();
    assert_eq!(scopes, vec!["display-agent", "voice-capture"]);
    assert!(
        body["deviceToken"].as_str().expect("token").len() >= 64,
        "an opaque 256-bit token"
    );

    // Stored against the owner's user, with the key and an audit row.
    let devices = store.devices();
    let node = devices
        .iter()
        .find(|d| d.class == DeviceClass::RoomNode)
        .expect("node stored");
    assert_eq!(
        node.public_key.as_deref(),
        Some(BASE64.encode(key.verifying_key().as_bytes()).as_str())
    );
    let owner = devices
        .iter()
        .find(|d| d.class == DeviceClass::OwnerUi)
        .expect("owner");
    assert_eq!(node.user_id, owner.user_id, "the node joins the owner");
    let audit = store
        .audits()
        .into_iter()
        .find(|a| a.event_type == "device.paired" && a.payload_json.contains("room-node"))
        .expect("device.paired audited");
    assert!(audit.payload_json.contains("keyFingerprint"));
    assert!(
        !audit.payload_json.contains("deviceToken"),
        "the token never reaches the audit trail"
    );
}

/// The token is bound to a key the node *proved* it holds. Signing with a
/// different key than the one registered is the impersonation attempt this
/// whole flow exists to defeat.
#[tokio::test]
async fn a_signature_from_a_different_key_is_refused() {
    let (router, _pairing, store) = harness().await;
    let code = open_window(&router).await;
    let registered = a_key();
    let attacker = a_key();

    let (_, challenge) = start(&router, &registered, "display-node", &code).await;
    let (status, body) = complete(&router, &attacker, &challenge).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "identity.challenge_rejected");
    assert_eq!(store.devices().len(), 1, "nothing was paired");
}

/// A challenge is spent by being presented, so a captured signature cannot be
/// replayed — not even the *legitimate* one.
#[tokio::test]
async fn a_challenge_cannot_be_used_twice() {
    let (router, _pairing, _store) = harness().await;
    let code = open_window(&router).await;
    let key = a_key();

    let (_, challenge) = start(&router, &key, "voice-node", &code).await;
    let (status, _) = complete(&router, &key, &challenge).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = complete(&router, &key, &challenge).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "replay refused");
    assert_eq!(body["code"], "identity.challenge_rejected");
}

/// A wrong signature burns the challenge too — otherwise one nonce would be a
/// standing target.
#[tokio::test]
async fn a_failed_attempt_consumes_its_challenge() {
    let (router, _pairing, _store) = harness().await;
    let code = open_window(&router).await;
    let key = a_key();
    let (_, challenge) = start(&router, &key, "display-node", &code).await;

    let (status, _) = complete(&router, &a_key(), &challenge).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Even the right key cannot rescue that challenge.
    let (status, _) = complete(&router, &key, &challenge).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// A node asks; the server assigns. `owner-ui` is refused outright — never
/// silently downgraded, because a client that thinks it holds tool authority
/// will act as though it does.
#[tokio::test]
async fn a_node_cannot_request_owner_authority() {
    let (router, _pairing, store) = harness().await;
    let code = open_window(&router).await;

    for class in ["owner-ui", "admin", "OWNER-UI", ""] {
        let (status, body) = start(&router, &a_key(), class, &code).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "class `{class}` must be refused"
        );
        assert_eq!(body["code"], "identity.class_not_grantable");
    }
    assert_eq!(store.devices().len(), 1, "nothing was paired");
}

#[tokio::test]
async fn pairing_needs_an_open_window_and_the_right_code() {
    let (router, _pairing, _store) = harness().await;
    let key = a_key();

    // No window open at all.
    let (status, body) = start(&router, &key, "room-node", "123-456").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "auth.pairing_invalid");

    // Open one; a wrong code still fails, with the same answer.
    let code = open_window(&router).await;
    let wrong = if code == "000-000" {
        "111-111"
    } else {
        "000-000"
    };
    let (status, body) = start(&router, &key, "room-node", wrong).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "auth.pairing_invalid");

    // The right code still works — a wrong guess must not burn the window.
    let (status, _) = start(&router, &key, "room-node", &code).await;
    assert_eq!(status, StatusCode::OK);
}

/// A 6-digit code is ~20 bits. On a LAN an attacker must not get 10^6 tries.
#[tokio::test]
async fn repeated_wrong_codes_close_the_window() {
    let (router, _pairing, _store) = harness().await;
    let code = open_window(&router).await;
    let wrong = if code == "000-000" {
        "111-111"
    } else {
        "000-000"
    };

    for _ in 0..5 {
        let (status, _) = start(&router, &a_key(), "room-node", wrong).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    // Even the real code is refused now: the owner re-opens deliberately.
    let (status, body) = start(&router, &a_key(), "room-node", &code).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "auth.pairing_invalid");
}

/// The challenge map is reachable without a token, so it is capped — and the
/// cap refuses new starts rather than evicting, which would let a flood push
/// out the legitimate node's challenge.
#[tokio::test]
async fn in_flight_challenges_are_bounded() {
    let (router, _pairing, _store) = harness().await;
    let code = open_window(&router).await;

    let mut refused = false;
    for _ in 0..12 {
        let (status, _) = start(&router, &a_key(), "display-node", &code).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            refused = true;
            break;
        }
    }
    assert!(refused, "an unbounded challenge map is a memory primitive");
}

#[tokio::test]
async fn a_malformed_key_or_signature_is_rejected() {
    let (router, _pairing, _store) = harness().await;
    let code = open_window(&router).await;

    for bad in ["", "not-base64!!", &BASE64.encode([7u8; 16])] {
        let (status, _) = send(
            &router,
            post(
                "/api/v1/devices/pair",
                serde_json::json!({
                    "publicKey": bad,
                    "deviceName": "kitchen",
                    "requestedClass": "room-node",
                    "pairingCode": code,
                }),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "key `{bad}` must not parse"
        );
    }

    let key = a_key();
    let (_, challenge) = start(&router, &key, "room-node", &code).await;
    let (status, _) = send(
        &router,
        post(
            "/api/v1/devices/pair/complete",
            serde_json::json!({
                "challengeId": challenge["challengeId"],
                "signature": BASE64.encode([0u8; 64]),
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Only the owner may open a window. A paired node opening one would be able
/// to enroll its own siblings.
#[tokio::test]
async fn a_node_cannot_open_a_pairing_window() {
    let (router, _pairing, store) = harness().await;
    let node_token = "node-token";
    store.add_device(device(
        "kitchen screen",
        DeviceClass::RoomNode,
        &hex::encode(Sha256::digest(node_token.as_bytes())),
    ));

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/devices/pairing-window")
        .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .expect("request");
    let (status, body) = send(&router, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "auth.scope_missing");

    // And unauthenticated is a 401, not an open door.
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/devices/pairing-window")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .expect("request");
    let (status, _) = send(&router, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// One window, one node: a second node cannot ride in on a window the owner
/// opened for the first.
#[tokio::test]
async fn pairing_closes_the_window_behind_it() {
    let (router, _pairing, _store) = harness().await;
    let code = open_window(&router).await;

    let first = a_key();
    let (_, challenge) = start(&router, &first, "room-node", &code).await;
    let (status, _) = complete(&router, &first, &challenge).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = start(&router, &a_key(), "room-node", &code).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "auth.pairing_invalid");
}

/// Re-presenting a key is not a re-pair: the old device must be revoked first,
/// or one key would name two devices and "which device is this?" loses its
/// answer.
#[tokio::test]
async fn the_same_key_cannot_pair_twice() {
    let (router, _pairing, _store) = harness().await;
    let key = a_key();

    let code = open_window(&router).await;
    let (_, challenge) = start(&router, &key, "room-node", &code).await;
    assert_eq!(complete(&router, &key, &challenge).await.0, StatusCode::OK);

    let code = open_window(&router).await;
    let (_, challenge) = start(&router, &key, "room-node", &code).await;
    let (status, body) = complete(&router, &key, &challenge).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "auth.pairing_invalid");
}

/// A node cannot pair into a house with no owner — there would be nobody whose
/// authority it is joining.
#[tokio::test]
async fn a_node_cannot_pair_before_the_owner_does() {
    let store = Arc::new(InMemoryIdentityStore::new());
    let auth = AuthState::bootstrap(store.clone()).await;
    let pairing = PairingState::new();
    let router = router_with(
        AppState::new().with_auth(auth),
        Wiring {
            pairing: pairing.clone(),
            ..Wiring::default()
        },
    );
    // No owner, so no authenticated route can open a window; drive the state
    // directly to isolate the store's refusal.
    let key = a_key();
    let code = {
        // The window opener is owner-gated, so reach past it deliberately.
        let (code, _) = pairing_window_for_test(&pairing);
        code
    };
    let (status, challenge) = start(&router, &key, "room-node", &code).await;
    assert_eq!(status, StatusCode::OK, "the window itself is fine");
    let (status, body) = complete(&router, &key, &challenge).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "auth.pairing_invalid");
    assert!(store.devices().is_empty());
}

fn pairing_window_for_test(pairing: &PairingState) -> (String, std::time::SystemTime) {
    pairing.open_window_for_test(std::time::SystemTime::now())
}
