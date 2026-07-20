//! Conversation entities (docs/04 §2). Pure data + invariant-preserving
//! constructors; time is passed in (the domain never reads clocks).

use crate::ids::{MessageId, SessionId};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Active,
    Archived,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: SessionId,
    pub title: Option<String>,
    pub status: SessionStatus,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}

impl Session {
    /// A new session starts active with equal created/updated timestamps.
    pub fn new(id: SessionId, title: Option<String>, now: SystemTime) -> Self {
        Self {
            id,
            title,
            status: SessionStatus::Active,
            created_at: now,
            updated_at: now,
        }
    }
}

/// Who authored a message (docs/04 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

/// An immutable conversation message (docs/04 §2). M1 is text-only; the wire
/// carries discriminated content blocks (docs/05 §2) and richer block kinds
/// (images, tool calls, artifacts) are added additively in later milestones —
/// the domain keeps just the text for now, and infra maps it to/from the JSON
/// block array stored in `conversation.messages.content`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    pub session_id: SessionId,
    pub role: MessageRole,
    pub text: String,
    pub created_at: SystemTime,
}

impl Message {
    pub fn new(
        id: MessageId,
        session_id: SessionId,
        role: MessageRole,
        text: String,
        created_at: SystemTime,
    ) -> Self {
        Self {
            id,
            session_id,
            role,
            text,
            created_at,
        }
    }
}
