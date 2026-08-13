//! The pairing ceremony, node side (ADR-031 §2, docs/05 §1).
//!
//! Three steps: present the public key and the owner's one-time code, sign the
//! challenge that comes back, receive a token and the class the server
//! **assigned**. The node requests a class; it never decides one.
//!
//! The security-critical step is the last one, and it is not on the wire at
//! all: before anything is stored, the fingerprint in the response is checked
//! against the certificate the server actually presented. ADR-031 §4 makes the
//! fingerprint meaningful by delivering it inside the ceremony the pairing code
//! already gated — but "delivered inside the ceremony" only counts if somebody
//! checks that the delivered value describes *this* connection. That is
//! [`verify_served_certificate`], and a mismatch aborts the pairing with
//! nothing written.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use jarvis_contracts::auth::PairResponse;
use jarvis_contracts::pairing::{
    NodePairChallengeDto, NodePairCompleteRequest, NodePairStartRequest,
};

use crate::http::{self, Endpoint};
use crate::identity::NodeKey;
use crate::pinning::{self, CapturingVerifier};
use crate::store::Credentials;

/// The classes a node may ask to be. `owner-ui` is refused by the server, never
/// upgraded (docs/05 §6.3) — it is rejected here too so the owner finds out
/// before they read out a pairing code.
pub const NODE_CLASSES: [&str; 3] = ["display-node", "voice-node", "room-node"];

/// Runs the ceremony and **returns** the credentials it earned.
///
/// It deliberately does not persist them. Two reasons, and the second is the
/// load-bearing one:
///
/// 1. Persisting is blocking D-Bus I/O, and this is an async fn — the caller
///    puts the save on a blocking thread where it belongs.
/// 2. A function that cannot write cannot half-write. "A refused pairing stores
///    nothing" stops being a property to test on every failure path and becomes
///    a property of the shape.
pub async fn pair(
    server_url: &str,
    device_name: &str,
    requested_class: &str,
    pairing_code: &str,
) -> Result<Credentials> {
    if !NODE_CLASSES.contains(&requested_class) {
        bail!(
            "class must be one of {} — a node cannot pair as the owner's own client",
            NODE_CLASSES.join(", ")
        );
    }

    let endpoint = Endpoint::parse(server_url)?;
    // One capturing verifier across both requests, so the certificate we check
    // is the certificate the exchange actually ran over.
    let capture = CapturingVerifier::new();
    let tls_config = endpoint.tls.then(|| {
        Arc::new(
            rustls::ClientConfig::builder_with_provider(pinning::default_provider())
                .with_safe_default_protocol_versions()
                .expect("ring provider supports the default protocol versions")
                .dangerous()
                .with_custom_certificate_verifier(capture.clone())
                .with_no_client_auth(),
        )
    });

    let key = NodeKey::generate();

    // ---- step one: public key + code, in exchange for a challenge ---------
    let start = NodePairStartRequest {
        public_key: key.public_key_base64(),
        device_name: device_name.to_owned(),
        requested_class: requested_class.to_owned(),
        pairing_code: pairing_code.to_owned(),
    };
    let response = http::post_json(
        &endpoint,
        tls_config.clone(),
        "/api/v1/devices/pair",
        &serde_json::to_value(&start).context("encoding the pairing request")?,
        None,
    )
    .await?;
    if !response.is_success() {
        bail!("pairing was refused: {}", response.problem_detail());
    }
    let challenge: NodePairChallengeDto =
        serde_json::from_str(&response.body).context("pairing challenge was not readable")?;

    // ---- step two: prove possession of the key --------------------------
    let nonce = BASE64
        .decode(&challenge.challenge)
        .context("challenge is not valid base64")?;
    let complete = NodePairCompleteRequest {
        challenge_id: challenge.challenge_id,
        signature: key.sign_base64(&nonce),
    };
    let response = http::post_json(
        &endpoint,
        tls_config,
        "/api/v1/devices/pair/complete",
        &serde_json::to_value(&complete).context("encoding the completion request")?,
        None,
    )
    .await?;
    if !response.is_success() {
        bail!("pairing was refused: {}", response.problem_detail());
    }
    let paired: PairResponse =
        serde_json::from_str(&response.body).context("pairing response was not readable")?;

    // ---- step three, and the one that matters: pin ----------------------
    let fingerprint =
        verify_served_certificate(&paired, capture.captured().as_deref(), endpoint.tls)?;

    Ok(Credentials {
        server_url: server_url.trim_end_matches('/').to_owned(),
        private_key: key.to_seed_base64(),
        device_token: paired.device_token,
        device_id: paired.device_id.to_string(),
        device_class: paired.device_class,
        server_fingerprint: fingerprint,
    })
}

