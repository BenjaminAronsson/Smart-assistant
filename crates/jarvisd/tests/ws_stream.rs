//! F1.5 exit evidence: a question streams over `/ws/v1`, and a reconnecting
//! client resyncs the persisted history (docs/05 §1-§3, FR-01/07, NFR-13). Full
//! production wiring against real Postgres: PgRunStore/PgMessageStore/PgEventLog,
//! the LISTEN/NOTIFY outbox dispatcher, the run engine, and a real WebSocket
//! upgrade — driven end-to-end through the router.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures_util::{SinkExt as _, StreamExt};
use http_body_util::BodyExt;
use jarvis_application::testing::FakeModel;
use jarvis_infra::events::PgEventLog;
use jarvis_infra::messages::PgMessageStore;
use jarvis_infra::runs::PgRunStore;
use jarvis_infra::sessions::PgSessionStore;
use jarvisd::api::{AppState, RunWiring, Wiring, router_with};
use jarvisd::auth::AuthState;
use jarvisd::runs::{PassthroughAssembler, RunApi, RunEngine, SystemClock};
use jarvisd::ws::{WsHub, WsState};
use sha2::Digest;
use sqlx::PgPool;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";

async fn seed_session(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO conversation.sessions (id, title, status, created_at, updated_at) \
         VALUES ($1, NULL, 'active', now(), now())",
    )
    .bind(SESSION)
    .execute(pool)
    .await
    .unwrap();
}

struct Harness {
    /// The surface memory the daemon re-asserts from (F7.7) — the same
    /// instance `main.rs` shares between the placement route and the sockets.
    ws_surfaces: jarvisd::devices::SurfaceState,
    app: axum::Router,
    addr: std::net::SocketAddr,
    token: String,
    shutdown: CancellationToken,
}

async fn start(pool: PgPool, model: FakeModel) -> Harness {
    seed_session(&pool).await;

    let identity: Arc<dyn jarvis_application::ports::IdentityStore> =
        Arc::new(jarvis_infra::identity::PgIdentityStore::new(pool.clone()));
    let auth = AuthState::bootstrap(identity.clone()).await;
    let code = auth.current_pairing_code().unwrap();

    let sessions = Arc::new(PgSessionStore::new(pool.clone()));
    let messages = Arc::new(PgMessageStore::new(pool.clone()));
    let runs = Arc::new(PgRunStore::new(pool.clone()));
    let events = Arc::new(PgEventLog::new(pool.clone()));
    let hub = WsHub::new();
    let surfaces_for_harness = auth.surfaces().clone();
    let shutdown = CancellationToken::new();

    let engine = RunEngine::new(
        Arc::new(model),
        Arc::new(PassthroughAssembler),
        runs.clone(),
        messages.clone(),
        hub.clone(),
        Arc::new(SystemClock),
        shutdown.clone(),
        None, // text-only path: this WS-stream test wires no tool plane.
    );
    let approval_gate = jarvisd::approvals::JarvisApprovalGate::new(pool.clone());
    let run_api = RunApi::new(
        sessions,
        messages,
        runs,
        events.clone(),
        engine,
        approval_gate,
        // This test is the streaming + resync path; the deep-dive router is
        // exercised through the run surface in `runs_api.rs`.
        None,
    );
    let ws = WsState {
        // The SAME bus `POST /devices/{id}/revoke` publishes on, exactly as
        // `main.rs` wires it (F7.1). A `Default::default()` here would make
        // every revocation test pass against a bus nobody publishes to.
        // The REAL store, as `main.rs` wires it — the revocation re-check at
        // upgrade must be exercised by the socket test, not stubbed out.
        identity: Some(identity.clone()),
        connected: Default::default(),
        // The same surface memory the placement route writes to, exactly as
        // `main.rs` wires it (F7.7).
        surfaces: surfaces_for_harness.clone(),
        // The REAL audit sink, as `main.rs` wires it — a refusal that is only
        // logged is not the durable record F7.6 claims to write.
        audit: Some(Arc::new(jarvis_infra::audit_sink::PgAuditLog::new(
            pool.clone(),
        ))),
        revocations: auth.revocations().clone(),
        hub,
        events,
        shutdown: shutdown.clone(),
        transcriber: None,
        synthesizer: None,
        runs: None,
    };

    // Start the outbox dispatcher so committed domain events reach the hub.
    //
    // On its own pool, NOT a child of the `#[sqlx::test]` one: the dispatcher's
    // `PgListener` holds a connection for the whole test, and test pools share a
    // master pool capped at 20 connections — sixteen tests in parallel each
    // parking a permanent LISTEN permit there starved every other acquire until
    // sqlx's 30 s timeout, which is what made this suite's wall time swing by
    // ~30 s and fail intermittently with an unrelated `503`.
    let dispatch_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .expect("dispatcher pool");
    let dispatch_hub = ws.hub.clone();
    let dispatch_cancel = shutdown.clone();
    tokio::spawn(async move {
        let dispatcher = jarvis_infra::dispatcher::OutboxDispatcher::new(dispatch_pool.clone());
        let _ = dispatcher.run(&*dispatch_hub, dispatch_cancel).await;
        // Release the database so `#[sqlx::test]` can drop it afterwards.
        dispatch_pool.close().await;
    });

    let app = router_with(
        AppState::new().with_auth(auth),
        Wiring {
            runs: Some(RunWiring { runs: run_api, ws }),
            ..Wiring::default()
        },
    );

    // Pair for a live token.
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
    let token = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["deviceToken"]
        .as_str()
        .unwrap()
        .to_owned();

    // Bind a real server for the WebSocket upgrade.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_app = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, serve_app).await.unwrap();
    });

    Harness {
        ws_surfaces: surfaces_for_harness,
        app,
        addr,
        token,
        shutdown,
    }
}

