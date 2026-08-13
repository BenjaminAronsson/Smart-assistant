//! F8.1 evidence: a node pairs against a **real** TLS listener serving a real
//! certificate, pins it, and afterwards refuses anything else (ADR-031 §2/§4).
//!
//! The listener here serves the pairing routes' real shapes rather than
//! borrowing jarvisd's router — the daemon side of this protocol is already
//! covered by `jarvisd/tests/pairing_api.rs` and golden 11. What is under test
//! is the **client**: that it signs the challenge the way the daemon verifies,
//! that it checks the fingerprint against the certificate actually served, and
//! that it stores nothing when that check fails.
//!
//! Nothing is mocked at the transport: rcgen mints a certificate per run,
//! rustls serves it, and the node's own `rustls` config is the only trust
//! decision in play.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use jarvis_agent::pairing;
use jarvis_agent::pinning;
use jarvis_agent::store::{CredentialStore, KeyringStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What the stub daemon should claim its fingerprint is.
enum Reports {
    /// The truth: the certificate it actually serves.
    TheCertificateItServes,
    /// A lie, which is what an attacker in the path produces.
    SomethingElse,
    /// Nothing at all — a daemon that serves TLS but forgot to say what.
    Nothing,
}

struct Served {
    address: std::net::SocketAddr,
    certificate_der: Vec<u8>,
    /// Set once the node's signature has been verified server-side, so a test
    /// can assert possession was actually proven rather than assumed.
    signature_verified: Arc<std::sync::atomic::AtomicBool>,
    /// Every request body the listener received, so a test can assert what did
    /// *not* cross the wire.
    bodies: Arc<std::sync::Mutex<Vec<String>>>,
}

/// A TLS listener that speaks the two pairing routes.
async fn spawn_pairing_daemon(reports: Reports) -> Served {
    let certificate =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
    let certificate_der = certificate.cert.der().to_vec();
    let key_der = certificate.signing_key.serialize_der();

    let config = rustls::ServerConfig::builder_with_provider(pinning::default_provider())
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(
                certificate_der.clone(),
            )],
            rustls::pki_types::PrivateKeyDer::try_from(key_der).expect("private key"),
        )
        .expect("server config");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let reported = match reports {
        Reports::TheCertificateItServes => Some(pinning::fingerprint(&certificate_der)),
        // A well-formed fingerprint that is simply not this certificate's.
        Reports::SomethingElse => Some("ab".repeat(32)),
        Reports::Nothing => None,
    };

    let signature_verified = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));

    let served = Served {
        address,
        certificate_der,
        signature_verified: signature_verified.clone(),
        bodies: bodies.clone(),
    };

    tokio::spawn(async move {
        // One challenge in flight is all a test ever needs.
        let mut pending: Option<(Vec<u8>, String)> = None;
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut stream) = acceptor.accept(socket).await else {
                // A handshake the node refused (the pinning test) — keep serving.
                continue;
            };
            let Some((path, body)) = read_request(&mut stream).await else {
                continue;
            };
            bodies.lock().expect("bodies").push(body.clone());
            let request: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();

            let response = match path.as_str() {
                "/api/v1/devices/pair" => {
                    let public_key = request["publicKey"].as_str().unwrap_or_default().to_owned();
                    let nonce = vec![7_u8; 32];
                    let dto = serde_json::json!({
                        "challengeId": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                        "challenge": BASE64.encode(&nonce),
                        "expiresAt": "2099-01-01T00:00:00Z",
                    });
                    pending = Some((nonce, public_key));
                    dto
                }
                "/api/v1/devices/pair/complete" => {
                    let (nonce, public_key) = pending.take().expect("a challenge was issued");
                    // Verify exactly as jarvisd does — the point of the test is
                    // that the client's signature satisfies the real check.
                    let key_bytes: [u8; 32] = BASE64
                        .decode(&public_key)
                        .expect("public key")
                        .try_into()
                        .expect("32 bytes");
                    let signature_bytes: [u8; 64] = BASE64
                        .decode(request["signature"].as_str().unwrap_or_default())
                        .expect("signature")
                        .try_into()
                        .expect("64 bytes");
                    VerifyingKey::from_bytes(&key_bytes)
                        .expect("valid key")
                        .verify(&nonce, &Signature::from_bytes(&signature_bytes))
                        .expect("the node's signature must verify");
                    signature_verified.store(true, std::sync::atomic::Ordering::SeqCst);

                    let mut dto = serde_json::json!({
                        "deviceId": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                        "deviceToken": "the-node-token",
                        "deviceClass": "room-node",
                        "scopes": ["display-agent", "voice-capture"],
                    });
                    if let Some(fingerprint) = &reported {
                        dto["serverFingerprint"] = serde_json::Value::String(fingerprint.clone());
                    }
                    dto
                }
                _ => serde_json::json!({"title": "not found"}),
            };
            let _ = write_json(&mut stream, &response).await;
        }
    });

    served
}

async fn read_request<S>(stream: &mut S) -> Option<(String, String)>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 1024];
    // Headers first.
    let split = loop {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        raw.extend_from_slice(&buffer[..read]);
        if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
    };
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let path = head.lines().next()?.split_whitespace().nth(1)?.to_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);

    let mut body = raw[split + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..read]);
    }
    Some((path, String::from_utf8_lossy(&body).into_owned()))
}

