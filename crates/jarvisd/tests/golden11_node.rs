//! **Golden 11 — M7 exit evidence (FR-19).** A second node pairs, receives a
//! surface, performs a voice/display flow, and revocation cuts it off.
//!
//! Everything here is the real thing. Real Postgres, the production router, a
//! real rustls listener serving a real certificate, real HTTP over TLS with
//! the certificate **pinned** the way a node pins it, the real pairing route
//! with a real Ed25519 keypair, and a real WebSocket. The only substitution is
//! the artifact store, because what artifact exists is not what this milestone
//! is proving.
//!
//! That is deliberate and it is the point. This project's most expensive
//! recurring bug is the fixture that builds its inputs *its own way* and so
//! agrees with nothing (M5 ×3, and the M6 gate's B1). M7's exit evidence *is*
//! the caller — "a second node pairs" is a claim about the pairing route, the
//! TLS handshake, and the scope set a node actually receives — so a fixture
//! standing in for any of those would make this file worthless.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt as _, StreamExt};
use jarvis_adapters::wyoming::WyomingClient;
use jarvis_application::testing::FakeModel;
use jarvis_domain::artifact::{
    ArtifactContent, ArtifactKind, ArtifactManifest, ArtifactSource, BuildProvenance,
};
use jarvis_domain::display::{DisplayProfile, MonitorId, Surface};
use jarvis_domain::ids::{ArtifactId, RunId};
use jarvis_domain::location::Sensitivity;
use jarvis_infra::events::PgEventLog;
use jarvis_infra::messages::PgMessageStore;
use jarvis_infra::runs::PgRunStore;
use jarvis_infra::sessions::PgSessionStore;
use jarvisd::api::{AppState, RunWiring, Wiring, router_with};
use jarvisd::auth::AuthState;
use jarvisd::devices::ConnectedDevices;
use jarvisd::display::{DisplayApi, NodeTargets};
use jarvisd::pairing::PairingState;
use jarvisd::runs::{PassthroughAssembler, RunApi, RunEngine, SystemClock};
use jarvisd::tls::ServerTls;
use jarvisd::ws::{WsHub, WsState};
use sqlx::PgPool;
use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;

mod golden11_support;
mod voice_fixture;
use golden11_support::{FakeArtifactStore, FakeAuditLog};

const ARTIFACT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";

