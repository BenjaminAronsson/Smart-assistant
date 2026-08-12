//! Identity entities (docs/04 §2, docs/05 §6). Token VALUES never appear
//! here — only their hashes; the value exists transiently at the gateway.
//!
//! # Two scope vocabularies, now typed apart (M7 F7.1)
//!
//! Conflating them was a real bug — M6 gate finding B1, where every paired
//! device held only the `ui` class scope and could therefore execute **no
//! tool at all**, because `policy::evaluate` rejects on the missing-scope arm
//! before any risk logic. The two vocabularies are:
//!
//! * [`ClassScope`] — *what kind of client this is*: `ui`, `display-agent`,
//!   `voice-capture` (docs/05 §6.3). A closed enum, not free text.
//! * [`Scope`] — *what a tool requires*: `files:read`, `home:control`, …
//!   (`<area>:<capability>`, validated by the policy module).
//!
//! A device never names its own scopes. It has a [`DeviceClass`], and the
//! class decides — which is why the class is what pairing records and what
//! authorization reads. `identity.devices.scopes` keeps the pairing-time
//! snapshot for audit and diagnostics; it is **not** read back for
//! authorization, so a stale or tampered row cannot widen authority.

use crate::ids::{DeviceId, UserId};
use crate::policy::Scope;
use std::fmt;
use std::str::FromStr;
use std::time::SystemTime;
use thiserror::Error;

/// What kind of client a paired device is (docs/05 §6.3, docs/02 §13).
///
/// The class is the unit of authority: it decides both the class scopes the
/// device holds and whether it may execute tools at all. Room satellites are
/// deliberately toolless — a screen in the kitchen has no business calling
/// `message.send`, and the pairing flow gives it no way to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DeviceClass {
    /// The owner's own client — the Angular shell or a CLI on the trusted
    /// host. Holds `ui` **and every tool scope** (see [`OWNER_TOOL_SCOPES`]).
    OwnerUi,
    /// A screen: the local desktop agent (`jarvis-agent`, M3a) or a remote
    /// display node. Presents surfaces; executes nothing.
    DisplayNode,
    /// A microphone/speaker satellite with no screen.
    VoiceNode,
    /// A room satellite that both listens and shows.
    RoomNode,
}

/// A device-class scope (docs/05 §6.3). Closed vocabulary: these three
/// strings are the whole of it, and they are not [`Scope`]s — `ui` is not of
/// the form `<area>:<capability>` and never was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ClassScope {
    /// The owner-facing control UI: sessions, approvals, device management.
    Ui,
    /// May receive display directives and present surfaces.
    DisplayAgent,
    /// May open a voice capture stream and receive synthesized speech.
    VoiceCapture,
}

impl ClassScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::DisplayAgent => "display-agent",
            Self::VoiceCapture => "voice-capture",
        }
    }
}

impl fmt::Display for ClassScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every tool scope the owner's device holds, in the order the tools were
/// introduced. This is the list M6's B1 fix established; it moved here from
/// `jarvisd::auth::FIRST_DEVICE_SCOPES` in F7.1 so that *one* place decides
/// what a class may do.
///
/// Adding a tool with a new scope means adding it here, deliberately. That is
/// the intended cost: a scope nobody granted is a tool nobody can run, and
/// `jarvisd::tools::scope_coverage_tests` fails until the grant is made.
pub const OWNER_TOOL_SCOPES: &[&str] = &[
    "files:read",
    "demo:light",
    "mcp:echo",
    "web:search",
    "web:fetch",
    "browser:act",
    "coding:patch",
    "home:read",
    "home:control",
    "home:write",
    "media:control",
    "media:search",
    "message:send",
    "app:build",
];

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown device class: {0}")]
pub struct DeviceClassParseError(String);

