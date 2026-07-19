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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
// snake_case variant tags (docs/05 §3 event names), camelCase fields (the
// wire convention everywhere else in this crate).
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DomainEvent {
    RunStarted {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        #[schemars(with = "crate::schema::UlidString")]
        session_id: SessionId,
    },
    RunStateChanged {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        state: RunStateDto,
    },
    /// Degraded mode: the run is parked awaiting provider recovery (FR-12) —
    /// a visible waiting state, replayed so a reconnecting client still sees it.
    RunQueued {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        reason: String,
    },
    RunCompleted {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        outcome: RunOutcome,
    },
    MessageCreated {
        message: MessageDto,
    },
    ProviderHealthChanged {
        provider: ProviderDto,
    },
    /// Recovery checkpoint (NFR-05/13); replayed so resync reflects the last
    /// safe boundary a restart would resume from.
    CheckpointSaved {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        state: RunStateDto,
    },
}

impl DomainEvent {
    /// The envelope `type` discriminator for this event (docs/05 §3). The hub
    /// copies this onto the [`crate::envelope::EventEnvelope`].
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::RunStarted { .. } => "run_started",
            Self::RunStateChanged { .. } => "run_state_changed",
            Self::RunQueued { .. } => "run_queued",
            Self::RunCompleted { .. } => "run_completed",
            Self::MessageCreated { .. } => "message_created",
            Self::ProviderHealthChanged { .. } => "provider_health_changed",
            Self::CheckpointSaved { .. } => "checkpoint_saved",
        }
    }
}

/// Disposable, never-replayed events (docs/05 §3 "not persisted"). A durable
/// snapshot (`DomainEvent`) always follows the work these describe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum TransientEvent {
    /// One incremental chunk of streamed model output (FR-01).
    TextDelta {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        text: String,
    },
}

impl TransientEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::TextDelta { .. } => "text_delta",
        }
    }
}