/// The whole of M7's exit evidence, in the order an owner would live it.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn golden11_a_second_node_pairs_receives_a_surface_speaks_and_is_revoked(pool: PgPool) {
    // ---- a real TLS listener in front of the production router ------------
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
    let dir = tempfile::tempdir().expect("temp dir");
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).expect("write cert");
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).expect("write key");
    let tls = ServerTls::load(&cert_path, &key_path).expect("loads");
    let served_fingerprint = tls.fingerprint.clone();

    let (app, harness) = build_app(&pool, served_fingerprint.clone()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let shutdown = CancellationToken::new();
    let server = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { jarvisd::tls::serve(listener, &tls, app, shutdown).await })
    };

    // A client that trusts exactly this certificate — a node's posture.
    let client = pinned_client(cert.cert.der().to_vec(), addr);
    let base = "https://localhost/api/v1".to_owned();

    // ---- the owner pairs first (bootstrap, over TLS) ----------------------
    let owner: serde_json::Value = client
        .post(format!("{base}/auth/pair"))
        .json(&serde_json::json!({
            "pairingCode": harness.bootstrap_code,
            "deviceName": "owner laptop"
        }))
        .send()
        .await
        .expect("pair request")
        .json()
        .await
        .expect("pair response");
    let owner_token = owner["deviceToken"].as_str().expect("token").to_owned();

    // ---- evidence 1: a second node pairs ----------------------------------
    let window: serde_json::Value = client
        .post(format!("{base}/devices/pairing-window"))
        .bearer_auth(&owner_token)
        .send()
        .await
        .expect("window request")
        .json()
        .await
        .expect("window response");
    let code = window["pairingCode"].as_str().expect("code").to_owned();

    let key = SigningKey::generate(&mut rand_core6::OsRng);
    let challenge: serde_json::Value = client
        .post(format!("{base}/devices/pair"))
        .json(&serde_json::json!({
            "publicKey": BASE64.encode(key.verifying_key().as_bytes()),
            "deviceName": "kitchen screen",
            "requestedClass": "room-node",
            "pairingCode": code,
        }))
        .send()
        .await
        .expect("challenge request")
        .json()
        .await
        .expect("challenge response");
    let nonce = BASE64
        .decode(challenge["challenge"].as_str().expect("challenge"))
        .expect("base64");

    let paired: serde_json::Value = client
        .post(format!("{base}/devices/pair/complete"))
        .json(&serde_json::json!({
            "challengeId": challenge["challengeId"],
            "signature": BASE64.encode(key.sign(&nonce).to_bytes()),
        }))
        .send()
        .await
        .expect("complete request")
        .json()
        .await
        .expect("complete response");

    let node_token = paired["deviceToken"].as_str().expect("token").to_owned();
    let node_id = paired["deviceId"].as_str().expect("id").to_owned();
    assert_eq!(paired["deviceClass"], "room-node");
    assert_eq!(
        paired["scopes"]
            .as_array()
            .expect("scopes")
            .iter()
            .map(|s| s.as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["display-agent", "voice-capture"],
        "a satellite is toolless by construction"
    );
    // The node pins what it was told, and what it was told is what it is
    // talking to — the whole basis of ADR-031's transport trust.
    assert_eq!(
        paired["serverFingerprint"].as_str().expect("fingerprint"),
        served_fingerprint,
        "the pairing response pins the certificate actually being served"
    );

    // ---- the node connects over TLS ---------------------------------------
    let mut socket = connect_node(addr, &node_token, cert.cert.der().to_vec()).await;

    // ---- evidence 2: it receives a surface --------------------------------
    // Placing races the node finishing its connection, and F7.5 answers that
    // race honestly: a node that is not connected *yet* gets the same visible
    // 409 as one that never will be. So wait for it to actually be there —
    // which is what an owner does too, by looking at the screen.
    let mut placed = None;
    for _ in 0..40 {
        let response = client
            .post(format!("{base}/artifacts/{ARTIFACT}/open"))
            .bearer_auth(&owner_token)
            // By device id: the alias table is fixed at construction and the
            // id only exists after pairing. Room-name resolution has its own
            // test in `display_api.rs`; golden 11 proves the surface reaches
            // the node that just paired.
            .json(&serde_json::json!({ "node": node_id, "display": "DP-2" }))
            .send()
            .await
            .expect("open request");
        if response.status() == 200 {
            placed = Some(response);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        placed.is_some(),
        "the kitchen screen never became placeable"
    );

    let directive = next_display(&mut socket, Duration::from_secs(5))
        .await
        .expect("the node is sent the surface");
    assert_eq!(directive["type"], "display.place_surface");
    assert_eq!(directive["payload"]["monitor"], "DP-2");
    assert_eq!(directive["payload"]["targetDeviceId"], node_id);

    // ---- evidence 3: it performs a voice flow -----------------------------
    socket
        .send(WsMessage::Text(
            serde_json::json!({
                "type": "voice.stream.start",
                "streamId": "kitchen-1",
                // Without a session the transcript starts no run, and a node
                // with no run is a node with nothing to be answered *with*.
                "sessionId": "01ARZ3NDEKTSV4RRFFQ69G5FB0",
                "sampleRateHz": 16000,
                "sampleWidthBytes": 2,
                "channels": 1
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("start capture");
    socket
        .send(WsMessage::Binary(vec![0u8; 640].into()))
        .await
        .expect("send audio");

    // End of speech. The turn is what produces an answer, so it has to close.
    socket
        .send(WsMessage::Text(
            serde_json::json!({ "type": "voice.stream.stop", "streamId": "kitchen-1" })
                .to_string()
                .into(),
        ))
        .await
        .expect("stop capture");

    // **The answer comes back to the node that asked (F8.5).**
    //
    // Until F10.1 this test stopped at "no refusal came back", which proved a
    // room node may *speak* and nothing about whether it is ever *answered*.
    // That gap mattered: a satellite never holds `ui`, and a run's text deltas
    // ride the Session channel whose rule is `ui` — so the node that asked the
    // question was, by construction, the one socket that could not hear the
    // reply, and a node cannot speak what it is not sent.
    //
    // Asserted at the socket, on binary frames, because that is the only
    // evidence that survives every intermediate claim being wrong.
    let mut heard_audio = false;
    let mut seen: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), socket.next()).await {
            Ok(Some(Ok(WsMessage::Binary(bytes)))) if !bytes.is_empty() => {
                heard_audio = true;
                break;
            }
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: serde_json::Value = serde_json::from_str(&text).expect("an envelope");
                let kind = value["type"].as_str().unwrap_or("?").to_owned();
                if kind == "voice.error" {
                    seen.push(format!("voice.error({})", value["payload"]));
                } else {
                    seen.push(kind);
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => panic!("socket error while waiting for the answer: {e}"),
            Ok(None) => panic!("the socket closed before the node was answered"),
            Err(_) => {}
        }
    }
    assert!(
        heard_audio,
        "the node that asked must hear the answer on its own socket; it received: {seen:?}"
    );

    // ---- evidence 4: revocation works, mid-flow ---------------------------
    let revoked = client
        .post(format!("{base}/devices/{node_id}/revoke"))
        .bearer_auth(&owner_token)
        .json(&serde_json::json!({ "reason": "golden 11" }))
        .send()
        .await
        .expect("revoke request");
    assert_eq!(revoked.status(), 200);

    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                Some(Ok(WsMessage::Close(frame))) => return Some(frame),
                None => return None,
                _ => continue,
            }
        }
    })
    .await
    .expect("the revoked node's socket closes without waiting for a reconnect");
    if let Some(Some(frame)) = closed {
        assert_eq!(u16::from(frame.code), 1008, "policy-violation close");
    }

    // And its token is dead for HTTP too.
    let after = client
        .get(format!("{base}/devices"))
        .bearer_auth(&node_token)
        .send()
        .await
        .expect("post-revocation request");
    assert_eq!(after.status(), 401);

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}

