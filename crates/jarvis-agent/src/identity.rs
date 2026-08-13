//! The node's Ed25519 identity (ADR-031 §1).
//!
//! Generated locally, and the private half never leaves this process except to
//! go into [`crate::store`]. It is not in the pairing request, not in a log,
//! not in an argument, and there is no accessor that returns it — the only way
//! out is [`NodeKey::to_seed_base64`], which exists so the store can persist it.

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey};

/// A node's keypair.
pub struct NodeKey {
    signing: SigningKey,
}

impl NodeKey {
    /// A fresh keypair from the OS RNG.
    pub fn generate() -> Self {
        Self {
            signing: SigningKey::generate(&mut rand_core6::OsRng),
        }
    }

    /// Restores a keypair from a stored base64 seed.
    pub fn from_seed_base64(seed: &str) -> Result<Self> {
        let bytes = BASE64
            .decode(seed)
            .context("stored private key is not valid base64")?;
        let seed: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored private key is not 32 bytes"))?;
        Ok(Self {
            signing: SigningKey::from_bytes(&seed),
        })
    }

    /// Base64 of the private seed — for [`crate::store`] and nothing else.
    pub fn to_seed_base64(&self) -> String {
        BASE64.encode(self.signing.to_bytes())
    }

    /// Base64 of the **public** key, in the encoding `NodePairStartRequest`
    /// specifies.
    pub fn public_key_base64(&self) -> String {
        BASE64.encode(self.signing.verifying_key().as_bytes())
    }

    /// Signs raw challenge bytes; returns base64 of the 64-byte signature.
    pub fn sign_base64(&self, message: &[u8]) -> String {
        BASE64.encode(self.signing.sign(message).to_bytes())
    }

    /// The short public-key fingerprint, computed exactly as jarvisd computes
    /// the `keyFingerprint` it writes into the `device.paired` audit event:
    /// `sha256(base64 public key)`, first 16 hex characters.
    ///
    /// It exists so the two ends can be compared by a human. The daemon's
    /// comment for its own copy says a fingerprint is "what an operator can
    /// actually compare against the node's own display" — this is that display.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest as _, Sha256};
        hex::encode(Sha256::digest(self.public_key_base64().as_bytes()))[..16].to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    #[test]
    fn a_signature_verifies_against_the_public_key_the_server_would_store() {
        let key = NodeKey::generate();
        let challenge = b"a 32-byte nonce would go here...";

        // Verify exactly the way jarvisd's `complete` does: decode the base64
        // public key and signature, then `verify_strict`.
        let public: [u8; 32] = BASE64
            .decode(key.public_key_base64())
            .expect("public key decodes")
            .try_into()
            .expect("32 bytes");
        let signature: [u8; 64] = BASE64
            .decode(key.sign_base64(challenge))
            .expect("signature decodes")
            .try_into()
            .expect("64 bytes");

        let verifying = VerifyingKey::from_bytes(&public).expect("valid key");
        verifying
            .verify(challenge, &Signature::from_bytes(&signature))
            .expect("signature verifies");
    }

    #[test]
    fn a_key_survives_a_round_trip_through_its_stored_seed() {
        let original = NodeKey::generate();
        let restored = NodeKey::from_seed_base64(&original.to_seed_base64()).expect("restores");
        assert_eq!(original.public_key_base64(), restored.public_key_base64());
        // Same key, same signature over the same message.
        assert_eq!(restored.sign_base64(b"x"), original.sign_base64(b"x"));
    }

    #[test]
    fn a_malformed_seed_is_rejected() {
        assert!(NodeKey::from_seed_base64("not base64!!").is_err());
        assert!(NodeKey::from_seed_base64(&BASE64.encode([0_u8; 16])).is_err());
    }
}
