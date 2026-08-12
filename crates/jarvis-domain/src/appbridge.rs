//! The **capability bridge**'s value types (F6.5, FR-18, docs/06 §6,
//! invariant 1).
//!
//! docs/06 §6: *"optional interaction only via a `postMessage` bridge exchanging
//! short-lived capability tokens for operations named in the artifact manifest;
//! undeclared capability ⇒ reject."*
//!
//! The sharpest test invariant 1 has faced. A message from a generated app is
//! untrusted input exactly like a fetched web page: it may **name** an operation
//! the app's own manifest declares, and it may never *perform* one. A capability
//! token is therefore an **authorization to ask** — never an authorization to
//! execute. Everything downstream of `ask` is the ordinary path:
//! `policy::evaluate`, approval where the tier demands it, a real
//! `ExecutionGrant` for R2+, execution, audit.
//!
//! What lives here is pure: the token's binding, its expiry, and the total
//! function that says whether a presented token authorizes *this* question.
//! Randomness lives in infra, as it does for [`crate::grants::GrantId`].

use std::fmt;
use std::time::{Duration, SystemTime};

use thiserror::Error;

use crate::artifact::{ArtifactVersion, Capability};
use crate::grants::GrantId;
use crate::ids::{ArtifactId, DeviceId};

/// How long a minted capability token remains usable.
///
/// Short because it has to be: a token's whole job is to bind one question, from
/// one app instance, on one device, to the moment it was asked. Long-lived
/// tokens degrade into ambient authority, which is what this design exists to
/// avoid. Sixty seconds is comfortably more than a `postMessage` round trip and
/// comfortably less than "leave a tab open".
pub const CAPABILITY_TOKEN_TTL: Duration = Duration::from_secs(60);

/// A cryptographically random, **single-use** capability token
/// (docs/06 §6). Reuses [`GrantId`]'s 32-byte hex representation and randomness
/// discipline but is a *distinct* type: a capability token authorizes a
/// question, an execution grant authorizes an effect, and the two must never be
/// substitutable.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CapabilityTokenId(GrantId);

impl CapabilityTokenId {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(GrantId::from_bytes(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
}

impl fmt::Display for CapabilityTokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

// Manual, and deliberately *not* the value: a capability token is a secret, and
// a `{:?}` in a span or an error must not spill it (invariant 5). The same rule
// grant secrets follow.
impl fmt::Debug for CapabilityTokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CapabilityTokenId(<redacted>)")
    }
}

impl std::str::FromStr for CapabilityTokenId {
    type Err = crate::grants::HexParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<GrantId>().map(Self)
    }
}

/// A minted capability token and everything it is bound to (docs/06 §6).
///
/// Every field is a binding, and the check below tests all of them. The
/// combination is what makes a stolen or borrowed token useless: it names one
/// app, at one version, for one operation, on one device, until one deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityToken {
    pub id: CapabilityTokenId,
    /// The app this token was minted for.
    pub artifact_id: ArtifactId,
    /// …at this exact version. A v2 of the same app declares its own
    /// capabilities and gets its own tokens; a v1 token must not carry over.
    pub version: ArtifactVersion,
    /// The single operation this token may be exchanged for.
    pub capability: Capability,
    /// The authenticated device that opened the app. Stands in for "session"
    /// (M6-features.md): the device is what jarvisd authenticates, and binding
    /// to it is what stops a token from being replayed from anywhere else.
    pub device_id: DeviceId,
    pub expires_at: SystemTime,
}

/// Why a presented token does not authorize the question being asked. Every
/// variant is audited (docs/06 §6, golden 8) — a rejection nobody can observe is
/// indistinguishable from an absent check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CapabilityTokenError {
    /// No such token: never minted, already spent, or forged. Deliberately one
    /// variant — telling a caller *which* would let it probe the token space.
    #[error("capability token is unknown, expired or already used")]
    Unusable,
    #[error("capability token has expired")]
    Expired,
    #[error("capability token was minted for a different app")]
    WrongArtifact,
    #[error("capability token was minted for a different version of this app")]
    WrongVersion,
    #[error("capability token was minted for a different capability")]
    WrongCapability,
    #[error("capability token was minted for a different device")]
    WrongDevice,
}

