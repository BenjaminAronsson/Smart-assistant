//! F1.5 exit evidence: a question streams over `/ws/v1`, and a reconnecting
//! client resyncs the persisted history (docs/05 §1-§3, FR-01/07, NFR-13). Full
//! production wiring against real Postgres: PgRunStore/PgMessageStore/PgEventLog,
//! the LISTEN/NOTIFY outbox dispatcher, the run engine, and a real WebSocket
//! upgrade — driven end-to-end through the router.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use jarvis_application::testing::FakeModel;
use jarvis_infra::events::PgEventLog;
use jarvis_infra::messages::PgMessageStore;
use jarvis_infra::runs::PgRunStore;
use jarvis_infra::sessions::PgSessionStore;
use jarvisd::api::{AppState, RunWiring, router_with};
use jarvisd::auth::AuthState;
use jarvisd::runs::{PassthroughAssembler, RunApi, RunEngine, SystemClock};
use jarvisd::ws::{WsHub, WsState};
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
    app: axum::Router,
    addr: std::net::SocketAddr,
    token: String,
    shutdown: CancellationToken,
}

async fn start(pool: PgPool, model: FakeModel) -> Harness {
    seed_session(&pool).await;

    let identity = Arc::new(jarvis_infra::identity::PgIdentityStore::new(pool.clone()));
    let auth = AuthState::bootstrap(identity).await;
    let code = auth.current_pairing_code().unwrap();

    let sessions = Arc::new(PgSessionStore::new(pool.clone()));
    let messages = Arc::new(PgMessageStore::new(pool.clone()));
    let runs = Arc::new(PgRunStore::new(pool.clone()));
    let events = Arc::new(PgEventLog::new(pool.clone()));
    let hub = WsHub::new();
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
    );
    let ws = WsState {
        hub,
        events,
        shutdown: shutdown.clone(),
    };

    // Start the outbox dispatcher so committed domain events reach the hub.
    let dispatch_pool = pool.clone();
    let dispatch_hub = ws.hub.clone();
    let dispatch_cancel = shutdown.clone();
    tokio::spawn(async move {
        let dispatcher = jarvis_infra::dispatcher::OutboxDispatcher::new(dispatch_pool);
        let _ = dispatcher.run(&*dispatch_hub, dispatch_cancel).await;
    });

    let app = router_with(
        AppState::new().with_auth(auth),
        None,
        Some(RunWiring { runs: run_api, ws }),
        None,
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
        app,
        addr,
        token,
        shutdown,
    }
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
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
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
