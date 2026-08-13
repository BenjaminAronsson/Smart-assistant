//! Certificate pinning, client side (ADR-031 §4, F7.3).
//!
//! The daemon serves a **self-signed** certificate. There is no chain, no CA,
//! and therefore nothing a trust store could tell us — so this module replaces
//! chain validation entirely rather than adding to it. Trust is exactly one
//! comparison: `sha256(leaf DER)` against the fingerprint the node recorded
//! during pairing.
//!
//! Two verifiers, for the two moments:
//!
//! * [`CapturingVerifier`] — used **only** during pairing, before a fingerprint
//!   exists. It accepts the certificate and records its DER so the caller can
//!   check it against the `serverFingerprint` the pairing response returns. It
//!   is not a trust decision; it defers one, and [`crate::pairing`] refuses to
//!   store anything if the check fails.
//! * [`PinnedVerifier`] — used for every connection afterwards. A certificate
//!   that is not byte-for-byte the pinned one is refused, which is the whole
//!   point: it turns "encrypted to somebody" into "encrypted to the daemon I
//!   paired with".
//!
//! Hostname is deliberately *not* verified. The certificate is pinned by its
//! bytes, so the name inside it adds nothing — and a node reaches its daemon by
//! whatever address the LAN gave it, which is frequently not the name the
//! self-signed certificate was minted for.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};

/// `sha256` of a certificate's DER bytes, lowercase hex — the same shape
/// `jarvisd::tls` computes and puts in `PairResponse.serverFingerprint`.
pub fn fingerprint(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

/// Constant-time-ish comparison of two hex fingerprints, case-insensitive.
///
/// Fingerprints are public values, so this is not a side-channel defence; it is
/// here so that a fingerprint stored in a different case than it arrived still
/// matches, rather than silently locking a node out of its own daemon.
fn fingerprints_match(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.eq_ignore_ascii_case(b)
}

/// Refuses any certificate whose DER does not hash to the pinned fingerprint.
#[derive(Debug)]
pub struct PinnedVerifier {
    expected: String,
    provider: Arc<CryptoProvider>,
}

impl PinnedVerifier {
    pub fn new(expected: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            expected: expected.into(),
            provider: default_provider(),
        })
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let presented = fingerprint(end_entity);
        if fingerprints_match(&presented, &self.expected) {
            Ok(ServerCertVerified::assertion())
        } else {
            // The message names neither fingerprint: this is the one error an
            // attacker would love to read a diff out of, and the node's own
            // logs say plenty (see `client::connect`).
            Err(TlsError::General(
                "server certificate does not match the fingerprint pinned at pairing".to_owned(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Accepts the served certificate and records it, so the pairing flow can hold
/// it against the fingerprint the response claims.
///
/// **Only** [`crate::pairing`] may use this, and only for the pairing request
/// itself. It is safe there for the reason ADR-031 §4 gives: the exchange is
/// gated on a one-time code the owner just read out, so an attacker in the path
/// has to have the code as well as the position — and if they do, the
/// fingerprint check below still catches a *substituted* certificate, because
/// the daemon's own response names the one it serves.
#[derive(Debug)]
pub struct CapturingVerifier {
    seen: Mutex<Option<Vec<u8>>>,
    provider: Arc<CryptoProvider>,
}

impl CapturingVerifier {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(None),
            provider: default_provider(),
        })
    }

    /// The DER of the certificate the server actually presented, if a handshake
    /// completed.
    pub fn captured(&self) -> Option<Vec<u8>> {
        self.seen.lock().expect("capture mutex poisoned").clone()
    }
}

impl ServerCertVerifier for CapturingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        *self.seen.lock().expect("capture mutex poisoned") = Some(end_entity.to_vec());
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The process-wide ring provider, installed on first use.
///
/// `install_default` races harmlessly: whoever loses gets `Err` and the
/// already-installed provider, which is the same one.
pub fn default_provider() -> Arc<CryptoProvider> {
    if let Some(installed) = CryptoProvider::get_default() {
        return installed.clone();
    }
    let provider = rustls::crypto::ring::default_provider();
    let _ = provider.install_default();
    CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()))
}

/// A client config that trusts exactly the pinned fingerprint.
pub fn pinned_config(expected: &str) -> rustls::ClientConfig {
    rustls::ClientConfig::builder_with_provider(default_provider())
        .with_safe_default_protocol_versions()
        .expect("ring provider supports the default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(PinnedVerifier::new(expected))
        .with_no_client_auth()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known-answer check against the daemon's own definition: sha256 of the
    /// DER bytes, lowercase hex. If this drifts, every node locks itself out.
    #[test]
    fn fingerprint_is_lowercase_hex_sha256_of_the_der() {
        // sha256("") — the empty input is the one vector everyone can check.
        assert_eq!(
            fingerprint(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn fingerprints_compare_case_insensitively_but_not_by_prefix() {
        let full = "ab".repeat(32);
        assert!(fingerprints_match(&full, &full.to_uppercase()));
        // A prefix must never satisfy the pin.
        assert!(!fingerprints_match(&full, &full[..60]));
        assert!(!fingerprints_match(&full[..60], &full));
    }
}
