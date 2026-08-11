//! Capability-bridge wire DTOs (F6.5, FR-18, docs/06 §6).
//!
//! Two endpoints, and the split between them is the design: **minting** a token
//! and **spending** it are separate operations, so a token is bound before any
//! operation is named and cannot be widened afterwards.
//!
//! Everything a generated app can influence appears here and nowhere else: a
//! capability from the closed vocabulary, a target string, and (where the
//! operation takes one) a value. There is no field for a tool id, an argument
//! name, a risk tier or a grant — an app names an operation, it never describes
//! a call (invariant 1).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::appspec::CapabilityDto;

/// `POST /api/v1/apps/{id}/versions/{version}/capability-tokens` — ask for a
/// short-lived, single-use token for one declared capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MintCapabilityTokenRequest {
    pub capability: CapabilityDto,
}

/// The minted token. The value is a secret in the same sense a device token is:
/// it is returned once, to one authenticated caller, and is spent by use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityTokenDto {
    /// Opaque 32-byte hex.
    pub token: String,
    /// RFC 3339 instant after which the token is dead.
    pub expires_at: String,
    /// Echoed so a client cannot mix up which token is for which capability.
    pub capability: CapabilityDto,
}

/// `POST /api/v1/apps/{id}/versions/{version}/invoke` — exchange a token for one
/// operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvokeCapabilityRequest {
    pub capability: CapabilityDto,
    /// The resource the operation applies to. Validated by the domain and then
    /// re-resolved by the backing tool's own allowlist — naming it confers
    /// nothing (ADR-029).
    pub target: String,
    /// The value the operation takes, where it takes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub token: String,
}

/// What the operation produced. Deliberately narrow: a content string and
/// whether it was truncated. No grant, no policy decision, no audit id — an app
/// learns the answer to its question and nothing about the machinery that
/// decided to answer it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityResultDto {
    pub content: String,
    pub truncated: bool,
}