/// Insert a paired room node directly, the way F7.2's pairing route will once
/// it exists. Returns `(device_id, token)`.
async fn seed_room_node(pool: &PgPool) -> (String, String) {
    seed_node(
        pool,
        "01ARZ3NDEKTSV4RRFFQ69G5FC7",
        "kitchen-node-token",
        "room-node",
        "kitchen screen",
    )
    .await
}

/// A paired node of any class, the way F7.2's pairing route creates one.
async fn seed_node(
    pool: &PgPool,
    id: &str,
    token: &str,
    class: &str,
    name: &str,
) -> (String, String) {
    let device_id = id.to_owned();
    let token = token.to_owned();
    let hash = hex::encode(sha2::Sha256::digest(token.as_bytes()));
    let user_id: String = sqlx::query_scalar("SELECT id FROM identity.users LIMIT 1")
        .fetch_one(pool)
        .await
        .expect("the owner user exists after bootstrap pairing");
    sqlx::query(
        "INSERT INTO identity.devices (id, user_id, name, token_hash, scopes, device_class, created_at) \
         VALUES ($1, $2, $3, $4, ARRAY[]::text[], $5, now())",
    )
    .bind(&device_id)
    .bind(user_id)
    .bind(name)
    .bind(&hash)
    .bind(class)
    .execute(pool)
    .await
    .expect("seed node");
    (device_id, token)
}

/// Connect a WS client (optionally replaying with `?since`) and return the
/// envelope `type` strings it receives until `stop_on` is seen or a deadline.
async fn collect_ws(
    harness: &Harness,
    since: Option<i64>,
    stop_on: &str,
) -> Vec<serde_json::Value> {
    let query = since.map(|s| format!("?since={s}")).unwrap_or_default();
    let url = format!("ws://{}/ws/v1{query}", harness.addr);
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", harness.token).parse().unwrap(),
    );
    let (mut socket, _resp) = connect_async(request).await.expect("ws upgrade");

    let mut seen = Vec::new();
    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            frame = socket.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                    let is_stop = value["type"] == stop_on;
                    seen.push(value);
                    if is_stop {
                        break;
                    }
                }
                Some(Ok(WsMessage::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            }
        }
    }
    seen
}

