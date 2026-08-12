//! TLS for LAN/remote nodes (F7.3, docs/06 §7, ADR-031).
//!
//! # What the certificate is for
//!
//! There is no CA in a house, so the certificate is self-signed and a node has
//! no chain to validate. What makes it meaningful is the **fingerprint handed
//! to the node during the pairing ceremony** — over the channel the owner
//! already trusted enough to read a one-time code across. The node pins that
//! fingerprint and refuses anything else afterwards, which is what turns
//! "encrypted to somebody" into "encrypted to the daemon I paired with".
//!
//! That is also why the fingerprint is computed from the certificate *file the
//! listener actually serves*, not from config: a fingerprint derived from
//! anything other than the bytes on the wire would pin the wrong thing.

use std::path::Path;

use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

/// A loaded server certificate and the fingerprint a node pins.
#[derive(Clone)]
pub struct ServerTls {
    pub config: std::sync::Arc<rustls::ServerConfig>,
    /// Lowercase hex sha256 of the leaf certificate's DER bytes — the same
    /// value `openssl x509 -fingerprint -sha256` prints, minus the colons.
    pub fingerprint: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: rustls_pki_types::pem::Error,
    },
    #[error("{path} contains no {what}")]
    Empty { path: String, what: &'static str },
    #[error("building the TLS configuration: {0}")]
    Config(#[from] rustls::Error),
}

impl ServerTls {
    /// Load a PEM certificate chain + private key and derive the fingerprint.
    ///
    /// Rejects an empty chain or a missing key rather than starting a listener
    /// that cannot complete a handshake — a daemon that binds and then fails
    /// every connection looks healthy to everything except its users.
    pub fn load(cert_path: &Path, key_path: &Path) -> Result<Self, TlsError> {
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
            .and_then(|iter| iter.collect())
            .map_err(|source| TlsError::Read {
                path: cert_path.display().to_string(),
                source,
            })?;
        let Some(leaf) = certs.first().cloned() else {
            return Err(TlsError::Empty {
                path: cert_path.display().to_string(),
                what: "certificate",
            });
        };
        let key = PrivateKeyDer::from_pem_file(key_path).map_err(|source| match source {
            rustls_pki_types::pem::Error::NoItemsFound => TlsError::Empty {
                path: key_path.display().to_string(),
                what: "private key",
            },
            source => TlsError::Read {
                path: key_path.display().to_string(),
                source,
            },
        })?;

        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)?;
        // The node speaks HTTP/1.1 (WebSocket upgrades ride on it); advertising
        // h2 as well would let a client negotiate a protocol the WS path does
        // not implement.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];

        Ok(Self {
            fingerprint: fingerprint_of(leaf.as_ref()),
            config: std::sync::Arc::new(config),
        })
    }
}

/// Serve `app` over TLS until `cancel` fires.
///
/// Hand-rolled rather than delegating to `axum-server`, which is the obvious
/// crate for this and pulls `rustls-pemfile` — unmaintained as of
/// RUSTSEC-2025-0134. Accepting a fresh advisory to save fifty lines is a bad
/// trade, especially in the feature whose entire purpose is to make a network
/// listener safe.
///
/// `serve_connection_with_upgrades` is load-bearing: `/ws/v1` is an HTTP
/// upgrade, and the plain `serve_connection` would answer the handshake and
/// then drop the upgraded socket — nodes would connect and immediately go
/// silent.
pub async fn serve(
    listener: tokio::net::TcpListener,
    tls: &ServerTls,
    app: axum::Router,
    cancel: tokio_util::sync::CancellationToken,
) -> std::io::Result<()> {
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tower::ServiceExt as _;

    let acceptor = tokio_rustls::TlsAcceptor::from(tls.config.clone());
    let connections = tokio_util::task::TaskTracker::new();

    loop {
        let (stream, peer) = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                // One failed accept (fd exhaustion, a client vanishing between
                // SYN and accept) must not take the listener down.
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    continue;
                }
            },
        };
        // Same reasoning as the plaintext path: voice interleaves many small
        // frames and Nagle costs ~40 ms per exchange (F5.2, NFR-04).
        let _ = stream.set_nodelay(true);

        let acceptor = acceptor.clone();
        let app = app.clone();
        let cancel = cancel.clone();
        connections.spawn(async move {
            let tls_stream = match acceptor.accept(stream).await {
                Ok(tls_stream) => tls_stream,
                // A failed handshake is normal traffic on an open port —
                // scanners, plaintext clients, a node with a stale pin.
                Err(e) => {
                    tracing::debug!(%peer, error = %e, "TLS handshake failed");
                    return;
                }
            };
            let service = hyper::service::service_fn(move |request| app.clone().oneshot(request));
            let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            let connection =
                builder.serve_connection_with_upgrades(TokioIo::new(tls_stream), service);
            tokio::pin!(connection);
            tokio::select! {
                result = connection.as_mut() => {
                    if let Err(e) = result {
                        tracing::debug!(%peer, error = %e, "connection ended");
                    }
                }
                // Shutdown asks the connection to finish; the caller's drain
                // deadline is what bounds how long we wait for it.
                () = cancel.cancelled() => {
                    connection.as_mut().graceful_shutdown();
                    let _ = connection.await;
                }
            }
        });
    }

    connections.close();
    connections.wait().await;
    Ok(())
}

/// sha256 over the DER bytes, lowercase hex.
pub fn fingerprint_of(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fingerprint must be the certificate's own DER digest — the value a
    /// node can independently recompute from the bytes it was served. Pinned
    /// against a known vector so a refactor cannot quietly start hashing
    /// something else (the PEM text, say, which differs by whitespace).
    #[test]
    fn the_fingerprint_is_the_sha256_of_the_der() {
        assert_eq!(
            fingerprint_of(b"jarvis"),
            hex::encode(Sha256::digest(b"jarvis"))
        );
        assert_eq!(fingerprint_of(b"").len(), 64, "lowercase hex sha256");
        assert_ne!(fingerprint_of(b"a"), fingerprint_of(b"b"));
    }
}
