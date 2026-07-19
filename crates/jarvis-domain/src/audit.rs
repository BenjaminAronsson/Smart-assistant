//! Audit event shape (docs/04 §2, invariant 6). The chain hash and storage
//! order are infra concerns; the domain defines what an event says. The
//! payload is carried as already-serialized JSON text so the domain stays
//! free of JSON-library dependencies.

use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq)]
pub struct AuditEvent {
    pub occurred_at: SystemTime,
    /// Who acted: `user:<ulid>`, `device:<ulid>`, or `system`.
    pub actor: String,
    /// Stable dotted event name, e.g. `session.created`.
    pub event_type: String,
    /// What was acted on: `session:<ulid>` etc.
    pub target: String,
    /// Trace/run correlation (NFR-07: every side effect links back).
    pub correlation_id: Option<String>,
    /// JSON text; infra canonicalizes it before hashing/storing.
    pub payload_json: String,
}