/// Checks that the fingerprint the daemon reported is the certificate it
/// actually served, and returns the value to pin.
///
/// The three cases are all real:
///
/// * **TLS, fingerprints agree** — pin it. The ordinary path.
/// * **TLS, fingerprints differ (or none was sent)** — refuse. Something in the
///   path served a different certificate than the daemon believes it serves,
///   which is precisely the attack pinning exists to stop. Nothing is stored.
/// * **Plaintext loopback** — nothing to pin, and saying so is honest. A
///   fingerprint arriving over plaintext is refused rather than pinned: it
///   would be a pin on a value nobody authenticated.
fn verify_served_certificate(
    paired: &PairResponse,
    served_der: Option<&[u8]>,
    tls: bool,
) -> Result<Option<String>> {
    if !tls {
        if paired.server_fingerprint.is_some() {
            bail!(
                "the daemon returned a certificate fingerprint over a plaintext connection; \
                 refusing to pin a value that nothing authenticated"
            );
        }
        return Ok(None);
    }

    let served = served_der.context("TLS handshake completed without a certificate")?;
    let actual = pinning::fingerprint(served);
    let Some(reported) = paired.server_fingerprint.as_deref() else {
        bail!(
            "the daemon served a certificate but reported no fingerprint to pin; \
             refusing to pair over an unpinnable connection"
        );
    };
    if !reported.eq_ignore_ascii_case(&actual) {
        bail!(
            "the certificate served does not match the fingerprint the daemon reported \
             (served {actual}, reported {reported}); refusing to pair"
        );
    }
    Ok(Some(actual))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Built through serde rather than by naming `DeviceId`, so the agent does
    /// not take a dependency on `jarvis-domain` for one test constructor — and
    /// so the fixture is shaped the way the daemon's JSON actually arrives.
    fn response(fingerprint: Option<&str>) -> PairResponse {
        let mut value = serde_json::json!({
            "deviceId": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "deviceToken": "token",
            "deviceClass": "room-node",
            "scopes": ["voice-capture"],
        });
        if let Some(fingerprint) = fingerprint {
            value["serverFingerprint"] = serde_json::Value::String(fingerprint.to_owned());
        }
        serde_json::from_value(value).expect("fixture decodes as a PairResponse")
    }

    #[test]
    fn pins_the_fingerprint_when_it_describes_the_served_certificate() {
        let der = b"a certificate";
        let actual = pinning::fingerprint(der);
        let pinned = verify_served_certificate(&response(Some(&actual)), Some(der), true)
            .expect("verifies")
            .expect("a fingerprint to pin");
        assert_eq!(pinned, actual);
    }

    /// The attack this whole feature exists to stop: something in the path
    /// served its own certificate. Nothing may be stored.
    #[test]
    fn refuses_when_the_reported_fingerprint_is_not_the_served_certificate() {
        let error = verify_served_certificate(
            &response(Some(&"ab".repeat(32))),
            Some(b"a different certificate"),
            true,
        )
        .expect_err("must refuse");
        assert!(error.to_string().contains("does not match"), "{error}");
    }

    #[test]
    fn refuses_tls_with_no_reported_fingerprint_rather_than_trusting_blind() {
        assert!(verify_served_certificate(&response(None), Some(b"cert"), true).is_err());
    }

    #[test]
    fn plaintext_loopback_pins_nothing() {
        assert!(
            verify_served_certificate(&response(None), None, false)
                .expect("verifies")
                .is_none()
        );
    }

    #[test]
    fn refuses_a_fingerprint_delivered_over_plaintext() {
        assert!(verify_served_certificate(&response(Some(&"ab".repeat(32))), None, false).is_err());
    }
}
