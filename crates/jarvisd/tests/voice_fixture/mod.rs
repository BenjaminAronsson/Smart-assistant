//! Shared fixtures for the F5.2 voice tests: an in-process Wyoming service that
//! speaks just enough of the protocol to drive one exchange, plus a jarvisd
//! harness wired for voice.
//!
//! Fixture-driven, never live services (CLAUDE.md "fixture-driven tests over
//! live-provider calls, always"). The **real** `jarvis_adapters::wyoming` client
//! is exercised against these — only the speech engines themselves are faked, so
//! the framing, cancellation and error paths under test are production code.
//!
//! Mirrors the fixture-server pattern in `jarvis-adapters/src/wyoming.rs`'s test
//! module (bind `127.0.0.1:0`, answer with the same framing), which is the one
//! place the wire format is defined.

#![allow(dead_code)] // each test target uses a subset of these helpers

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt;
use jarvis_application::testing::FakeModel;
use jarvis_application::voice::{SpeechSynthesizer, SpeechTranscriber};
use jarvis_infra::events::PgEventLog;
use jarvis_infra::messages::PgMessageStore;
use jarvis_infra::runs::PgRunStore;
use jarvis_infra::sessions::PgSessionStore;
use jarvisd::api::{AppState, RunWiring, Wiring, router_with};
use jarvisd::auth::AuthState;
use jarvisd::orchestrator_ports::{PassthroughAssembler, SystemClock};
use jarvisd::runs::{RunApi, RunEngine};
use jarvisd::ws::{WsHub, WsState};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

pub const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";

// ---------------------------------------------------------------------------
// Wyoming wire framing (fixture side)
// ---------------------------------------------------------------------------

pub struct Frame {
    pub msg_type: String,
    pub data: Option<Value>,
    pub payload: Option<Vec<u8>>,
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg_type: &str,
    data: Option<Value>,
    payload: Option<&[u8]>,
) {
    let data_bytes = data.as_ref().map(|d| serde_json::to_vec(d).unwrap());
    let mut header = json!({ "type": msg_type });
    if let Some(bytes) = &data_bytes {
        header["data_length"] = json!(bytes.len());
    }
    if let Some(bytes) = payload {
        header["payload_length"] = json!(bytes.len());
    }
    let mut line = serde_json::to_vec(&header).unwrap();
    line.push(b'\n');
    if writer.write_all(&line).await.is_err() {
        return;
    }
    if let Some(bytes) = &data_bytes
        && writer.write_all(bytes).await.is_err()
    {
        return;
    }
    if let Some(bytes) = payload
        && writer.write_all(bytes).await.is_err()
    {
        return;
    }
    let _ = writer.flush().await;
}

pub async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Option<Frame> {
    let mut line = String::new();
    if reader.read_line(&mut line).await.ok()? == 0 {
        return None;
    }
    let header: Value = serde_json::from_str(line.trim_end()).ok()?;
    let read_exact = async |reader: &mut R, len: usize| -> Option<Vec<u8>> {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await.ok()?;
        Some(buf)
    };
    let data = match header.get("data_length").and_then(Value::as_u64) {
        Some(len) => Some(serde_json::from_slice(&read_exact(reader, len as usize).await?).ok()?),
        None => None,
    };
    let payload = match header.get("payload_length").and_then(Value::as_u64) {
        Some(len) => Some(read_exact(reader, len as usize).await?),
        None => None,
    };
    Some(Frame {
        msg_type: header.get("type")?.as_str()?.to_owned(),
        data,
        payload,
    })
}

/// Bind an ephemeral port and serve every inbound connection with `handler`.
/// Returns `tcp://host:port` — the `[voice]` config shape jarvisd expects.
async fn serve<F, Fut>(handler: F) -> String
where
    F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture");
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            // Match a production speech service on the one thing that matters
            // for the latency harness: no Nagle coalescing on small frames.
            let _ = stream.set_nodelay(true);
            tokio::spawn(handler(stream));
        }
    });
    format!("tcp://{addr}")
}

