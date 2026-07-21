//! Tool identity, arguments, and the canonical argument form (docs/05 §4,
//! docs/06 §3–4). Pure domain vocabulary: no I/O, no crypto, no JSON library
//! (the domain external allowlist is `serde` + `thiserror`; see
//! `xtask::arch-test`). Arguments enter the domain as a [`CanonicalValue`]
//! tree — the boundary layer (contracts/jarvisd) converts wire JSON into it.
//!
//! [`canonical_form`] is the single normalization used by grant minting and
//! validation (docs/06 §4). The actual SHA-256 of these bytes is computed where
//! `sha2` is allowed (infra, F2.4); adding a crypto dep to the pure domain is a
//! human-only decision (docs/11 §3), so the domain owns *what* canonical means
//! and infra owns *hashing* it.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use thiserror::Error;

/// A JSON-shaped value tree the domain owns (so it needs no `serde_json`).
/// Object keys live in a `BTreeMap`, which makes [`canonical_form`]
/// key-order-independent structurally rather than by a sort step that could be
/// forgotten. Floats are carried as their already-normalized shortest decimal
/// text so no float bit-pattern ever enters a hashed form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(String),
    Str(String),
    Array(Vec<CanonicalValue>),
    Object(BTreeMap<String, CanonicalValue>),
}

impl CanonicalValue {
    pub fn str(s: impl Into<String>) -> Self {
        Self::Str(s.into())
    }

    pub fn int(n: i64) -> Self {
        Self::Int(n)
    }

    pub fn array(items: impl IntoIterator<Item = CanonicalValue>) -> Self {
        Self::Array(items.into_iter().collect())
    }

    pub fn obj(entries: impl IntoIterator<Item = (&'static str, CanonicalValue)>) -> Self {
        Self::Object(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
        )
    }
}

/// Deterministic, unambiguous byte encoding of an argument tree. Type-tagged
/// and length-prefixed so no two distinct trees can collide (docs/06 §5
/// confused-deputy defence): strings carry their byte length, so a key/value
/// boundary can never be forged; scalars carry a type tag, so `"2"` and `2`
/// differ; arrays preserve order, objects sort keys via the `BTreeMap`.
pub fn canonical_form(value: &CanonicalValue) -> Vec<u8> {
    let mut buf = Vec::new();
    encode(value, &mut buf);
    buf
}

fn encode(value: &CanonicalValue, buf: &mut Vec<u8>) {
    match value {
        CanonicalValue::Null => buf.push(b'n'),
        CanonicalValue::Bool(false) => buf.extend_from_slice(b"b0"),
        CanonicalValue::Bool(true) => buf.extend_from_slice(b"b1"),
        CanonicalValue::Int(n) => {
            buf.push(b'i');
            buf.extend_from_slice(n.to_string().as_bytes());
            buf.push(b';');
        }
        CanonicalValue::Float(text) => {
            // Length-prefixed (like a string), not terminator-delimited: float
            // text is boundary-supplied and could contain any byte, so making it
            // self-delimiting keeps the encoding *provably* injective rather than
            // incidentally so (security-auditor advisory, F2.2).
            buf.push(b'f');
            buf.extend_from_slice(text.len().to_string().as_bytes());
            buf.push(b':');
            buf.extend_from_slice(text.as_bytes());
        }
        CanonicalValue::Str(s) => encode_str(s, buf),
        CanonicalValue::Array(items) => {
            buf.push(b'a');
            buf.extend_from_slice(items.len().to_string().as_bytes());
            buf.push(b':');
            for item in items {
                encode(item, buf);
            }
        }
        CanonicalValue::Object(map) => {
            buf.push(b'o');
            buf.extend_from_slice(map.len().to_string().as_bytes());
            buf.push(b':');
            // BTreeMap iterates in sorted key order → order-independent.
            for (k, v) in map {
                encode_str(k, buf);
                encode(v, buf);
            }
        }
    }
}

/// `s<byte-len>:<bytes>` — length-prefixed so the value bytes are self-
/// delimiting and cannot be confused with a following key or scalar.
fn encode_str(s: &str, buf: &mut Vec<u8>) {
    buf.push(b's');
    buf.extend_from_slice(s.len().to_string().as_bytes());
    buf.push(b':');
    buf.extend_from_slice(s.as_bytes());
}

/// A namespaced tool identifier such as `fs.read` or `home.set_light`. Dotted,
/// lowercase, at least two segments — never a ULID (tools are named, not
/// minted). Validation keeps injection-shaped junk out of the registry key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ToolId(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid tool id: {0}")]
pub struct ToolIdParseError(String);

impl ToolId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ToolId {
    type Err = ToolIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let segments: Vec<&str> = s.split('.').collect();
        let valid_segment = |seg: &str| {
            let mut chars = seg.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
                && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        };
        if segments.len() >= 2 && segments.iter().all(|seg| valid_segment(seg)) {
            Ok(Self(s.to_owned()))
        } else {
            Err(ToolIdParseError(s.to_owned()))
        }
    }
}

impl fmt::Display for ToolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A tool's semantic version (docs/05 §4). A pure `{major, minor, patch}` triple
/// rather than `semver::Version`: the domain external allowlist forbids the
/// `semver` crate, and grant binding needs only equality + display. Richer
/// range semantics, if ever required, live in an adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ToolVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid tool version (want major.minor.patch): {0}")]
pub struct ToolVersionParseError(String);

impl ToolVersion {
    pub fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl FromStr for ToolVersion {
    type Err = ToolVersionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('.').collect();
        let err = || ToolVersionParseError(s.to_owned());
        if parts.len() != 3 {
            return Err(err());
        }
        let major = parts[0].parse().map_err(|_| err())?;
        let minor = parts[1].parse().map_err(|_| err())?;
        let patch = parts[2].parse().map_err(|_| err())?;
        Ok(Self::new(major, minor, patch))
    }
}

impl fmt::Display for ToolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// What a model proposed (docs/02 §5 step 5). Text never grants authority: a
/// proposal is only ever an input to `policy::evaluate` (invariant #1), never a
/// licence to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolProposal {
    pub tool_id: ToolId,
    pub arguments: CanonicalValue,
}

/// The concrete, registry-resolved call the executor runs — carries the exact
/// version so the grant binds a specific tool build (docs/06 §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInvocation {
    pub tool_id: ToolId,
    pub tool_version: ToolVersion,
    pub arguments: CanonicalValue,
}

/// A tool's output (docs/02 §5 step 9). `truncated` records that the result
/// validator size-capped the content (docs/06 §5 tool-result smuggling).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub content: String,
    pub truncated: bool,
}

/// Terminal outcomes of an attempted tool execution. Grant-specific failures
/// live in [`crate::grants::GrantError`]; this covers the execution itself.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ToolError {
    #[error("tool timed out after {0:?}")]
    Timeout(Duration),
    #[error("tool execution was cancelled")]
    Cancelled,
    #[error("tool result failed schema validation: {0}")]
    SchemaInvalid(String),
    #[error("policy denied the tool call: {0}")]
    Denied(String),
    #[error("tool execution failed: {0}")]
    ExecutionFailed(String),
}