impl DeviceClass {
    /// The wire/storage name. Stable — these strings are persisted.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OwnerUi => "owner-ui",
            Self::DisplayNode => "display-node",
            Self::VoiceNode => "voice-node",
            Self::RoomNode => "room-node",
        }
    }

    /// Every class, for exhaustive tests and for the pairing route's
    /// "which classes may a node request" check.
    pub const ALL: &'static [DeviceClass] = &[
        Self::OwnerUi,
        Self::DisplayNode,
        Self::VoiceNode,
        Self::RoomNode,
    ];

    /// The class scopes this class holds.
    pub fn class_scopes(&self) -> &'static [ClassScope] {
        match self {
            Self::OwnerUi => &[ClassScope::Ui],
            Self::DisplayNode => &[ClassScope::DisplayAgent],
            Self::VoiceNode => &[ClassScope::VoiceCapture],
            Self::RoomNode => &[ClassScope::DisplayAgent, ClassScope::VoiceCapture],
        }
    }

    /// The tool scopes this class holds. **Only the owner's device holds
    /// any** — a node's authority is to present and to capture, never to act
    /// (docs/06 §2: a satellite is Z1 with presentation capabilities).
    pub fn tool_scopes(&self) -> Vec<Scope> {
        match self {
            Self::OwnerUi => OWNER_TOOL_SCOPES
                .iter()
                .map(|s| Scope::new(*s).expect("OWNER_TOOL_SCOPES are valid scopes"))
                .collect(),
            Self::DisplayNode | Self::VoiceNode | Self::RoomNode => Vec::new(),
        }
    }

    /// Whether this class may execute tools at all — the single question the
    /// pairing route and the device list need to answer about authority.
    pub fn executes_tools(&self) -> bool {
        !self.tool_scopes().is_empty()
    }

    /// The flat scope list the gateway puts in a `PolicyContext`: class scopes
    /// first, then tool scopes. This is the ONLY definition of a device's
    /// authority.
    pub fn scopes(&self) -> Vec<String> {
        self.class_scopes()
            .iter()
            .map(|c| c.as_str().to_owned())
            .chain(
                self.tool_scopes()
                    .into_iter()
                    .map(|s| s.as_str().to_owned()),
            )
            .collect()
    }

    /// Whether a device of this class holds `scope` (either vocabulary).
    pub fn holds(&self, scope: &str) -> bool {
        self.class_scopes().iter().any(|c| c.as_str() == scope)
            || self.tool_scopes().iter().any(|s| s.as_str() == scope)
    }
}

impl FromStr for DeviceClass {
    type Err = DeviceClassParseError;

    /// Fails closed: an unrecognized stored class is an error, never a
    /// default. A row this cannot parse authenticates nothing.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .find(|c| c.as_str() == s)
            .copied()
            .ok_or_else(|| DeviceClassParseError(s.to_owned()))
    }
}

impl fmt::Display for DeviceClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A paired client device and the class that decides its authority.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: DeviceId,
    pub user_id: UserId,
    pub name: String,
    /// sha256 hex of the opaque bearer token (docs/05 §6).
    pub token_hash: String,
    pub class: DeviceClass,
    pub created_at: SystemTime,
    /// Last time this device was seen on a socket or a request (docs/04 §2);
    /// `None` until it connects. Presence detail lands in M7 F7.4.
    pub last_seen_at: Option<SystemTime>,
    pub revoked_at: Option<SystemTime>,
    /// Why the owner revoked it, for the device list and the audit trail.
    pub revoked_reason: Option<String>,
}

impl Device {
    /// Revoked tokens fail closed on the next request (docs/05 §6).
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }

    /// The scopes this device actually holds — derived from the class, never
    /// read back from storage (see the module docs).
    pub fn effective_scopes(&self) -> Vec<String> {
        self.class.scopes()
    }
}

