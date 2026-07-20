//! Message DTOs (docs/05 §1/§2, FR-01). Messages are immutable role/content
//! blocks (docs/04 §2); content is discriminated blocks, never one overloaded
//! string.

use crate::content::ContentBlock;
use jarvis_domain::ids::{MessageId, SessionId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageDto {
    #[schemars(with = "crate::schema::UlidString")]
    pub id: MessageId,
    #[schemars(with = "crate::schema::UlidString")]
    pub session_id: SessionId,
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
    /// RFC 3339.
    pub created_at: String,
}

/// Body of `POST /sessions/{id}/messages` (docs/05 §1). The idempotency key
/// rides the `Idempotency-Key` header (docs/05 §2), not the body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubmitMessageRequest {
    pub content: Vec<ContentBlock>,
}