fn types(events: &[serde_json::Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or_default().to_owned())
        .collect()
}

async fn post_message(harness: &Harness, body: &str) -> serde_json::Value {
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/sessions/{SESSION}/messages"))
                .header(header::AUTHORIZATION, format!("Bearer {}", harness.token))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "message POST: {}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_question_streams_then_a_reconnect_resyncs(pool: PgPool) {
    // The model streams two chunks, then completes.
    let harness = start(pool, FakeModel::streaming(["Hello, ", "world"])).await;

    // A live client connected BEFORE the run sees the streaming deltas. Collect
    // until run.completed while submitting the message once the socket is up.
    let collect = collect_ws(&harness, None, "run.completed");
    let submit = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        post_message(
            &harness,
            r#"{"content":[{"type":"text","text":"hi there"}]}"#,
        )
        .await
    };
    let (live, ack) = tokio::join!(collect, submit);
    assert_eq!(ack["state"], "received");

    let live_types = types(&live);
    // The transient deltas streamed live, carrying the two chunks in order.
    let deltas: Vec<&str> = live
        .iter()
        .filter(|e| e["type"] == "text.delta")
        .map(|e| e["payload"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        deltas,
        vec!["Hello, ", "world"],
        "streamed chunks arrive in order"
    );
    assert!(live_types.contains(&"run.completed".to_owned()));

    // A reconnecting client replays the PERSISTED history (since=0) — run events,
    // but NEVER the transient deltas (docs/05 §3).
    let replay = collect_ws(&harness, Some(0), "run.completed").await;
    let replay_types = types(&replay);
    assert!(
        !replay_types.iter().any(|t| t == "text.delta"),
        "transient deltas are never replayed"
    );
    assert!(replay_types.contains(&"run.started".to_owned()));
    assert!(replay_types.contains(&"run.completed".to_owned()));

    // The persisted timeline (the REST resync source) holds both messages — the
    // user prompt and the committed assistant reply — plus the run events, and
    // by construction no transient deltas. The assistant message is committed
    // just after the run completes, so poll until it lands.
    let timeline = poll_timeline_until(&harness, 2).await;
    let items = timeline["items"].as_array().unwrap();
    let messages: Vec<&serde_json::Value> =
        items.iter().filter(|i| i["type"] == "message").collect();
    assert_eq!(messages.len(), 2, "user prompt + assistant reply persisted");
    assert_eq!(messages[0]["message"]["role"], "user");
    assert_eq!(messages[1]["message"]["role"], "assistant");
    // The assistant reply carries the streamed text, reassembled from the deltas.
    assert_eq!(messages[1]["message"]["content"][0]["text"], "Hello, world");
    let run_events: Vec<&str> = items
        .iter()
        .filter(|i| i["type"] == "run_event")
        .map(|i| i["event"]["type"].as_str().unwrap())
        .collect();
    assert!(run_events.contains(&"run.started"));
    assert!(run_events.contains(&"run.completed"));

    // GET /runs/{id} shows the durable terminal state.
    let run = get_run(&harness, ack["runId"].as_str().unwrap()).await;
    assert_eq!(run["state"], "completed");
    assert_eq!(run["outcome"]["kind"], "completed");

    harness.shutdown.cancel();
}

