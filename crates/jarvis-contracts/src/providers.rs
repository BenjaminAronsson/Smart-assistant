//! Model-provider health DTOs (docs/05 §1, FR-11/12). Surfaces profile health,
//! quota state, and the reset window so the client can show a visible degraded
//! mode rather than a silent stall (ADR-011).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderState {
    Healthy,
    /// Reachable but degraded (e.g. rate-limited); runs may queue.
    Degraded,
    /// Unusable (quota exhausted / auth missing / network down); LLM runs queue.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuotaState {
    /// RFC 3339 instant the quota/rate window resets, when the provider
    /// reports one — the user sees *when* service returns (docs/03 §4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDto {
    /// Profile id, e.g. `claude-cli` (docs/03 §4).
    pub id: String,
    pub state: ProviderState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota: Option<QuotaState>,
    /// Stable reason code only — never raw provider/driver text (docs/06 §5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProvidersResponse {
    pub providers: Vec<ProviderDto>,
}

/// Model provider invocation request (internal, not wire).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInvokeRequest {
    /// Messages in conversation order (system, then alternating user/assistant).
    pub messages: Vec<ModelMessage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelMessage {
    pub role: String, // "user", "assistant", "system"
    pub content: String,
}

/// Stream events from Claude CLI (streamed as newline-delimited JSON).
/// See `claude api messages stream` output format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderStreamEvent {
    /// Streaming output event; carries delta text or tool input.
    #[serde(rename_all = "camelCase")]
    ContentBlockDelta { index: i64, delta: Delta },
    /// Message finished; carries stop reason.
    #[serde(rename_all = "camelCase")]
    MessageStop { message: Message },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    #[serde(rename_all = "camelCase")]
    TextDelta { text: String },
    #[serde(rename_all = "camelCase")]
    InputJsonDelta { partial_json: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub content: Vec<ContentBlock>,
    pub model: String,
    pub stop_reason: String,
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
}

/// Model provider response, streamed line-by-line as JSON or one-shot.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelResponse {
    /// Streaming response: events arrive over time.
    Stream(Vec<ProviderStreamEvent>),
    /// One-shot response from complete message.
    Complete(Message),
}
