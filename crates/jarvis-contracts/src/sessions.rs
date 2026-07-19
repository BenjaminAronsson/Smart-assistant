//! Session DTOs (docs/05 §1, FR-02). M0 covers create/get/basic list; search,
//! branch, and archive commands land in M1+ additively.

use jarvis_domain::ids::SessionId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Archived,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionDto {
    #[schemars(with = "crate::schema::UlidString")]
    pub id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub status: SessionStatus,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339.
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionListResponse {
    pub sessions: Vec<SessionDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}
