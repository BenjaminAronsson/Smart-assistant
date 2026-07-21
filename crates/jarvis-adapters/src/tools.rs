//! Native + example-tier tools (F2.6, docs/06 §3, docs/08 §2 step 7–8).
//!
//! Each tool implements the [`ToolExecutor`] port
//! ([`jarvis_application::policy::ToolExecutor`]) and carries host-owned
//! [`ToolPolicy`] metadata. Registration and the live [`ToolStack`] wiring live
//! in the host (jarvisd, Slice 3); an executor is never callable except through
//! a proposal that [`jarvis_application::policy::evaluate`] authorized — the
//! tools here add no side-effect path that bypasses the policy engine
//! (invariant #1).
//!
//! Three tiers are exercised:
//! - [`fs_read`] — a **real** R0 native tool: read a project file within an
//!   allowlisted root, read-only, path-traversal denied (exit evidence #1).
//! - [`example_light`] — a **reversible R1 example** that registers a
//!   compensating undo (stand-in for the M5 Home Assistant `home.set_light`;
//!   drives golden 4 without pulling the HA adapter forward).
//! - [`example_message`] — a **fake R2 external tool** (`message.send`
//!   stand-in; the real SMTP adapter is M4/ADR-026) to drive the
//!   approval → grant → execute → edit-invalidation flow (golden 5).
//!
//! The example tools are clearly marked tier demonstrations, not shipping
//! integrations — they perform no real external effect.

pub mod example_light;
pub mod example_message;
pub mod fs_read;
pub mod timeout;

use jarvis_domain::tools::{CanonicalValue, ToolError};

/// Extract a required string argument from an invocation's `CanonicalValue`
/// object. A missing key, a non-object argument tree, or a non-string value is a
/// caller (model) error, surfaced as [`ToolError::ExecutionFailed`] with a
/// stable, non-sensitive message (never the raw argument value — invariant #5).
fn required_str<'a>(args: &'a CanonicalValue, key: &str) -> Result<&'a str, ToolError> {
    let CanonicalValue::Object(map) = args else {
        return Err(ToolError::ExecutionFailed(
            "arguments must be an object".to_owned(),
        ));
    };
    match map.get(key) {
        Some(CanonicalValue::Str(s)) => Ok(s),
        Some(_) => Err(ToolError::ExecutionFailed(format!(
            "argument `{key}` must be a string"
        ))),
        None => Err(ToolError::ExecutionFailed(format!(
            "missing required argument `{key}`"
        ))),
    }
}

/// Assert an arguments object carries `key` as a string — the schema check a
/// tool's `validate_args` runs (CF-9, docs/06 §4). The orchestrator calls
/// `validate_args` on the human's *approved* (possibly edited) arguments BEFORE
/// a grant binds, so a malformed edit is rejected at approval time rather than
/// failing later inside `execute`. Returns [`ToolError::SchemaInvalid`] — the
/// orchestrator maps it to a run failure — and echoes only the static argument
/// name, never the value (invariant #5). Distinct from [`required_str`], which
/// runs at execution time and yields [`ToolError::ExecutionFailed`].
fn require_str_arg(args: &CanonicalValue, key: &str) -> Result<(), ToolError> {
    match args {
        CanonicalValue::Object(map) => match map.get(key) {
            Some(CanonicalValue::Str(_)) => Ok(()),
            _ => Err(ToolError::SchemaInvalid(format!(
                "argument `{key}` must be a string"
            ))),
        },
        _ => Err(ToolError::SchemaInvalid(
            "arguments must be an object".to_owned(),
        )),
    }
}