/// A Wyoming STT service that answers every request with `transcript`, after
/// consuming the audio the client streams (so the timing this measures includes
/// the whole capture → transcript path, not just a connect).
pub async fn stt_returning(transcript: &'static str) -> String {
    serve(move |stream| async move {
        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut writer = write_half;
        while let Some(frame) = read_frame(&mut reader).await {
            if frame.msg_type == "audio-stop" {
                break;
            }
        }
        write_frame(
            &mut writer,
            "transcript",
            Some(json!({ "text": transcript })),
            None,
        )
        .await;
    })
    .await
}

/// A Wyoming STT service that closes mid-stream without ever answering — the
/// "dead service" case that must not look like silence.
pub async fn stt_dying() -> String {
    serve(|stream| async move {
        let (read_half, _write) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        // Read one frame so the connection is genuinely established, then send a
        // truncated header: a broken frame, distinct from a clean close.
        let _ = read_frame(&mut reader).await;
        let mut write = _write;
        let _ = write.write_all(b"{\"type\": \"transcr").await;
        let _ = write.flush().await;
    })
    .await
}

/// A Wyoming TTS service. Emits `chunks` `audio-chunk` frames of `chunk_bytes`
/// each, pausing `gap` between them, then `audio-stop`. A long `gap` models a
/// slow synthesizer, which is what makes barge-in observable.
pub async fn tts_streaming(chunks: usize, chunk_bytes: usize, gap: Duration) -> String {
    serve(move |stream| async move {
        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut writer = write_half;
        let Some(request) = read_frame(&mut reader).await else {
            return;
        };
        if request.msg_type != "synthesize" {
            return;
        }
        write_frame(
            &mut writer,
            "audio-start",
            Some(json!({ "rate": 22_050, "width": 2, "channels": 1 })),
            None,
        )
        .await;
        let pcm = vec![7u8; chunk_bytes];
        for _ in 0..chunks {
            tokio::time::sleep(gap).await;
            write_frame(&mut writer, "audio-chunk", None, Some(&pcm)).await;
        }
        write_frame(&mut writer, "audio-stop", None, None).await;
    })
    .await
}

/// A Wyoming TTS service that accepts the connection and then dies before
/// `audio-start` — a failure the client must surface rather than swallow.
pub async fn tts_dying() -> String {
    serve(|stream| async move {
        let (read_half, _write) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let _ = read_frame(&mut reader).await;
    })
    .await
}

/// Strip the `tcp://` prefix the config shape carries, for direct client use.
pub fn addr_of(url: &str) -> &str {
    url.strip_prefix("tcp://").expect("fixture url is tcp://")
}

// ---------------------------------------------------------------------------
// jarvisd harness
// ---------------------------------------------------------------------------

pub struct Harness {
    pub app: axum::Router,
    pub addr: std::net::SocketAddr,
    pub token: String,
    pub shutdown: CancellationToken,
    pub model: Arc<FakeModel>,
}

pub struct VoiceWiring {
    pub transcriber: Option<Arc<dyn SpeechTranscriber>>,
    pub synthesizer: Option<Arc<dyn SpeechSynthesizer>>,
}