impl CapabilityTokenError {
    /// Stable machine code for the problem body and the audit payload
    /// (docs/05 §7).
    pub fn code(self) -> &'static str {
        match self {
            Self::Unusable => "app.token_unusable",
            Self::Expired => "app.token_expired",
            Self::WrongArtifact => "app.token_wrong_artifact",
            Self::WrongVersion => "app.token_wrong_version",
            Self::WrongCapability => "app.token_wrong_capability",
            Self::WrongDevice => "app.token_wrong_device",
        }
    }
}

impl CapabilityToken {
    /// Does this token authorize *this* question? Total, pure, and checked in
    /// full — no short-circuit that leaves a binding untested.
    ///
    /// Order is deliberate: expiry first, because an expired token is the one
    /// case that is nobody's fault and the cheapest to answer; then the
    /// bindings, most-specific last. A caller must treat every `Err` the same
    /// way — reject and audit — and the distinct variants exist for the audit
    /// trail, not for the caller's control flow.
    pub fn authorizes(
        &self,
        artifact_id: &ArtifactId,
        version: ArtifactVersion,
        capability: Capability,
        device_id: &DeviceId,
        now: SystemTime,
    ) -> Result<(), CapabilityTokenError> {
        if now >= self.expires_at {
            return Err(CapabilityTokenError::Expired);
        }
        if &self.artifact_id != artifact_id {
            return Err(CapabilityTokenError::WrongArtifact);
        }
        if self.version != version {
            return Err(CapabilityTokenError::WrongVersion);
        }
        if self.capability != capability {
            return Err(CapabilityTokenError::WrongCapability);
        }
        if &self.device_id != device_id {
            return Err(CapabilityTokenError::WrongDevice);
        }
        Ok(())
    }
}

/// Why a bridge request was refused before it ever reached `policy::evaluate`.
///
/// These are the bridge's *own* refusals. A request that passes them is not
/// authorized — it has merely earned the right to be evaluated by the policy
/// engine like any other proposal (invariant 1).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BridgeDenial {
    #[error("capability token rejected: {0}")]
    Token(#[from] CapabilityTokenError),

    /// **The headline rejection** (docs/06 §6, golden 8): the app asked for a
    /// capability its own manifest does not declare. Decidable only because the
    /// vocabulary is closed (ADR-029) — against free-form strings this check
    /// would be unenforceable.
    #[error("app does not declare capability {0}")]
    UndeclaredCapability(Capability),

    #[error("no such app version")]
    UnknownApp,

    #[error("artifact {0} is not a generated app")]
    NotAnApp(ArtifactId),

    /// The named target failed the domain's own binding-target validation, so
    /// it never reached a tool. (A *valid* target still confers nothing — the
    /// backing tool re-resolves it through its own allowlist.)
    #[error("invalid binding target")]
    InvalidTarget,
}

impl BridgeDenial {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Token(e) => e.code(),
            Self::UndeclaredCapability(_) => "app.undeclared_capability",
            Self::UnknownApp => "app.unknown",
            Self::NotAnApp(_) => "app.not_an_app",
            Self::InvalidTarget => "app.invalid_target",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(hex: &str) -> ArtifactId {
        hex.parse().unwrap()
    }
    fn device() -> DeviceId {
        "01ARZ3NDEKTSV4RRFFQ69G5FB1".parse().unwrap()
    }
    const NOW: Duration = Duration::from_secs(1_800_000_000);

