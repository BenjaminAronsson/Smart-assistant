//! Risk classification and tool policy metadata (docs/06 §3, docs/05 §4). This
//! is the vocabulary the policy engine (`jarvis-application::policy`, F2.2)
//! speaks; the engine's *decisions* live there, the *types* live here so the
//! domain remains the single source of truth for what a risk tier means.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The five risk tiers (docs/06 §3, FR-05). Ordering is meaningful: higher is
/// more dangerous. R4 is prohibited and cannot be approved through any
/// conversational path (invariant #1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RiskLevel {
    /// Read-only within scope; automatic, audited.
    R0,
    /// Reversible low impact; automatic when policy permits, shown live.
    R1,
    /// External / meaningful mutation; explicit approval with exact effect.
    R2,
    /// Destructive / security / financial; strong confirmation, shortest TTL.
    R3,
    /// Prohibited; rejected, no override through conversation.
    R4,
}

impl RiskLevel {
    /// R2/R3 need an explicit `ExecutionGrant` before execution. R0/R1 are
    /// auto-authorizable (still through `policy::evaluate` + audit — there is no
    /// skip-policy shortcut, docs/06 §3). R4 is never "approved" — it is
    /// rejected outright, so it does not require approval.
    pub fn requires_approval(self) -> bool {
        matches!(self, Self::R2 | Self::R3)
    }

    /// R4 actions are refused unconditionally (credential harvesting, disabling
    /// security controls, unrestricted root shell — docs/06 §3).
    pub fn is_prohibited(self) -> bool {
        matches!(self, Self::R4)
    }

    /// Default expiry for a grant minted at this tier (docs/06 §3: R3 gets the
    /// shortest TTL). A grant that is not consumed within its TTL is dead — a
    /// stale approval cannot execute later. R0/R1 never mint grants; their value
    /// here is unused but defined for totality.
    pub fn default_grant_ttl(self) -> Duration {
        match self {
            Self::R0 | Self::R1 => Duration::from_secs(0),
            Self::R2 => Duration::from_secs(300),
            Self::R3 => Duration::from_secs(60),
            Self::R4 => Duration::from_secs(0),
        }
    }
}

/// Where a tool's data may travel (docs/06 §5 egress classification). Used to
/// keep sensitive context off external requests without explicit consent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DataEgress {
    /// No data leaves the process.
    None,
    /// Data reaches loopback/LAN services only (DB, HA, MCP child).
    Local,
    /// Data may reach external/Z5 networks (cloud APIs, the open web).
    External,
}

/// A capability scope a device/tool must hold (docs/05 §6.3), e.g.
/// `files:read`, `home:control`. Validated so scope strings stay a closed,
/// greppable vocabulary rather than free text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Scope(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid scope: {0}")]
pub struct ScopeParseError(String);

impl Scope {
    /// `<area>:<capability>` — lowercase letters/digits/underscore segments.
    pub fn new(s: impl Into<String>) -> Result<Self, ScopeParseError> {
        let s = s.into();
        let valid_segment = |seg: &str| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        };
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() == 2 && parts.iter().all(|seg| valid_segment(seg)) {
            Ok(Self(s))
        } else {
            Err(ScopeParseError(s))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A pattern a target resource must match for a policy rule or grant to apply
/// (docs/06 §4). Supports exact match and a single trailing-`*` prefix glob —
/// deliberately minimal so matching is auditable and cannot over-grant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResourcePattern(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid resource pattern: {0}")]
pub struct ResourcePatternParseError(String);

impl ResourcePattern {
    /// Returns true if `resource` is covered by this pattern. A trailing `*`
    /// matches by prefix (the text before the `*`); otherwise the match is
    /// exact. There is no interior wildcard, so a pattern can never expand
    /// across a path boundary it does not literally prefix.
    pub fn matches(&self, resource: &str) -> bool {
        match self.0.strip_suffix('*') {
            Some(prefix) => resource.starts_with(prefix),
            None => self.0 == resource,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ResourcePattern {
    type Err = ResourcePatternParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(ResourcePatternParseError(s.to_owned()));
        }
        // Only a single trailing `*` is permitted; interior wildcards would make
        // matching non-obvious and risk over-granting.
        if s.trim_end_matches('*').contains('*') {
            return Err(ResourcePatternParseError(s.to_owned()));
        }
        Ok(Self(s.to_owned()))
    }
}

impl fmt::Display for ResourcePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Host-owned policy metadata a tool is registered with (docs/05 §4). A tool
/// with no `ToolPolicy` cannot be registered (enforced in F2.2). Imported MCP
/// descriptors get this overlaid locally — a server never declares its own
/// safety (docs/06 §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPolicy {
    pub risk: RiskLevel,
    pub is_reversible: bool,
    pub requires_user_presence: bool,
    pub timeout: Duration,
    pub required_scopes: BTreeSet<Scope>,
    pub egress: DataEgress,
}

impl ToolPolicy {
    /// Whether an execution of this tool must present a valid `ExecutionGrant`.
    pub fn requires_grant(&self) -> bool {
        self.risk.requires_approval()
    }
}