impl Harness {
    /// The whole daemon wired the way `main.rs` wires it for voice: real
    /// Postgres repositories, the real outbox dispatcher, the real WS upgrade,
    /// and — critically — [`DeterministicFirstProvider`] in front of the model,
    /// so a recognized utterance never reaches `model`.
    pub async fn start(pool: PgPool, model: FakeModel, voice: VoiceWiring) -> Self {
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
        let model = Arc::new(model);

        let engine = RunEngine::new(
            Arc::new(
                jarvis_application::deterministic::DeterministicFirstProvider::new(model.clone()),
            ),
            Arc::new(PassthroughAssembler),
            runs.clone(),
            messages.clone(),
            hub.clone(),
            Arc::new(SystemClock),
            shutdown.clone(),
            None, // text-only path; F5.2 wires no tools of its own
        );
        let approval_gate = jarvisd::approvals::JarvisApprovalGate::new(pool.clone());
        let run_api = RunApi::new(
            sessions,
            messages,
            runs,
            events.clone(),
            engine,
            approval_gate,
            None,
        );
        let ws = WsState {
            identity: None,
            connected: Default::default(),
            surfaces: Default::default(),
            audit: None,
            revocations: Default::default(),
            hub,
            events,
            shutdown: shutdown.clone(),
            transcriber: voice.transcriber,
            synthesizer: voice.synthesizer,
            runs: Some(run_api.clone()),
        };

        let dispatch_pool = dispatcher_pool(&pool).await;
        let dispatch_hub = ws.hub.clone();
        let dispatch_cancel = shutdown.clone();
        tokio::spawn(async move {
            let dispatcher = jarvis_infra::dispatcher::OutboxDispatcher::new(dispatch_pool.clone());
            let _ = dispatcher.run(&*dispatch_hub, dispatch_cancel).await;
            // Release the database as soon as the test cancels: `#[sqlx::test]`
            // drops the throwaway DB afterwards and cannot while a session is
            // still connected.
            dispatch_pool.close().await;
        });

        let app = router_with(
            AppState::new().with_auth(auth),
            Wiring {
                runs: Some(RunWiring { runs: run_api, ws }),
                ..Wiring::default()
            },
        );

        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/auth/pair")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"pairingCode":"{code}","deviceName":"hud"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let token = serde_json::from_slice::<Value>(&bytes).unwrap()["deviceToken"]
            .as_str()
            .unwrap()
            .to_owned();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Same as `main.rs`: without this the latency harness measures Nagle's
        // ~40 ms delayed-ACK stall instead of the pipeline.
        let listener = axum::serve::ListenerExt::tap_io(listener, |stream| {
            let _ = stream.set_nodelay(true);
        });
        let serve_app = app.clone();
        tokio::spawn(async move {
            axum::serve(listener, serve_app).await.unwrap();
        });

        Self {
            app,
            addr,
            token,
            shutdown,
            model,
        }
    }