/// Would revoking `target` leave the owner with no active `owner-ui` device,
/// and therefore no way to pair a replacement short of restarting `jarvisd`?
///
/// `active_owners` is every currently-active `owner-ui` device — the caller is
/// responsible for reading that set atomically with the revocation (the
/// Postgres store locks it `FOR UPDATE`).
///
/// This lives in the domain because it is one rule with two implementations —
/// the Postgres store and the in-memory double — and two hand-written
/// expressions of one invariant is the divergence surface the double exists to
/// remove (rust-reviewer, F7.1).
pub fn revoking_would_orphan_the_owner(active_owners: &[DeviceId], target: &DeviceId) -> bool {
    active_owners.iter().all(|id| id == target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_scopes() -> Vec<String> {
        DeviceClass::OwnerUi.scopes()
    }

    #[test]
    fn owner_tool_scopes_are_all_valid_policy_scopes() {
        // The list is `&[&str]` for ergonomics; if one of them were not a
        // legal `<area>:<capability>` scope, `tool_scopes()` would panic at
        // the first pairing rather than here.
        for raw in OWNER_TOOL_SCOPES {
            assert!(
                Scope::new(*raw).is_ok(),
                "`{raw}` is not a valid tool scope"
            );
        }
    }

    #[test]
    fn class_scopes_are_not_tool_scopes() {
        // The whole point of the split: `ui` and friends could never have
        // been `Scope`s, and a test says so rather than a comment.
        for class_scope in [
            ClassScope::Ui,
            ClassScope::DisplayAgent,
            ClassScope::VoiceCapture,
        ] {
            assert!(
                Scope::new(class_scope.as_str()).is_err(),
                "`{class_scope}` parses as a tool scope — the vocabularies have merged"
            );
        }
    }

    #[test]
    fn the_owner_device_holds_ui_and_every_tool_scope() {
        let scopes = owner_scopes();
        assert!(scopes.contains(&"ui".to_owned()));
        for tool_scope in OWNER_TOOL_SCOPES {
            assert!(
                scopes.contains(&(*tool_scope).to_owned()),
                "owner device lost `{tool_scope}` — this is M6 finding B1 returning"
            );
        }
    }

    /// The inverse of B1, and the reason F7.1 exists: a node must NOT inherit
    /// the owner's tool scopes. If a future tool scope is added to a node
    /// class by accident, this fails.
    #[test]
    fn no_node_class_holds_any_tool_scope() {
        for class in [
            DeviceClass::DisplayNode,
            DeviceClass::VoiceNode,
            DeviceClass::RoomNode,
        ] {
            assert!(
                class.tool_scopes().is_empty(),
                "{class} holds tool scopes: {:?}",
                class.tool_scopes()
            );
            assert!(!class.executes_tools(), "{class} may execute tools");
            for tool_scope in OWNER_TOOL_SCOPES {
                assert!(
                    !class.holds(tool_scope),
                    "{class} holds `{tool_scope}` — a satellite must not act"
                );
            }
        }
    }

    #[test]
    fn a_node_holds_exactly_its_presentation_and_capture_scopes() {
        assert_eq!(DeviceClass::DisplayNode.scopes(), vec!["display-agent"]);
        assert_eq!(DeviceClass::VoiceNode.scopes(), vec!["voice-capture"]);
        assert_eq!(
            DeviceClass::RoomNode.scopes(),
            vec!["display-agent", "voice-capture"]
        );
        // And no class holds `ui`: device management is the owner's alone.
        for class in [
            DeviceClass::DisplayNode,
            DeviceClass::VoiceNode,
            DeviceClass::RoomNode,
        ] {
            assert!(!class.holds(ClassScope::Ui.as_str()), "{class} holds `ui`");
        }
    }

    #[test]
    fn class_names_round_trip_and_unknown_classes_fail_closed() {
        for class in DeviceClass::ALL {
            assert_eq!(
                DeviceClass::from_str(class.as_str()).expect("round trips"),
                *class
            );
        }
        for bogus in ["", "owner", "OWNER-UI", "admin", "owner-ui "] {
            assert!(
                DeviceClass::from_str(bogus).is_err(),
                "`{bogus}` parsed as a device class"
            );
        }
    }

    #[test]
    fn the_orphan_guard_answers_the_three_cases_that_matter() {
        let a: DeviceId = "01ARZ3NDEKTSV4RRFFQ69G5FA1".parse().expect("ulid");
        let b: DeviceId = "01ARZ3NDEKTSV4RRFFQ69G5FA2".parse().expect("ulid");

        // The only owner device: refuse.
        assert!(revoking_would_orphan_the_owner(
            std::slice::from_ref(&a),
            &a
        ));
        // One of two: allow.
        assert!(!revoking_would_orphan_the_owner(
            &[a.clone(), b.clone()],
            &a
        ));
        // Revoking something that is not an owner device at all (so it is not
        // in the set) never orphans anyone — but an EMPTY owner set must not
        // read as "fine": there is nothing to protect and nothing to pair
        // with, so the caller is expected to reach here only with the target
        // included. Pinned so the vacuous-truth of `all()` is a decision, not
        // an accident.
        assert!(revoking_would_orphan_the_owner(&[], &a));
    }

    #[test]
    fn effective_scopes_come_from_the_class() {
        let device = Device {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("ulid"),
            user_id: "01ARZ3NDEKTSV4RRFFQ69G5FAW".parse().expect("ulid"),
            name: "kitchen screen".into(),
            token_hash: "deadbeef".into(),
            class: DeviceClass::DisplayNode,
            created_at: SystemTime::UNIX_EPOCH,
            last_seen_at: None,
            revoked_at: None,
            revoked_reason: None,
        };
        assert_eq!(device.effective_scopes(), vec!["display-agent"]);
        assert!(device.is_active());
    }
}
