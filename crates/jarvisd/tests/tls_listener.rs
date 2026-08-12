//! F7.3 (docs/06 §7): TLS is the only legal way to serve anything but
//! loopback, and the fingerprint a node pins is the certificate's own digest.
//!
//! The handshake test runs against a **real** rustls listener with a **real**
//! self-signed certificate, because the property under test is exactly the one
//! a fixture would fake away: that a plaintext client cannot talk to it, and
//! that the fingerprint the pairing response promises equals the one the
//! client can compute from the certificate it was actually served.

use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use jarvisd::tls::{ServerTls, fingerprint_of};
use rustls::pki_types::{CertificateDer, ServerName};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// A throwaway self-signed certificate + key, PEM, written to a temp dir.
fn self_signed() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    Vec<u8>,
) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
        .expect("generates a certificate");
    let dir = tempfile::tempdir().expect("temp dir");
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).expect("write cert");
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).expect("write key");
    let der = cert.cert.der().to_vec();
    (dir, cert_path, key_path, der)
}

#[test]
fn the_advertised_fingerprint_is_the_certificate_that_is_served() {
    let (_dir, cert_path, key_path, der) = self_signed();
    let loaded = ServerTls::load(&cert_path, &key_path).expect("loads");
    assert_eq!(
        loaded.fingerprint,
        hex::encode(Sha256::digest(&der)),
        "the pinned value must be the DER digest a node can recompute"
    );
    // Stable across loads — a node that pinned it yesterday must still match.
    let again = ServerTls::load(&cert_path, &key_path).expect("loads");
    assert_eq!(loaded.fingerprint, again.fingerprint);
}

#[test]
fn a_certificate_without_a_key_is_refused_rather_than_half_loaded() {
    let (dir, cert_path, key_path, _) = self_signed();
    let empty = dir.path().join("empty.pem");
    std::fs::write(&empty, b"").expect("write");

    assert!(
        ServerTls::load(&cert_path, &empty).is_err(),
        "a listener that cannot complete a handshake must not start"
    );
    assert!(ServerTls::load(&empty, &key_path).is_err());
    assert!(ServerTls::load(&dir.path().join("nope.pem"), &key_path).is_err());
}

/// The end-to-end property: a real client completes a TLS handshake against
/// the real serving stack and sees exactly the certificate whose fingerprint
/// the daemon advertises — while a plaintext client gets nowhere.
#[tokio::test]
async fn a_tls_listener_serves_the_pinned_certificate_and_refuses_plaintext() {
    let (_dir, cert_path, key_path, der) = self_signed();
    let loaded = ServerTls::load(&cert_path, &key_path).expect("loads");
    let advertised = loaded.fingerprint.clone();

    let app = Router::new().route("/api/v1/diagnostics/health", get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    // The PRODUCTION accept loop — `main` calls this exact function, so a
    // handshake that works here works there (and a WebSocket upgrade that
    // breaks there breaks here).
    let cancel = tokio_util::sync::CancellationToken::new();
    let server = {
        let cancel = cancel.clone();
        let loaded = loaded.clone();
        tokio::spawn(async move { jarvisd::tls::serve(listener, &loaded, app, cancel).await })
    };

    // A client that pins the fingerprint — the node's posture after pairing.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let verifier = PinnedFingerprint { seen: seen.clone() };
    let client_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let mut tls = connector
        .connect(ServerName::try_from("localhost").expect("name"), stream)
        .await
        .expect("handshake completes");

    let presented = seen.lock().expect("not poisoned").clone();
    assert_eq!(
        fingerprint_of(&presented),
        advertised,
        "the certificate on the wire is the one the pairing response promises"
    );
    assert_eq!(
        fingerprint_of(&presented),
        hex::encode(Sha256::digest(&der))
    );

    tls.write_all(
        b"GET /api/v1/diagnostics/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await
    .expect("request");
    let mut response = Vec::new();
    tls.read_to_end(&mut response).await.expect("response");
    assert!(
        String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"),
        "the TLS listener actually serves the app"
    );

    // A plaintext client speaking HTTP at a TLS port gets no HTTP response:
    // its bytes are not a ClientHello, so the handshake fails and the
    // connection closes.
    let mut plain = tokio::net::TcpStream::connect(addr).await.expect("connect");
    plain
        .write_all(b"GET /api/v1/diagnostics/health HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("write");
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        plain.read_to_end(&mut buf),
    )
    .await;
    assert!(
        !String::from_utf8_lossy(&buf).contains("200"),
        "plaintext must never be served on the TLS port: {:?}",
        String::from_utf8_lossy(&buf)
    );

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
}

/// A verifier that accepts anything and records what it was shown — this is a
/// test observing the wire, not a client trusting the network.
#[derive(Debug)]
struct PinnedFingerprint {
    seen: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedFingerprint {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        *self.seen.lock().expect("not poisoned") = end_entity.to_vec();
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
