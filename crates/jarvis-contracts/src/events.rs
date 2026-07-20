//! Typed WebSocket event payloads (docs/05 §3). The union is split into two
//! Rust types so the persistence classification is carried by the type system,
//! not by convention:
//!
//! * [`DomainEvent`] — persisted to the outbox and **replayable** on resync via
//!   `since` (run state, message creation, provider health, checkpoints).
//! * [`TransientEvent`] — direct broadcast, **never replayed** (token deltas).
//!
//! A `DomainEvent` can always be reconstructed into the timeline snapshot
//! (`crate::timeline::TimelineItem`); a `TransientEvent` never can — that is the
//! resync contract (NFR-13). The WS hub wraps either in a
//! [`crate::envelope::EventEnvelope`] and fills the envelope fields; payload
//! authors never touch `seq`/`occurredAt`/etc.

use crate::messages::MessageDto;
use crate::providers::ProviderDto;
use crate::runs::{RunOutcome, RunStateDto};
use jarvis_domain::ids::{RunId, SessionId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Persisted, replayable events (docs/05 §3 "persisted event categories").
/// Every variant must be representable in the timeline snapshot — a client that
/// missed it while disconnected recovers it via `GET /sessions/{id}/timeline`.
///
/// The `type` discriminator is **dotted-namespaced** (`run.started`,
/// `message.created`), matching the envelope example in docs/05 §3
/// (`run.tool.completed`) and the error-code scheme — clients route on this
/// string, so it is load-bearing and every tag is spelled explicitly below.
///
/// This union is intentionally **strict**: there is no `Unknown` catch-all
/// (unlike [`crate::content::ContentBlock`], which is open-world because blocks
/// originate from external providers). Every `DomainEvent` is authored by
/// jarvisd itself, and the web shell is served by the same binary — so producer
/// and consumer share one contract version and can never skew. A tag the reader
/// does not recognize is therefore a genuine bug we want surfaced as a decode
/// error, not silently dropped from a resync page. New variants are added
/// additively within a version (docs/05 §3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
// camelCase fields (the wire convention everywhere else in this crate); the
// dotted variant tags are set per-variant since `rename_all` cannot produce
// namespaced names.
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum DomainEvent {
    #[serde(rename = "run.started")]
    RunStarted {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        #[schemars(with = "crate::schema::UlidString")]
        session_id: SessionId,
    },
    #[serde(rename = "run.state_changed")]
    RunStateChanged {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        state: RunStateDto,
    },
    /// Degraded mode: the run is parked awaiting provider recovery (FR-12) —
    /// a visible waiting state, replayed so a reconnecting client still sees it.
    #[serde(rename = "run.queued")]
    RunQueued {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        reason: String,
    },
    #[serde(rename = "run.completed")]
    RunCompleted {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        outcome: RunOutcome,
    },
    #[serde(rename = "message.created")]
    MessageCreated { message: MessageDto },
    #[serde(rename = "provider.health_changed")]
    ProviderHealthChanged { provider: ProviderDto },
    /// Recovery checkpoint (NFR-05/13); replayed so resync reflects the last
    /// safe boundary a restart would resume from.
    #[serde(rename = "run.checkpoint_saved")]
    CheckpointSaved {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        state: RunStateDto,
    },
}

impl DomainEvent {
    /// The envelope `type` discriminator for this event (docs/05 §3). The hub
    /// copies this onto the [`crate::envelope::EventEnvelope`]. Must stay in
    /// lockstep with the `#[serde(rename)]` tags above.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::RunStarted { .. } => "run.started",
            Self::RunStateChanged { .. } => "run.state_changed",
            Self::RunQueued { .. } => "run.queued",
            Self::RunCompleted { .. } => "run.completed",
            Self::MessageCreated { .. } => "message.created",
            Self::ProviderHealthChanged { .. } => "provider.health_changed",
            Self::CheckpointSaved { .. } => "run.checkpoint_saved",
        }
    }
}

/// Disposable, never-replayed events (docs/05 §3 "not persisted"). A durable
/// snapshot (`DomainEvent`) always follows the work these describe. Dotted tags
/// and strict (no-catch-all) decoding for the same reasons as [`DomainEvent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum TransientEvent {
    /// One incremental chunk of streamed model output (FR-01).
    #[serde(rename = "text.delta")]
    TextDelta {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        text: String,
    },
}

impl TransientEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::TextDelta { .. } => "text.delta",
        }
    }
}