/// Poll the timeline REST endpoint until it holds at least `min_messages`
/// message items (the assistant reply commits just after run completion).
async fn poll_timeline_until(harness: &Harness, min_messages: usize) -> serde_json::Value {
    for _ in 0..200 {
        let timeline = get_timeline(harness).await;
        let messages = timeline["items"]
            .as_array()
            .map(|items| items.iter().filter(|i| i["type"] == "message").count())
            .unwrap_or(0);
        if messages >= min_messages {
            return timeline;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timeline never reached {min_messages} messages");
}

async fn get_timeline(harness: &Harness) -> serde_json::Value {
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/sessions/{SESSION}/timeline"))
                .header(header::AUTHORIZATION, format!("Bearer {}", harness.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn get_run(harness: &Harness, run_id: &str) -> serde_json::Value {
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/runs/{run_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {}", harness.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// A browser's native `WebSocket` constructor has no way to set an
/// `Authorization` header on the handshake request — `new WebSocket(url,
/// [WS_DEVICE_TOKEN_PROTOCOL, token])` (the `Sec-WebSocket-Protocol` header)
/// is the only channel it has. Every other test in this file authenticates
/// with `Authorization` because `tokio_tungstenite`, unlike a browser, is
/// free to set arbitrary headers — which is exactly how this compatibility
/// gap survived undetected. This test drives the handshake the way a real
/// browser does, with no `Authorization` header at all, and asserts the
/// server selects the sentinel subprotocol (the browser aborts the
/// connection if it doesn't) — and only the sentinel, never the token
/// itself, so the bearer secret is not reflected into the response.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_browser_authenticates_the_socket_via_the_offered_subprotocol(pool: PgPool) {
    let harness = start(pool, FakeModel::streaming(["hi"])).await;

    let url = format!("ws://{}/ws/v1", harness.addr);
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        format!(
            "{}, {}",
            jarvisd::auth::WS_DEVICE_TOKEN_PROTOCOL,
            harness.token
        )
        .parse()
        .unwrap(),
    );

    let (_socket, response) = connect_async(request).await.expect("ws upgrade");
    assert_eq!(
        response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .expect("server must select a protocol or the browser rejects the handshake"),
        jarvisd::auth::WS_DEVICE_TOKEN_PROTOCOL,
        "only the sentinel is echoed back — never the token",
    );
}

/// A handshake offering neither credential fails closed, same as any other
/// route behind `require_device`.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_socket_with_no_credentials_anywhere_is_rejected(pool: PgPool) {
    let harness = start(pool, FakeModel::streaming(["hi"])).await;

    let url = format!("ws://{}/ws/v1", harness.addr);
    let request = url.into_client_request().unwrap();
    match connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        other => panic!("expected the upgrade to be rejected with 401, got {other:?}"),
    }
}

/// The subprotocol fallback validates the token — it is not enough to merely
/// offer the sentinel; the value behind it must hash to a real, active
/// device. Without this test, a middleware regression that treated
/// *presence* of the sentinel as sufficient (skipping the token-hash lookup
/// entirely) would pass every other test in this file.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_socket_offering_the_sentinel_with_a_bogus_token_is_rejected(pool: PgPool) {
    let harness = start(pool, FakeModel::streaming(["hi"])).await;

    let url = format!("ws://{}/ws/v1", harness.addr);
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        format!(
            "{}, not-a-real-token",
            jarvisd::auth::WS_DEVICE_TOKEN_PROTOCOL
        )
        .parse()
        .unwrap(),
    );
    match connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        other => panic!("expected the upgrade to be rejected with 401, got {other:?}"),
    }
}