    pub async fn connect(&self) -> VoiceSocket {
        let url = format!("ws://{}/ws/v1", self.addr);
        let mut request = url.into_client_request().unwrap();
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", self.token).parse().unwrap(),
        );
        let (socket, _resp) = connect_async(request).await.expect("ws upgrade");
        // A real browser's WS stack disables Nagle; tokio-tungstenite does not,
        // and without this the client's own `voice.stream.stop` frame sits ~40 ms
        // behind the last PCM frame waiting for a delayed ACK — measuring the
        // test client instead of the daemon.
        if let tokio_tungstenite::MaybeTlsStream::Plain(tcp) = socket.get_ref() {
            let _ = tcp.set_nodelay(true);
        }
        VoiceSocket {
            socket,
            audio_frames: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub async fn timeline(&self) -> Value {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/sessions/{SESSION}/timeline"))
                    .header(header::AUTHORIZATION, format!("Bearer {}", self.token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }
}

/// A pool for the outbox dispatcher that is **not** a child of the `#[sqlx::test]`
/// pool.
///
/// The dispatcher's `PgListener` holds one connection open for the whole test.
/// Test pools are children of a single shared master pool capped at 20
/// connections, so on a 16-core machine sixteen tests running in parallel each
/// parked a permanent LISTEN permit there and left almost nothing for actual
/// queries — every other acquire then waited out sqlx's 30 s timeout and surfaced
/// as a `503 identity store unavailable` from an unrelated request. That is the
/// whole of the ~30 s run-to-run spread and the lone intermittent failure this
/// suite showed; it is a harness artefact, not daemon behaviour.
async fn dispatcher_pool(pool: &PgPool) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .expect("dispatcher pool")
}

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

/// What one received WS frame was.
#[derive(Debug, Clone, PartialEq)]
pub enum Received {
    Event { event_type: String, payload: Value },
    Audio(usize),
}

pub struct VoiceSocket {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    audio_frames: Arc<AtomicUsize>,
}

impl VoiceSocket {
    pub async fn send_control(&mut self, control: Value) {
        use futures_util::SinkExt;
        self.socket
            .send(WsMessage::Text(control.to_string().into()))
            .await
            .unwrap();
    }

    pub async fn send_pcm(&mut self, bytes: Vec<u8>) {
        use futures_util::SinkExt;
        self.socket
            .send(WsMessage::Binary(bytes.into()))
            .await
            .unwrap();
    }

    pub fn start_stream(stream_id: &str, session_id: Option<&str>) -> Value {
        json!({
            "type": "voice.stream.start",
            "streamId": stream_id,
            "sessionId": session_id,
            "sampleRateHz": 16_000,
            "sampleWidthBytes": 2,
            "channels": 1,
        })
    }

    pub fn stop_stream(stream_id: &str) -> Value {
        json!({ "type": "voice.stream.stop", "streamId": stream_id })
    }

    /// Collect frames until `predicate` accepts one or `budget` elapses. Every
    /// wait in these tests is bounded so a regression fails rather than hangs
    /// (the `bounded()` discipline from the wyoming.rs tests).
    pub async fn collect_until(
        &mut self,
        budget: Duration,
        predicate: impl Fn(&Received) -> bool,
    ) -> Vec<Received> {
        self.collect_timed(budget, predicate)
            .await
            .into_iter()
            .map(|(_, received)| received)
            .collect()
    }

    /// As [`Self::collect_until`], but stamping each frame with the instant it
    /// arrived. The latency harness needs per-frame arrival times: timing them
    /// from the point the batch is *processed* collapses everything in one batch
    /// to zero, which would silently report a fictitious 0 ms.
    pub async fn collect_timed(
        &mut self,
        budget: Duration,
        predicate: impl Fn(&Received) -> bool,
    ) -> Vec<(std::time::Instant, Received)> {
        use futures_util::StreamExt;
        let mut seen = Vec::new();
        let deadline = tokio::time::sleep(budget);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return seen,
                frame = self.socket.next() => match frame {
                    Some(Ok(WsMessage::Text(text))) => {
                        let at = std::time::Instant::now();
                        let value: Value = serde_json::from_str(&text).unwrap();
                        let received = Received::Event {
                            event_type: value["type"].as_str().unwrap_or_default().to_owned(),
                            payload: value["payload"].clone(),
                        };
                        let stop = predicate(&received);
                        seen.push((at, received));
                        if stop {
                            return seen;
                        }
                    }
                    Some(Ok(WsMessage::Binary(bytes))) => {
                        let at = std::time::Instant::now();
                        self.audio_frames.fetch_add(1, Ordering::SeqCst);
                        let received = Received::Audio(bytes.len());
                        let stop = predicate(&received);
                        seen.push((at, received));
                        if stop {
                            return seen;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return seen,
                }
            }
        }
    }

    /// Drain for exactly `window`, returning everything that arrived. Used to
    /// assert an *absence* (no audio after barge-in) rather than a presence.
    pub async fn drain_for(&mut self, window: Duration) -> Vec<Received> {
        self.collect_until(window, |_| false).await
    }

    /// Whether the daemon closes this socket within `budget`. A socket loop
    /// that is wedged can never poll its shutdown branch, so this is how a
    /// graceful-drain regression fails within a bound instead of hanging.
    pub async fn closed_within(&mut self, budget: Duration) -> bool {
        use futures_util::StreamExt;
        let deadline = tokio::time::sleep(budget);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return false,
                frame = self.socket.next() => match frame {
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => return true,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

pub fn events_of(received: &[Received]) -> Vec<&str> {
    received
        .iter()
        .filter_map(|r| match r {
            Received::Event { event_type, .. } => Some(event_type.as_str()),
            Received::Audio(_) => None,
        })
        .collect()
}

pub fn audio_frame_count(received: &[Received]) -> usize {
    received
        .iter()
        .filter(|r| matches!(r, Received::Audio(_)))
        .count()
}

pub fn payload_of<'a>(received: &'a [Received], event_type: &str) -> Option<&'a Value> {
    received.iter().find_map(|r| match r {
        Received::Event {
            event_type: t,
            payload,
        } if t == event_type => Some(payload),
        _ => None,
    })
}