struct AppHarness {
    bootstrap_code: String,
}

/// The production router, wired the way `main.rs` wires it.
async fn build_app(pool: &PgPool, server_fingerprint: String) -> (axum::Router, AppHarness) {
    sqlx::query(
        "INSERT INTO conversation.sessions (id, title, status, created_at, updated_at) \
         VALUES ('01ARZ3NDEKTSV4RRFFQ69G5FB0', NULL, 'active', now(), now())",
    )
    .execute(pool)
    .await
    .expect("seed session");

    let identity: Arc<dyn jarvis_application::ports::IdentityStore> =
        Arc::new(jarvis_infra::identity::PgIdentityStore::new(pool.clone()));
    let audit = Arc::new(jarvis_infra::audit_sink::PgAuditLog::new(pool.clone()));
    let auth = AuthState::bootstrap(identity.clone())
        .await
        .with_audit(audit.clone());
    let bootstrap_code = auth.current_pairing_code().expect("first-run window");

    let hub = WsHub::new();
    let shutdown = CancellationToken::new();
    let events = Arc::new(PgEventLog::new(pool.clone()));
    let engine = RunEngine::new(
        // A real answer, because evidence 3 now asserts the node *hears* it.
        // Two clauses, deliberately. The segmenter emits a clause when it sees
        // a terminator *followed by more text*; the last fragment waits for the
        // run's terminal event to flush it, and this harness runs no outbox
        // dispatcher, so that event never arrives here. In production it does.
        Arc::new(FakeModel::streaming(["Twenty minutes. Left on the pasta."])),
        Arc::new(PassthroughAssembler),
        Arc::new(PgRunStore::new(pool.clone())),
        Arc::new(PgMessageStore::new(pool.clone())),
        hub.clone(),
        Arc::new(SystemClock),
        shutdown.clone(),
        None,
    );
    let run_api = RunApi::new(
        Arc::new(PgSessionStore::new(pool.clone())),
        Arc::new(PgMessageStore::new(pool.clone())),
        Arc::new(PgRunStore::new(pool.clone())),
        events.clone(),
        engine,
        jarvisd::approvals::JarvisApprovalGate::new(pool.clone()),
        None,
    );

    let connected = ConnectedDevices::new();
    let artifacts = Arc::new(FakeArtifactStore::with(seed_manifest()));
    let display = DisplayApi::new(
        artifacts,
        Arc::new(DisplayProfile::new([(
            Surface::ArtifactCanvas,
            MonitorId::new("DP-1").expect("monitor"),
        )])),
        Arc::new(FakeAuditLog::default()),
        hub.clone(),
    )
    .with_nodes(NodeTargets {
        // No aliases: golden 11 places by device id, because the id only
        // exists after pairing. Alias resolution is `display_api.rs`'s job.
        aliases: Default::default(),
        identity: identity.clone(),
        connected: connected.clone(),
        surfaces: auth.surfaces().clone(),
    });

    // The **real** Wyoming client against in-process fixture services: the
    // framing, cancellation and error paths under test stay production code,
    // and only the speech engines are faked (their latency is measured
    // separately by `perf --voice-real` against the real models).
    let stt_url = voice_fixture::stt_returning("how long on the pasta").await;
    let tts_url = voice_fixture::tts_streaming(3, 1024, Duration::from_millis(0)).await;

    let ws = WsState {
        hub: hub.clone(),
        events,
        shutdown,
        transcriber: Some(Arc::new(WyomingClient::new(
            "stt",
            voice_fixture::addr_of(&stt_url),
        ))),
        synthesizer: Some(Arc::new(WyomingClient::new(
            "tts",
            voice_fixture::addr_of(&tts_url),
        ))),
        runs: Some(run_api.clone()),
        identity: Some(identity),
        revocations: auth.revocations().clone(),
        connected,
        audit: Some(audit),
        surfaces: auth.surfaces().clone(),
    };

    let app = router_with(
        AppState::new().with_auth(auth.clone()),
        Wiring {
            runs: Some(RunWiring { runs: run_api, ws }),
            display: Some(display),
            pairing: PairingState::new(),
            // What the node pins (F7.3/ADR-031), wired as `main.rs` wires it.
            server_fingerprint: Some(server_fingerprint),
            ..Wiring::default()
        },
    );
    (app, AppHarness { bootstrap_code })
}