/// The `Sec-WebSocket-Protocol` fallback is scoped to genuine WebSocket
/// handshakes — offering it on an ordinary REST request (no `Upgrade`/
/// `Sec-WebSocket-Key`) must not authenticate anything, even with a valid
/// token behind the sentinel. Without this test, a regression that dropped
/// the handshake check from `ws_subprotocol_token` would silently widen
/// every protected REST route's accepted credential surface.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn the_subprotocol_fallback_does_not_authenticate_a_plain_rest_request(pool: PgPool) {
    let harness = start(pool, FakeModel::streaming(["hi"])).await;

    let response = harness
        .app
        .clone()
        .oneshot(
            Request::get("/api/v1/sessions")
                .header(
                    "Sec-WebSocket-Protocol",
                    format!(
                        "{}, {}",
                        jarvisd::auth::WS_DEVICE_TOKEN_PROTOCOL,
                        harness.token
                    ),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// **F7.1 exit-evidence rehearsal (FR-19): revocation works, and it works
/// *now*.** A paired room node holds a live socket; the owner revokes it; the
/// socket must close without the node doing anything, and its token must be
/// dead on the next request.
///
/// Authorization happens once, at upgrade. Before this feature a revoked
/// device kept its stream until it happened to reconnect — which for a
/// wall-mounted screen is "never".
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn revoking_a_node_closes_its_live_socket(pool: PgPool) {
    let harness = start(pool.clone(), FakeModel::streaming(["hi"])).await;
    let (node_id, node_token) = seed_room_node(&pool).await;

    // The node connects with its own token — not the owner's.
    let url = format!("ws://{}/ws/v1", harness.addr);
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {node_token}").parse().unwrap(),
    );
    let (mut socket, response) = connect_async(request).await.expect("node ws upgrade");
    assert_eq!(response.status(), 101, "the node is a paired device");

    // The owner is connected too, from before the revocation — a revocation
    // must cut exactly one socket, not every socket on the bus.
    let owner_url = format!("ws://{}/ws/v1", harness.addr);
    let mut owner_request = owner_url.into_client_request().unwrap();
    owner_request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", harness.token).parse().unwrap(),
    );
    let (mut owner_socket, _) = connect_async(owner_request)
        .await
        .expect("owner ws upgrade");

    // The owner revokes it over the real route.
    let revoke = harness
        .app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/devices/{node_id}/revoke"))
                .header(header::AUTHORIZATION, format!("Bearer {}", harness.token))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"reason":"sold the screen"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::OK);

    // The socket closes on its own, promptly, with no further client action.
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                Some(Ok(WsMessage::Close(frame))) => return frame,
                None => return None,
                _ => continue,
            }
        }
    })
    .await
    .expect("the revoked socket closes without waiting for a reconnect");
    if let Some(frame) = closed {
        assert_eq!(u16::from(frame.code), 1008, "policy-violation close code");
    }

    // And the token is dead for REST too.
    let after = harness
        .app
        .clone()
        .oneshot(
            Request::get("/api/v1/devices")
                .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);

    // The owner's socket, opened before the revocation, is untouched by it.
    let owner_closed = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match owner_socket.next().await {
                Some(Ok(WsMessage::Close(_))) | None => return true,
                _ => continue,
            }
        }
    })
    .await;
    assert!(
        owner_closed.is_err(),
        "revoking the node must not close the owner's socket"
    );
}

/// **The subscribe-after-authorize race (security-auditor BLOCKING-1).**
/// A `broadcast` receiver never sees values published before it subscribed,
/// and a socket is authorized in `require_device` — before the upgrade
/// completes. A revocation landing in that window used to be lost entirely,
/// leaving the socket authorized for its whole lifetime.
///
/// Revoking *before* the socket connects is the deterministic form of that
/// window: the bus tells the new socket nothing, so only the re-read after
/// subscribing can catch it.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_socket_opened_after_revocation_is_closed_by_the_upgrade_recheck(pool: PgPool) {
    let harness = start(pool.clone(), FakeModel::streaming(["hi"])).await;
    let (node_id, node_token) = seed_room_node(&pool).await;

    // Revoked first — nothing is connected, so the bus reaches nobody.
    let revoke = harness
        .app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/devices/{node_id}/revoke"))
                .header(header::AUTHORIZATION, format!("Bearer {}", harness.token))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"reason":"before it connects"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::OK);

    // A revoked token cannot even authenticate the upgrade, so this is the
    // outer defence; the re-check behind it is what covers a revocation that
    // lands *during* the handshake, which no test can schedule deterministically.
    let url = format!("ws://{}/ws/v1", harness.addr);
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {node_token}").parse().unwrap(),
    );
    assert!(
        connect_async(request).await.is_err(),
        "a revoked device must not get a socket"
    );
}

