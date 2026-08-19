//! The diagnostics bundle (F10.4, NFR-07, docs/09).
//!
//! One thing an owner can read — or **send** — when the house misbehaves.
//!
//! # Redaction is the feature, not a caveat
//!
//! A bundle nobody dares share is useless: the owner sits on it, describes the
//! symptom from memory, and everyone guesses. So the requirement is not "strip
//! the secrets before sending" — a filter that must be remembered, and that
//! fails silently the day someone adds a field. The requirement is that **there
//! is no field here capable of holding a secret**.
//!
//! Look at the types: counts, enum-like identifiers, timestamps, durations,
//! booleans. Not one `String` that carries content. `AuditShapeDto` holds an
//! event *type* and a count, never a target, an actor, or a payload.
//! `RunOutcomesDto` holds tallies, never a message. There is nowhere for a
//! transcript, a tool argument, a message body or a keyring value to go — not
//! because they are removed, but because the shape has no room for them.
//!
//! That is the property the tests assert: seed a secret, a transcript and a
//! message body, generate a bundle, and find none of them anywhere in it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How many audit events of one type, and when the most recent was.
///
/// The *shape* of what happened, which is what diagnoses a fault: "forty
/// `tool.denied` in the last hour" is the whole story, and it needs no argument,
/// actor or target to tell it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditShapeDto {
    /// A host-defined event type such as `device.paired`. Never free text: these
    /// are written by the daemon, not by a model, a tool or a user.
    pub event_type: String,
    pub count: u64,
    /// RFC 3339, most recent occurrence.
    pub last_at: String,
}

/// Migration state — the first thing to check after an upgrade goes wrong.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MigrationStateDto {
    pub applied: u64,
    /// The highest applied version, or `null` on an unmigrated database.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<i64>,
}

/// Run outcomes as tallies. No prompts, no answers, no tool arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunOutcomesDto {
    pub total: u64,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
    /// Still in a non-terminal state — a large number here after a restart is
    /// itself the diagnosis.
    pub in_flight: u64,
}

/// What the daemon is costing the machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesDto {
    /// Resident set size in kibibytes, or `null` where the platform will not say.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_kib: Option<u64>,
    pub uptime_secs: u64,
}

/// `GET /api/v1/diagnostics/bundle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsBundleDto {
    /// Generated-at, RFC 3339 — so a bundle sent three days later is not read
    /// as current.
    pub generated_at: String,
    pub version: String,
    /// Adapter and capability readiness, the same map the health page reports:
    /// name → `up` | `down` | `disabled`.
    pub adapters: Vec<AdapterLineDto>,
    pub migrations: MigrationStateDto,
    /// Registered tool ids. Host-defined identifiers, and the fastest answer to
    /// "why did it not do the thing" — usually because the tool is not there.
    pub tools: Vec<String>,
    pub audit_shapes: Vec<AuditShapeDto>,
    pub runs: RunOutcomesDto,
    pub resources: ResourcesDto,
    /// Counts only. Deliberately not a device list: names are owner-chosen and
    /// can be personal ("Dad's phone"), and nothing here needs them.
    pub device_count: u64,
    pub session_count: u64,
    pub message_count: u64,
}

/// One adapter's state, flattened for a bundle a human reads top to bottom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdapterLineDto {
    pub name: String,
    /// `up` | `down` | `disabled`.
    pub state: String,
    /// The daemon's own fixed hint, e.g. "set [integrations.web_search]". A
    /// config *key*, never a value — the same rule the health endpoint follows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}
