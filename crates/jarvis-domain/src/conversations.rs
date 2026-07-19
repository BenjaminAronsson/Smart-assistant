//! Conversation entities (docs/04 §2). Pure data + invariant-preserving
//! constructors; time is passed in (the domain never reads clocks).

use crate::ids::SessionId;
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
