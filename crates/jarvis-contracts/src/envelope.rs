//! WebSocket event envelope (docs/05 §3).
//!
//! Envelope fields are attached by the WS hub, never by payload authors.
//! `seq` is monotonic per connection scope; on gap or reconnect clients resync
//! via the REST snapshot endpoints. Payload stays opaque JSON at M0 — the typed
//! event unions (DomainEvent vs TransientEvent) land with the first real WS
//! events in M1 and must carry the docs/05 §3 persistence classification in
//! the type system.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One WebSocket replaces v1's three hubs; `channel` discriminates (docs/05 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Session,
    Display,
    Voice,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    /// Contract version ([`crate::CONTRACT_VERSION`]).
    pub v: u16,
    /// Monotonic per connection scope; gaps trigger client resync (NFR-13).
    pub seq: u64,
    pub channel: Channel,
    /// Event type discriminator, e.g. `run.tool.completed`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// RFC 3339 timestamp of the domain occurrence.
    pub occurred_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_version: Option<u64>,
    pub payload: serde_json::Value,
}
