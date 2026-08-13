//! F8.1 evidence: the session loop reconnects when it should and **stops when
//! it must** (docs/05 §6.4, ADR-031 consequences).
//!
//! Revocation is the interesting half. A satellite that treats "you are
//! revoked" as a transient error becomes a device the owner cannot switch off —
//! it sits in the kitchen retrying forever against a daemon that keeps saying
//! no. So the two terminal signals jarvisd can send, a 1008 close and a refused
//! handshake, are asserted to end the loop, and the ordinary disconnect is
//! asserted *not* to.
//!
//! Plaintext `ws://` throughout: the pinned-TLS path is covered by
//! `pairing_tls.rs`, and what is under test here is the loop's decisions.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use jarvis_agent::client::{self, SessionOutcome};
use jarvis_agent::compositor::NoCompositor;
use jarvis_agent::store::Credentials;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// How the stub daemon greets each connection.
#[derive(Clone, Copy)]
enum Greeting {
    /// Close with 1008 — jarvisd's `REVOKED_CLOSE_CODE`.
    CloseRevoked,
    /// Close normally, as a restarting daemon does.
    CloseNormally,
    /// Refuse the upgrade with 403, as the bearer middleware does for a token
    /// that is no longer authority.
    Refuse403,
}

/// Returns the listener address and a counter of accepted connections.
async fn spawn_daemon(greeting: Greeting) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let connections = Arc::new(AtomicUsize::new(0));

    let counter = connections.clone();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                match greeting {
                    Greeting::Refuse403 => {
                        // Reject at the HTTP layer, before any upgrade.
                        use tokio::io::AsyncWriteExt as _;
                        let mut socket = socket;
                        let _ = socket
                            .write_all(
                                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\
                                  Connection: close\r\n\r\n",
                            )
                            .await;
                        let _ = socket.shutdown().await;
                    }
                    Greeting::CloseRevoked | Greeting::CloseNormally => {
                        let Ok(mut stream) = tokio_tungstenite::accept_async(socket).await else {
                            return;
                        };
                        use futures_util::SinkExt as _;
                        let frame = match greeting {
                            Greeting::CloseRevoked => CloseFrame {
                                code: CloseCode::Policy, // 1008
                                reason: "device revoked".into(),
                            },
                            _ => CloseFrame {
                                code: CloseCode::Normal,
                                reason: "restarting".into(),
                            },
                        };
                        let _ = stream.send(Message::Close(Some(frame))).await;
                        let _ = stream.flush().await;
                    }
                }
            });
        }
    });

    (address, connections)
}

fn credentials_for(address: std::net::SocketAddr) -> Credentials {
    Credentials {
        server_url: format!("http://{address}"),
        // Base64 of 32 zero bytes: a structurally valid seed that is
        // obviously not a key. Deliberately zero-entropy so the CI secret scan
        // reads it as the placeholder it is (this loop never parses it — only
        // `main` does, at startup).
        private_key: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
        device_token: "a-token".into(),
        device_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        device_class: "voice-node".into(),
        server_fingerprint: None,
    }
}

/// The claim in the feature list: "exits clean on revocation".
#[tokio::test]
async fn a_revoked_node_stops_instead_of_reconnecting() {
    let (address, connections) = spawn_daemon(Greeting::CloseRevoked).await;
    let (_tx, rx) = tokio::sync::watch::channel(false);

    let revoked = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client::run(&credentials_for(address), &NoCompositor, rx),
    )
    .await
    .expect("the loop must terminate on revocation, not spin")
    .expect("run");

    assert!(revoked, "revocation must be reported to the caller");
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "a revoked node must not reconnect even once"
    );
}

/// The same decision at the handshake: a token that is no longer authority is
/// refused with 403, and asking again cannot change that.
#[tokio::test]
async fn a_handshake_refused_with_403_is_terminal() {
    let (address, connections) = spawn_daemon(Greeting::Refuse403).await;
    let (_tx, rx) = tokio::sync::watch::channel(false);

    let revoked = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client::run(&credentials_for(address), &NoCompositor, rx),
    )
    .await
    .expect("the loop must terminate")
    .expect("run");

    assert!(revoked);
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

/// The other side of the same coin: an ordinary close is *not* terminal, or a
/// daemon restart would need a human to walk to the kitchen.
#[tokio::test]
async fn an_ordinary_close_reconnects() {
    let (address, connections) = spawn_daemon(Greeting::CloseNormally).await;
    let (tx, rx) = tokio::sync::watch::channel(false);

    // Stop the loop once it has demonstrably come back for a second attempt.
    let watcher = connections.clone();
    tokio::spawn(async move {
        while watcher.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let _ = tx.send(true);
    });

    let revoked = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        client::run(&credentials_for(address), &NoCompositor, rx),
    )
    .await
    .expect("the loop must reconnect and then observe shutdown")
    .expect("run");

    assert!(!revoked, "an ordinary close is not a revocation");
    assert!(
        connections.load(Ordering::SeqCst) >= 2,
        "the node must reconnect after an ordinary close"
    );
}

/// A node that paired over TLS refuses to run unpinned, rather than silently
/// downgrading to "encrypted to somebody".
#[tokio::test]
async fn a_tls_node_with_no_stored_fingerprint_refuses_to_connect() {
    let mut credentials = credentials_for("127.0.0.1:1".parse().expect("addr"));
    credentials.server_url = "https://jarvis.lan:8741".into();
    credentials.server_fingerprint = None;

    let (_tx, mut rx) = tokio::sync::watch::channel(false);
    let error = client::connect_once(&credentials, &NoCompositor, &mut rx)
        .await
        .expect_err("must refuse to connect unpinned");
    assert!(error.to_string().contains("re-pair"), "{error}");
}

#[tokio::test]
async fn shutdown_ends_the_loop_without_claiming_revocation() {
    let (address, _connections) = spawn_daemon(Greeting::CloseNormally).await;
    let (tx, rx) = tokio::sync::watch::channel(false);
    tx.send(true).expect("signal shutdown");

    let revoked = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client::run(&credentials_for(address), &NoCompositor, rx),
    )
    .await
    .expect("must return promptly")
    .expect("run");
    assert!(!revoked);
}

/// The outcome type is what the binary's exit code is derived from, so the
/// three cases must stay distinguishable.
#[test]
fn session_outcomes_are_distinct() {
    assert_ne!(SessionOutcome::Revoked, SessionOutcome::Disconnected);
    assert_ne!(SessionOutcome::Shutdown, SessionOutcome::Disconnected);
}