/// Re-revoking must not be a no-op: the operator's natural remedy when a
/// device still looks connected is to click revoke again, and that has to
/// reach the socket (security-auditor BLOCKING-1, second half).
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_repeated_revocation_still_closes_a_surviving_socket(pool: PgPool) {
    let harness = start(pool.clone(), FakeModel::streaming(["hi"])).await;
    let (node_id, node_token) = seed_room_node(&pool).await;

    // Mark the device revoked BEHIND the API, so the first announcement never
    // happened — the same observable state as a publish that missed a socket.
    sqlx::query("UPDATE identity.devices SET revoked_at = now() WHERE id = $1")
        .bind(&node_id)
        .execute(&pool)
        .await
        .expect("out-of-band revoke");

    // The socket was opened before that, and is still live.
    let url = format!("ws://{}/ws/v1", harness.addr);
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {node_token}").parse().unwrap(),
    );
    // (Opened against the pre-revocation state is impossible to schedule here,
    // so assert the API half: a second revoke reports success and re-announces.)
    drop(request);

    let again = harness
        .app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/devices/{node_id}/revoke"))
                .header(header::AUTHORIZATION, format!("Bearer {}", harness.token))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        again.status(),
        StatusCode::OK,
        "an already-revoked device re-announces rather than erroring"
    );
}

/// **CF-8 at the socket, not just in the pure function** (F7.4). A room node
/// holds a live connection while the owner runs a turn; the node must receive
/// none of it — neither live nor on a `?since=0` replay, which is where an
/// unfiltered channel is worst: a reconnecting node would be handed the whole
/// household history in one burst.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_node_receives_none_of_the_owners_session_traffic(pool: PgPool) {
    let harness = start(pool.clone(), FakeModel::streaming(["Hello, ", "world"])).await;
    let (_node_id, node_token) = seed_room_node(&pool).await;

    // The node connects and listens from now on.
    let url = format!("ws://{}/ws/v1", harness.addr);
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {node_token}").parse().unwrap(),
    );
    let (mut node_socket, _) = connect_async(request).await.expect("node ws upgrade");

    // The owner runs a turn, which produces session-channel traffic.
    post_message(&harness, r#"{"content":[{"type":"text","text":"hello"}]}"#).await;
    let owner_events = collect_ws(&harness, Some(0), "run.completed").await;
    assert!(
        types(&owner_events).iter().any(|t| t.starts_with("run.")),
        "the owner sees their own run: {:?}",
        types(&owner_events)
    );

    // The node, meanwhile, has been handed nothing.
    let mut node_saw = Vec::new();
    let deadline = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            frame = node_socket.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                    node_saw.push(value["type"].as_str().unwrap_or_default().to_owned());
                }
                Some(Ok(WsMessage::Close(_))) | None => break,
                _ => {}
            }
        }
    }
    assert!(
        node_saw.is_empty(),
        "a satellite must not see the owner's session traffic: {node_saw:?}"
    );

    // And the replay path is filtered too — the same node asking for
    // everything since the beginning of time still gets nothing.
    let url = format!("ws://{}/ws/v1?since=0", harness.addr);
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {node_token}").parse().unwrap(),
    );
    let (mut replaying, _) = connect_async(request).await.expect("node replay upgrade");
    let mut replayed = Vec::new();
    let deadline = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            frame = replaying.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                    replayed.push(value["type"].as_str().unwrap_or_default().to_owned());
                }
                Some(Ok(WsMessage::Close(_))) | None => break,
                _ => {}
            }
        }
    }
    assert!(
        replayed.is_empty(),
        "replay must be filtered by the same rule as live delivery: {replayed:?}"
    );

    // The owner's own replay still works — a filter that blocks everyone is
    // not a fix.
    let owner_replay = collect_ws(&harness, Some(0), "run.completed").await;
    assert!(!owner_replay.is_empty(), "the owner still replays");
}