fn seed_manifest() -> ArtifactManifest {
    ArtifactManifest::initial(
        ARTIFACT.parse::<ArtifactId>().expect("ulid"),
        RUN.parse::<RunId>().expect("ulid"),
        ArtifactContent {
            sha256: jarvis_domain::grants::Sha256::from_bytes([7u8; 32]),
            media_type: "text/markdown".parse().expect("media type"),
            kind: ArtifactKind::MarkdownHtml,
            sources: vec![ArtifactSource::Run(RUN.parse().expect("ulid"))],
            sensitivity: Sensitivity::Normal,
            build: BuildProvenance::none(),
            capabilities: vec![],
        },
    )
}

/// An HTTP client that trusts exactly the daemon's certificate and nothing
/// else — pinning, not `danger_accept_invalid_certs`, because "the node pins
/// the fingerprint" is one of the claims under test.
fn pinned_client(der: Vec<u8>, addr: std::net::SocketAddr) -> reqwest::Client {
    reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_der(&der).expect("certificate"))
        .resolve("localhost", addr)
        .build()
        .expect("client")
}

async fn connect_node(
    addr: std::net::SocketAddr,
    token: &str,
    der: Vec<u8>,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(der))
        .expect("root");
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let url = format!("wss://localhost:{}/ws/v1", addr.port());
    let mut request = url.into_client_request().expect("request");
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (socket, _) = tokio_tungstenite::client_async_tls_with_config(
        request,
        stream,
        None,
        Some(Connector::Rustls(Arc::new(config))),
    )
    .await
    .expect("ws over TLS");
    socket
}

async fn next_display(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    within: Duration,
) -> Option<serde_json::Value> {
    next_matching(socket, within, |v| v["channel"] == "display").await
}

async fn next_matching(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    within: Duration,
    matches: impl Fn(&serde_json::Value) -> bool,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::sleep(within);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return None,
            frame = socket.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
                    if matches(&value) {
                        return Some(value);
                    }
                }
                Some(Ok(WsMessage::Close(_))) | None => return None,
                _ => {}
            }
        }
    }
}
