//! ULID-backed identifier newtypes (docs/04 §2). IDs are opaque strings on the
//! wire; database sequences never leak. Generation happens at the edges (infra
//! owns randomness) — the domain only validates and carries them, staying pure.

use std::fmt;
use std::str::FromStr;

/// Canonical ULID text: 26 chars of Crockford base32, uppercase (no I, L, O, U).
const ULID_LEN: usize = 26;
const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdParseError {
    #[error("id must be {ULID_LEN} characters, got {0}")]
    InvalidLength(usize),
    #[error("id contains non-Crockford-base32 character {0:?}")]
    InvalidCharacter(char),
}

fn validate_ulid(s: &str) -> Result<(), IdParseError> {
    if s.chars().count() != ULID_LEN {
        return Err(IdParseError::InvalidLength(s.chars().count()));
    }
    match s
        .chars()
        .find(|c| !c.is_ascii() || !CROCKFORD.contains(&(*c as u8)))
    {
        Some(bad) => Err(IdParseError::InvalidCharacter(bad)),
        None => Ok(()),
    }
}

macro_rules! ulid_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                validate_ulid(s)?;
                Ok(Self(s.to_owned()))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                validate_ulid(&raw).map_err(serde::de::Error::custom)?;
                Ok(Self(raw))
            }
        }
    };
}

ulid_id!(
    /// Conversation session identifier (docs/04 §2).
    SessionId
);
ulid_id!(
    /// Paired client/node device identifier (docs/04 §2).
    DeviceId
);
ulid_id!(
    /// Owner identity identifier (docs/04 §2).
    UserId
);
ulid_id!(
    /// Orchestrator run identifier (docs/04 §2).
    RunId
);
