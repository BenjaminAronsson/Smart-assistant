//! Automation wire shapes (FR-17, docs/05 §1, F8.6/F8.7).
//!
//! Note what is **absent**, because it is the whole design: there is no
//! `scopes` field and no `approved` flag on the wire, in either direction. An
//! automation is a stored *intention*; its authority is resolved at fire time
//! from its creator's current scopes. A client cannot state what an automation
//! is allowed to do, because stating it would not make it so.
//!
//! `createdByDeviceId` is returned but never accepted: the server takes the
//! creator from the authenticated device, so a client cannot create an
//! automation that borrows somebody else's authority.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What makes an automation fire. A closed vocabulary — an open-ended
/// predicate would be a path from client (or model) text to a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum TriggerDto {
    /// Every day at a wall-clock time, local to the daemon.
    #[serde(rename = "daily_at")]
    DailyAt {
        /// 0–1439. Minutes rather than an instant: "07:00" means seven tomorrow
        /// as well as today.
        minutes_since_midnight: u16,
    },
    /// A Home Assistant entity entering a state (presence, zone).
    #[serde(rename = "ha_state")]
    HomeAssistantState { entity_id: String, state: String },
}

/// One automation, as the settings surface renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AutomationDto {
    #[schemars(with = "crate::schema::UlidString")]
    pub id: jarvis_domain::ids::AutomationId,
    /// Human label, sanitized. Rendered as text, never as markup.
    pub name: String,
    pub trigger: TriggerDto,
    /// The tool it proposes. Naming a tool is not authorizing it.
    pub tool_id: String,
    /// The arguments it proposes, as JSON.
    pub arguments: serde_json::Value,
    pub enabled: bool,
    /// Whose authority is consulted **at fire time**. Returned so the owner can
    /// see which device an automation depends on — revoke that device and this
    /// automation stops, by design.
    #[schemars(with = "crate::schema::UlidString")]
    pub created_by_device_id: jarvis_domain::ids::DeviceId,
    /// RFC 3339 UTC.
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<String>,
}

/// `POST /api/v1/automations`.
///
/// Creating an automation is itself an R2 action: it is a durable, unattended
/// capability to act, and the owner approves it the same way they approve
/// anything else that changes the world.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateAutomationRequest {
    pub name: String,
    pub trigger: TriggerDto,
    pub tool_id: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// `PATCH /api/v1/automations/{id}` — the one mutation, deliberately.
///
/// Enabling and disabling is all an edit can do. Changing what an automation
/// *does* is creating a different automation, which keeps the execution
/// history joined to a thing that never silently changed meaning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAutomationRequest {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AutomationListResponse {
    pub automations: Vec<AutomationDto>,
}

/// One past firing. A **denial is the most important row here**: "it ran and
/// nothing happened" and "it was refused" are otherwise identical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AutomationExecutionDto {
    /// RFC 3339 UTC.
    pub occurred_at: String,
    /// `executed` | `needs_approval` | `denied` | `failed`.
    pub outcome: String,
    /// Why it was refused, or how it failed. Closed-vocabulary policy reasons
    /// and adapter-neutral failure text — never raw provider strings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AutomationHistoryResponse {
    pub executions: Vec<AutomationExecutionDto>,
}