    fn token() -> CapabilityToken {
        CapabilityToken {
            id: CapabilityTokenId::from_bytes([1; 32]),
            artifact_id: artifact("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            version: ArtifactVersion::new(1).unwrap(),
            capability: Capability::HomeReadState,
            device_id: device(),
            expires_at: SystemTime::UNIX_EPOCH + NOW + CAPABILITY_TOKEN_TTL,
        }
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + NOW
    }

    #[test]
    fn a_matching_token_authorizes_the_question() {
        assert_eq!(
            token().authorizes(
                &artifact("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
                ArtifactVersion::new(1).unwrap(),
                Capability::HomeReadState,
                &device(),
                now(),
            ),
            Ok(())
        );
    }

    /// Each binding is checked, and each has its own reason — the audit trail
    /// has to be able to say *which* mismatch happened, even though a caller
    /// must treat them all identically.
    #[test]
    fn every_binding_is_enforced_with_its_own_reason() {
        let t = token();
        let v1 = ArtifactVersion::new(1).unwrap();
        let v2 = ArtifactVersion::new(2).unwrap();
        let other_device: DeviceId = "01ARZ3NDEKTSV4RRFFQ69G5FB2".parse().unwrap();
        let other_app = artifact("01ARZ3NDEKTSV4RRFFQ69G5FB9");

        assert_eq!(
            t.authorizes(&other_app, v1, Capability::HomeReadState, &device(), now()),
            Err(CapabilityTokenError::WrongArtifact)
        );
        assert_eq!(
            t.authorizes(
                &t.artifact_id,
                v2,
                Capability::HomeReadState,
                &device(),
                now()
            ),
            Err(CapabilityTokenError::WrongVersion)
        );
        assert_eq!(
            t.authorizes(
                &t.artifact_id,
                v1,
                Capability::HomeSetLight,
                &device(),
                now()
            ),
            Err(CapabilityTokenError::WrongCapability)
        );
        assert_eq!(
            t.authorizes(
                &t.artifact_id,
                v1,
                Capability::HomeReadState,
                &other_device,
                now()
            ),
            Err(CapabilityTokenError::WrongDevice)
        );
    }

    /// Expiry is inclusive at the deadline: a token is dead *at* `expires_at`,
    /// not one tick after. The off-by-one here would be a real, if small,
    /// widening of every token's life.
    #[test]
    fn a_token_is_dead_at_its_deadline_not_after_it() {
        let t = token();
        let v1 = ArtifactVersion::new(1).unwrap();
        assert_eq!(
            t.authorizes(
                &t.artifact_id,
                v1,
                Capability::HomeReadState,
                &device(),
                t.expires_at
            ),
            Err(CapabilityTokenError::Expired)
        );
        assert_eq!(
            t.authorizes(
                &t.artifact_id,
                v1,
                Capability::HomeReadState,
                &device(),
                t.expires_at - Duration::from_millis(1)
            ),
            Ok(())
        );
    }

    /// Expiry is checked before any binding, so an expired token minted for a
    /// different app still reads as expired — a caller cannot use the reason to
    /// learn what a token it holds was bound to.
    #[test]
    fn expiry_is_answered_before_any_binding_is_revealed() {
        let t = token();
        assert_eq!(
            t.authorizes(
                &artifact("01ARZ3NDEKTSV4RRFFQ69G5FB9"),
                ArtifactVersion::new(9).unwrap(),
                Capability::HomeSetLight,
                &"01ARZ3NDEKTSV4RRFFQ69G5FB2".parse().unwrap(),
                t.expires_at + Duration::from_secs(1),
            ),
            Err(CapabilityTokenError::Expired)
        );
    }

    /// A token id is a secret: it must never render itself into a log or a span
    /// (invariant 5).
    #[test]
    fn a_token_id_never_debug_prints_its_value() {
        let id = CapabilityTokenId::from_bytes([0xab; 32]);
        assert!(!format!("{id:?}").contains("abab"));
        // Display is the wire form and is deliberately still the value: it is
        // what the host hands to the one client that is allowed to hold it.
        assert!(format!("{id}").starts_with("abab"));
    }

    #[test]
    fn denial_codes_are_stable_and_distinct() {
        let codes = [
            BridgeDenial::UndeclaredCapability(Capability::HomeSetLight).code(),
            BridgeDenial::UnknownApp.code(),
            BridgeDenial::NotAnApp(artifact("01ARZ3NDEKTSV4RRFFQ69G5FAV")).code(),
            BridgeDenial::InvalidTarget.code(),
            BridgeDenial::Token(CapabilityTokenError::Expired).code(),
        ];
        assert_eq!(codes[0], "app.undeclared_capability");
        let mut sorted = codes;
        sorted.sort_unstable();
        let mut deduped = sorted.to_vec();
        deduped.dedup();
        assert_eq!(deduped.len(), codes.len(), "codes must be distinct");
    }
}
