//! Infra half of the capability bridge (F6.5): token randomness, token storage,
//! and the argument digest that closes **D-M5-4**.
//!
//! Both live here for the same reason the grant minter does — this is where
//! `getrandom` and `sha2` are allowed (invariant 3).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

use async_trait::async_trait;
use jarvis_application::appbridge::CapabilityTokenStore;
use jarvis_application::ports::ArgumentDigest;
use jarvis_domain::appbridge::{CapabilityToken, CapabilityTokenId};
use jarvis_domain::grants::Sha256 as ArgsHash;
use jarvis_domain::tools::{CanonicalValue, canonical_form};
use sha2::{Digest, Sha256};

/// `sha256(canonical_form(arguments))` — **the same function the grant minter
/// binds with** (`crate::grants`), which is the whole point: an audit row and a
/// grant row describing the same effect must carry the same value, or joining
/// them after the fact is guesswork. A test below pins the two together.
pub struct Sha256ArgumentDigest;

impl ArgumentDigest for Sha256ArgumentDigest {
    fn digest(&self, arguments: &CanonicalValue) -> ArgsHash {
        let mut hasher = Sha256::new();
        hasher.update(canonical_form(arguments));
        ArgsHash::from_bytes(hasher.finalize().into())
    }
}

/// In-memory, single-use capability tokens (F6.5).
///
/// **Deliberately not durable.** A capability token is scoped to one app
/// instance open on one device right now; a token that survived a daemon
/// restart would outlive the frame it was minted for and become exactly the
/// ambient authority the design avoids. Losing them on restart is the correct
/// behaviour, not a limitation.
///
/// Expired tokens are swept on every mint so a long-lived daemon cannot
/// accumulate them — the map is bounded by *live* tokens, not by tokens ever
/// minted.
#[derive(Default)]
pub struct InMemoryCapabilityTokens {
    tokens: Mutex<HashMap<[u8; 32], CapabilityToken>>,
}

impl InMemoryCapabilityTokens {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live tokens — for tests and diagnostics only.
    pub fn len(&self) -> usize {
        self.tokens.lock().expect("token map not poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn sweep(map: &mut HashMap<[u8; 32], CapabilityToken>, now: SystemTime) {
        map.retain(|_, t| now < t.expires_at);
    }
}

#[async_trait]
impl CapabilityTokenStore for InMemoryCapabilityTokens {
    async fn put(&self, token: CapabilityToken) {
        let mut map = self.tokens.lock().expect("token map not poisoned");
        // Sweep against the token's own mint-time clock: the store has no clock
        // of its own, and every mint carries a fresh `expires_at`.
        let now = token.expires_at - jarvis_domain::appbridge::CAPABILITY_TOKEN_TTL;
        Self::sweep(&mut map, now);
        map.insert(*token.id.as_bytes(), token);
    }

    async fn consume(&self, id: &CapabilityTokenId) -> Option<CapabilityToken> {
        // Remove-on-read: single use is the only operation, so replay is not a
        // rule a caller has to remember.
        self.tokens
            .lock()
            .expect("token map not poisoned")
            .remove(id.as_bytes())
    }

    async fn new_id(&self) -> CapabilityTokenId {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("system CSPRNG available");
        CapabilityTokenId::from_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::appbridge::CAPABILITY_TOKEN_TTL;
    use jarvis_domain::artifact::{ArtifactVersion, Capability};
    use std::time::Duration;

    fn token(id: [u8; 32], expires_at: SystemTime) -> CapabilityToken {
        CapabilityToken {
            id: CapabilityTokenId::from_bytes(id),
            artifact_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
            version: ArtifactVersion::new(1).unwrap(),
            capability: Capability::HomeReadState,
            device_id: "01ARZ3NDEKTSV4RRFFQ69G5FB1".parse().unwrap(),
            expires_at,
        }
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// The headline property: a token can be spent exactly once. A replay finds
    /// nothing, and finds nothing *regardless* of whether it would otherwise
    /// have been valid.
    #[tokio::test]
    async fn a_token_is_consumed_by_its_first_use() {
        let store = InMemoryCapabilityTokens::new();
        let t = token([7; 32], at(1000) + CAPABILITY_TOKEN_TTL);
        store.put(t.clone()).await;

        assert_eq!(store.consume(&t.id).await, Some(t.clone()));
        assert_eq!(
            store.consume(&t.id).await,
            None,
            "a replayed token must find nothing"
        );
    }

    #[tokio::test]
    async fn an_unknown_token_is_simply_absent() {
        let store = InMemoryCapabilityTokens::new();
        assert_eq!(
            store.consume(&CapabilityTokenId::from_bytes([9; 32])).await,
            None
        );
    }

    /// A long-lived daemon must not accumulate dead tokens.
    #[tokio::test]
    async fn expired_tokens_are_swept_when_new_ones_are_minted() {
        let store = InMemoryCapabilityTokens::new();
        let old = token([1; 32], at(1000) + CAPABILITY_TOKEN_TTL);
        store.put(old.clone()).await;
        assert_eq!(store.len(), 1);

        // A token minted an hour later: the sweep runs against its mint time.
        let fresh = token([2; 32], at(4600) + CAPABILITY_TOKEN_TTL);
        store.put(fresh.clone()).await;

        assert_eq!(store.len(), 1, "the expired token was swept");
        assert_eq!(store.consume(&old.id).await, None);
        assert!(store.consume(&fresh.id).await.is_some());
    }

    #[tokio::test]
    async fn minted_ids_are_random_and_distinct() {
        let store = InMemoryCapabilityTokens::new();
        let a = store.new_id().await;
        let b = store.new_id().await;
        assert_ne!(a.as_bytes(), b.as_bytes());
        assert_ne!(a.as_bytes(), &[0u8; 32]);
    }

    /// **D-M5-4's correctness condition.** The digest an audit row records and
    /// the hash a grant binds must be the identical function — otherwise the
    /// audit trail and the grant table describe the same effect with two
    /// different values, and the join that makes an effect answerable is not
    /// available.
    #[test]
    fn the_audit_digest_is_the_same_hash_the_grant_minter_binds() {
        let arguments = CanonicalValue::obj([
            ("state", CanonicalValue::str("on")),
            ("entity_id", CanonicalValue::str("light.kitchen")),
        ]);
        // Reordered keys must hash identically — canonical form, not literal
        // form, is what both sides bind.
        let reordered = CanonicalValue::obj([
            ("entity_id", CanonicalValue::str("light.kitchen")),
            ("state", CanonicalValue::str("on")),
        ]);

        let digest = Sha256ArgumentDigest.digest(&arguments);
        assert_eq!(digest, Sha256ArgumentDigest.digest(&reordered));
        assert_eq!(
            digest.to_string(),
            crate::grants::args_hash_for_tests(&arguments).to_string(),
            "the audit digest and the grant binding must be one function"
        );
    }

    #[test]
    fn different_arguments_digest_differently() {
        let a = CanonicalValue::obj([("entity_id", CanonicalValue::str("light.kitchen"))]);
        let b = CanonicalValue::obj([("entity_id", CanonicalValue::str("light.hall"))]);
        assert_ne!(
            Sha256ArgumentDigest.digest(&a),
            Sha256ArgumentDigest.digest(&b)
        );
    }
}