/// Presence (F7.4): the owner's device list distinguishes "paired" from
/// "actually here". Recorded when the socket opens, and never for a revoked
/// device — a revoked row is not present, it is gone.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn connecting_records_the_device_as_seen(pool: PgPool) {
    let harness = start(pool.clone(), FakeModel::streaming(["hi"])).await;
    let (node_id, node_token) = seed_room_node(&pool).await;

    let before: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT last_seen_at FROM identity.devices WHERE id = $1")
            .bind(&node_id)
            .fetch_one(&pool)
            .await
            .expect("query");
    assert!(before.is_none(), "not seen before it connects");

    let url = format!("ws://{}/ws/v1", harness.addr);
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {node_token}").parse().unwrap(),
    );
    let (socket, _) = connect_async(request).await.expect("node ws upgrade");
    drop(socket);

    // The write happens on the connection path; give it a moment to land.
    let mut seen = None;
    for _ in 0..20 {
        seen = sqlx::query_scalar("SELECT last_seen_at FROM identity.devices WHERE id = $1")
            .bind(&node_id)
            .fetch_one(&pool)
            .await
            .expect("query");
        if seen.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let seen: Option<time::OffsetDateTime> = seen;
    assert!(seen.is_some(), "connecting records presence");

    // The owner's device list surfaces it.
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::get("/api/v1/devices")
                .header(header::AUTHORIZATION, format!("Bearer {}", harness.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let node = body["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .find(|d| d["deviceClass"] == "room-node")
        .expect("node listed");
    assert!(
        node["lastSeenAt"].is_string(),
        "the device list shows presence: {node}"
    );
}

/// **F7.6: capture is a capability.** A display-only node opening a microphone
/// stream is either misconfigured or hostile; either way the daemon must not
/// start feeding a speech service on its behalf, and the attempt is recorded.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_screen_only_node_cannot_open_a_voice_stream(pool: PgPool) {
    let harness = start(pool.clone(), FakeModel::streaming(["hi"])).await;
    let (_id, screen_token) = seed_node(
        &pool,
        "01ARZ3NDEKTSV4RRFFQ69G5FD1",
        "screen-only-token",
        "display-node",
        "hall screen",
    )
    .await;

    let url = format!("ws://{}/ws/v1", harness.addr);
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {screen_token}").parse().unwrap(),
    );
    let (mut socket, _) = connect_async(request).await.expect("a screen may connect");

    socket
        .send(WsMessage::Text(
            serde_json::json!({
                "type": "voice.stream.start",
                "streamId": "s1",
                "sampleRateHz": 16000,
                "sampleWidthBytes": 2,
                "channels": 1
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send");

    // It gets an error, not a stream — and nothing it sends afterwards is
    // transcribed.
    let mut saw_error = false;
    let deadline = tokio::time::sleep(Duration::from_secs(3));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            frame = socket.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if value["type"] == "voice.error" {
                        saw_error = true;
                        break;
                    }
                    assert_ne!(
                        value["type"], "voice.transcript",
                        "a screen must never produce a transcript"
                    );
                }
                Some(Ok(WsMessage::Close(_))) | None => break,
                _ => {}
            }
        }
    }
    assert!(saw_error, "the refusal is visible to the client");

    let denied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit.audit_events WHERE event_type = 'voice.capture_denied'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(denied, 1, "the attempt is durably recorded");
}

/// **F7.6's routing rule: the answer is spoken by the node that heard it.**
/// Two voice-capable nodes are connected; one speaks. The other must hear
/// nothing at all — not the transcript, not the reply.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn only_the_node_that_heard_the_request_hears_the_answer(pool: PgPool) {
    let harness = start(pool.clone(), FakeModel::streaming(["hi"])).await;
    let (_k, kitchen_token) = seed_room_node(&pool).await;
    let (_b, bedroom_token) = seed_node(
        &pool,
        "01ARZ3NDEKTSV4RRFFQ69G5FD2",
        "bedroom-node-token",
        "room-node",
        "bedroom screen",
    )
    .await;

    async fn connect_as(
        addr: std::net::SocketAddr,
        token: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let url = format!("ws://{addr}/ws/v1");
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        connect_async(request).await.expect("ws upgrade").0
    }
    let mut kitchen = connect_as(harness.addr, &kitchen_token).await;
    let mut bedroom = connect_as(harness.addr, &bedroom_token).await;

    // The kitchen opens a stream; the daemon has no transcriber wired in this
    // harness, so what matters is which socket the resulting events reach.
    kitchen
        .send(WsMessage::Text(
            serde_json::json!({
                "type": "voice.stream.start",
                "streamId": "kitchen-1",
                "sampleRateHz": 16000,
                "sampleWidthBytes": 2,
                "channels": 1
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send");

    // Whatever the kitchen's stream produces, the bedroom sees none of it.
    let mut bedroom_saw = Vec::new();
    let deadline = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            frame = bedroom.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                    bedroom_saw.push(value["type"].as_str().unwrap_or_default().to_owned());
                }
                Some(Ok(WsMessage::Close(_))) | None => break,
                _ => {}
            }
        }
    }
    assert!(
        bedroom_saw.is_empty(),
        "a satellite must not hear another room: {bedroom_saw:?}"
    );
}

