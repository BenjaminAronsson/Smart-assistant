//! Memory review and forget wire contracts (FR-16, docs/05 §1/§7).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::schema::UlidString;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLayerDto {
    Working,
    Episodic,
    Semantic,
    Procedural,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemorySourceDto {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScopeDto {
    User,
    Session(String),
    Project(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RetentionDto {
    UntilForgotten,
    ExpiresAt(String),
    Session,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemoryDto {
    #[schemars(with = "UlidString")]
    pub id: String,
    pub layer: MemoryLayerDto,
    pub text: String,
    pub source: MemorySourceDto,
    pub scope: MemoryScopeDto,
    pub retention: RetentionDto,
    pub confidence: f32,
    pub sensitivity: String,
    pub pinned: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemoryListResponse {
    pub memories: Vec<MemoryDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PatchMemoryRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<RetentionDto>,
}