async fn write_json<S>(stream: &mut S, body: &serde_json::Value) -> std::io::Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let body = serde_json::to_string(body).expect("encodes");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    stream.shutdown().await
}

fn store_in(dir: &tempfile::TempDir) -> KeyringStore {
    KeyringStore::with_file(dir.path().join("credentials.json"))
}

/// `Credentials` has no `Debug` (it holds a token and a private key), so the
/// success arm is unwrapped by hand rather than given a printable impl.
async fn expect_refusal(port: u16) -> anyhow::Error {
    match pairing::pair(
        &format!("https://localhost:{port}"),
        "kitchen",
        "room-node",
        "123-456",
    )
    .await
    {
        Ok(_) => panic!("pairing must be refused"),
        Err(e) => e,
    }
}

/// The ordinary path: pair, prove possession, pin what was served.
#[tokio::test]
async fn a_node_pairs_over_real_tls_and_pins_the_certificate_it_was_served() {
    let daemon = spawn_pairing_daemon(Reports::TheCertificateItServes).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let store = store_in(&dir);

    let credentials = match pairing::pair(
        &format!("https://localhost:{}", daemon.address.port()),
        "kitchen",
        "room-node",
        "123-456",
    )
    .await
    {
        Ok(credentials) => credentials,
        Err(e) => panic!("pairing must succeed: {e}"),
    };
    // Persisting is the caller's job (see `pairing::pair`), so do what the
    // binary does.
    store.save(&credentials).expect("save");

    // The class comes from the server, not from what we asked for.
    assert_eq!(credentials.device_class, "room-node");
    assert!(
        daemon
            .signature_verified
            .load(std::sync::atomic::Ordering::SeqCst),
        "the daemon must have verified the node's signature"
    );

    let stored = store.load().expect("load").expect("credentials stored");
    assert_eq!(stored.device_token, "the-node-token");
    assert_eq!(stored.device_class, "room-node");
    assert_eq!(
        stored.server_fingerprint.as_deref(),
        Some(pinning::fingerprint(&daemon.certificate_der).as_str()),
        "the pinned fingerprint must be the certificate actually served"
    );
    assert!(stored.is_tls());

    // Invariant 5, checked at the wire rather than asserted: the private key
    // never appears in anything the node sent.
    let seed = stored.private_key;
    for body in daemon.bodies.lock().expect("bodies").iter() {
        assert!(
            !body.contains(&seed),
            "the private key must never cross the wire"
        );
    }
}

/// The attack pinning exists to stop. A daemon whose reported fingerprint is
/// not the certificate it served is refused — and, critically, **nothing is
/// stored**, so a failed pairing cannot leave a half-trusted node behind.
#[tokio::test]
async fn a_fingerprint_that_is_not_the_served_certificate_is_refused_and_stores_nothing() {
    let daemon = spawn_pairing_daemon(Reports::SomethingElse).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let store = store_in(&dir);

    let error = expect_refusal(daemon.address.port()).await;
    assert!(
        error.to_string().contains("does not match"),
        "unexpected error: {error}"
    );
    assert!(
        store.load().expect("load").is_none(),
        "a refused pairing must store nothing"
    );
}

#[tokio::test]
async fn tls_with_no_reported_fingerprint_is_refused_rather_than_left_unpinned() {
    let daemon = spawn_pairing_daemon(Reports::Nothing).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let store = store_in(&dir);

    let error = expect_refusal(daemon.address.port()).await;
    assert!(
        error.to_string().contains("unpinnable"),
        "unexpected error: {error}"
    );
    assert!(store.load().expect("load").is_none());
}

/// "Pins the fingerprint and refuses anything else afterwards" — the second
/// half, at the TLS layer. A listener serving a *different* certificate is
/// rejected during the handshake, before a single byte of application data.
#[tokio::test]
async fn after_pairing_a_different_certificate_is_refused_at_the_handshake() {
    // Pair against one daemon...
    let first = spawn_pairing_daemon(Reports::TheCertificateItServes).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let store = store_in(&dir);
    let credentials = match pairing::pair(
        &format!("https://localhost:{}", first.address.port()),
        "kitchen",
        "room-node",
        "123-456",
    )
    .await
    {
        Ok(credentials) => credentials,
        Err(e) => panic!("pairing must succeed: {e}"),
    };
    store.save(&credentials).expect("save");
    let pinned = store
        .load()
        .expect("load")
        .expect("stored")
        .server_fingerprint
        .expect("a fingerprint");

    // ...then meet an impostor with a valid certificate of its own.
    let impostor = spawn_pairing_daemon(Reports::TheCertificateItServes).await;
    assert_ne!(
        pinning::fingerprint(&impostor.certificate_der),
        pinned,
        "the impostor must genuinely be a different certificate"
    );

    let endpoint = jarvis_agent::http::Endpoint::parse(&format!(
        "https://localhost:{}",
        impostor.address.port()
    ))
    .expect("endpoint");
    let result = jarvis_agent::http::post_json(
        &endpoint,
        Some(Arc::new(pinning::pinned_config(&pinned))),
        "/api/v1/devices/pair",
        &serde_json::json!({}),
        None,
    )
    .await;

    // `Response` has no `Debug` on purpose (its body carries a device token),
    // so unwrap the error by hand.
    let error = match result {
        Ok(_) => panic!("the pinned client must refuse the impostor"),
        Err(e) => e,
    };
    assert!(
        error.to_string().contains("TLS handshake"),
        "expected a handshake failure, got: {error}"
    );
}