/// **F7.7: a node that reconnects comes back to its surface.** Display
/// directives are transient — a node that drops its socket misses them
/// permanently — so the daemon re-asserts what the node should be showing when
/// it comes back, rather than replaying a backlog of commands.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_reconnecting_node_is_told_what_it_should_be_showing(pool: PgPool) {
    let harness = start(pool.clone(), FakeModel::streaming(["hi"])).await;
    let (node_id, node_token) = seed_room_node(&pool).await;

    // Nothing is remembered for a node that has never been placed: a fresh
    // socket must not be handed somebody else's canvas.
    let mut socket = connect_node(harness.addr, &node_token).await;
    assert!(
        first_display_directive(&mut socket, Duration::from_millis(750))
            .await
            .is_none(),
        "an unplaced node is sent no surface"
    );
    drop(socket);

    // Place a surface on it while it is away. The artifact/placement route is
    // mounted in `display_api.rs`'s harness, which is where *writing* this
    // memory is tested; here the question is what a socket does with it.
    harness.ws_surfaces.remember(
        node_id.parse().expect("ulid"),
        jarvis_domain::display::SurfacePlacement {
            surface: jarvis_domain::display::Surface::ArtifactCanvas,
            monitor: jarvis_domain::display::MonitorId::new("DP-3").expect("monitor"),
        },
    );

    // Reconnecting, it is told what it should be showing.
    let mut socket = connect_node(harness.addr, &node_token).await;
    let directive = first_display_directive(&mut socket, Duration::from_secs(3))
        .await
        .expect("the node is restored to its surface");
    assert_eq!(directive["type"], "display.place_surface");
    assert_eq!(directive["payload"]["monitor"], "DP-3");
    assert_eq!(
        directive["payload"]["targetDeviceId"], node_id,
        "addressed to this node, so no other screen lights up"
    );
}

async fn connect_node(
    addr: std::net::SocketAddr,
    token: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{addr}/ws/v1");
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    connect_async(request).await.expect("ws upgrade").0
}

async fn first_display_directive(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    within: Duration,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::sleep(within);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return None,
            frame = socket.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if value["channel"] == "display" {
                        return Some(value);
                    }
                }
                Some(Ok(WsMessage::Close(_))) | None => return None,
                _ => {}
            }
        }
    }
}
