//! Execution grants and their binding fields (docs/06 §4, docs/05 §4). A grant
//! is the *only* thing that authorizes an R2+ tool call (invariant #1): minted
//! on human approval, bound to exact arguments, validated again immediately
//! before execution.
//!
//! The domain defines the grant's shape and its pure predicates (expiry,
//! binding equality). Minting randomness and SHA-256 computation happen where
//! `getrandom`/`sha2` are allowed (infra, F2.4); the domain external allowlist
//! is `serde` + `thiserror`.

use std::fmt;
use std::str::FromStr;
use std::time::SystemTime;

use thiserror::Error;

use crate::ids::{DeviceId, RunId, UserId};
use crate::policy::ResourcePattern;
use crate::tools::{ToolId, ToolVersion};

/// A 32-byte value rendered as lowercase hex. Shared container for both the
/// SHA-256 of normalized arguments and the random [`GrantId`]. The domain never
/// *computes* a SHA-256 (no crypto dep) — it carries one produced by infra.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256([u8; 32]);

/// A cryptographically random, single-use grant identifier (docs/06 §4). Same
/// 32-byte hex representation as [`Sha256`] but a distinct type so a hash can
/// never be mistaken for an id.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GrantId([u8; 32]);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid 32-byte hex value")]
pub struct HexParseError;

fn to_hex(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        // Lowercase hex, two chars per byte.
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    s
}

fn from_hex(s: &str) -> Result<[u8; 32], HexParseError> {
    if s.len() != 64 {
        return Err(HexParseError);
    }
    let mut out = [0u8; 32];
    let bytes = s.as_bytes();
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = (bytes[i * 2] as char).to_digit(16).ok_or(HexParseError)?;
        let lo = (bytes[i * 2 + 1] as char)
            .to_digit(16)
            .ok_or(HexParseError)?;
        *slot = ((hi << 4) | lo) as u8;
    }
    Ok(out)
}

macro_rules! hex32 {
    ($name:ident) => {
        impl $name {
            pub fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            pub fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&to_hex(&self.0))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), to_hex(&self.0))
            }
        }

        impl FromStr for $name {
            type Err = HexParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(from_hex(s)?))
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&to_hex(&self.0))
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                Ok(Self(from_hex(&raw).map_err(serde::de::Error::custom)?))
            }
        }
    };
}

hex32!(Sha256);
hex32!(GrantId);

/// A single-use authorization to execute one exact tool call (docs/06 §4,
/// docs/05 §4). Every field is a binding: any change to the tool, its version,
/// the normalized arguments, the target resource, the actor, device, or run
/// invalidates the grant. Validation (F2.3) re-checks all of this immediately
/// before execution, never trusting that the decision-time state still holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionGrant {
    pub grant_id: GrantId,
    pub user_id: UserId,
    pub device_id: DeviceId,
    pub run_id: RunId,
    pub tool_id: ToolId,
    pub tool_version: ToolVersion,
    pub normalized_args_sha256: Sha256,
    pub target_resource: ResourcePattern,
    pub expires_at: SystemTime,
    pub single_use: bool,
}

impl ExecutionGrant {
    /// A grant is expired once `now` reaches or passes its expiry. Expiry is
    /// checked at validation time against the infra clock, never trusted from a
    /// caller-supplied timestamp.
    pub fn is_expired(&self, now: SystemTime) -> bool {
        now >= self.expires_at
    }
}

/// Why a grant failed validation (docs/06 §4, skill `policy-grants`). Each maps
/// to a stable machine error code carried across the API boundary (docs/05 §7).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GrantError {
    #[error("grant has expired")]
    Expired,
    #[error("grant was already consumed")]
    Consumed,
    #[error("grant arguments do not match the proposed call")]
    ArgsMismatch,
    #[error("grant is bound to a different run")]
    WrongRun,
    #[error("grant is bound to a different tool or version")]
    WrongTool,
    #[error("grant is bound to a different actor or device")]
    WrongActor,
    #[error("grant target does not cover the requested resource")]
    ResourceMismatch,
    #[error("no grant was presented for an action that requires one")]
    Missing,
}

impl GrantError {
    /// The stable code emitted in audit events and RFC 9457 problem bodies.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Expired => "grant.expired",
            Self::Consumed => "grant.consumed",
            Self::ArgsMismatch => "grant.args_mismatch",
            Self::WrongRun => "grant.wrong_run",
            Self::WrongTool => "grant.wrong_tool",
            Self::WrongActor => "grant.wrong_actor",
            Self::ResourceMismatch => "grant.resource_mismatch",
            Self::Missing => "grant.missing",
        }
    }
}
