//! Timeline snapshot (docs/05 §1, NFR-13). The resync source: on a WS gap or
//! reconnect the client fetches this to replay the **persisted** history since
//! a sequence cursor. Only messages and [`DomainEvent`]s appear here — transient
//! deltas are intentionally absent (they are re-derived by the next run, never
//! replayed).

use crate::events::DomainEvent;
use crate::messages::MessageDto;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One persisted entry in a session's history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimelineItem {
    Message {
        message: MessageDto,
    },
    /// Any persisted run event — the same [`DomainEvent`] that streamed live.
    RunEvent {
        event: DomainEvent,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TimelineResponse {
    pub items: Vec<TimelineItem>,
    /// Sequence cursor to pass as `since` for the next page / next resync;
    /// absent when the snapshot reaches the head.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_since: Option<u64>,
}
