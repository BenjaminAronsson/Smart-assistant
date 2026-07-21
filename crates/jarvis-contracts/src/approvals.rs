//! Approval-surface DTOs (docs/05 §4, docs/06 §3, FR-05/07). The wire projection
//! of an R2/R3 authorization request: the human sees the **exact effect** — the
//! real tool and its concrete arguments, never a model paraphrase — and answers
//! with an [`ApprovalDecisionDto`]. Invariant #1 is why the card carries the real
//! payload: the decision authorizes precisely what is shown, and editing the
//! arguments rebinds the grant (docs/06 §4), so an approval can never execute
//! something other than what the human read.
//!
//! `proposedArguments` / `editedArguments` are opaque JSON (`serde_json::Value`,
//! the same shape the envelope payload uses) mapped to/from the domain
//! `CanonicalValue` at the gateway. The risk/egress enums are faithful, total
//! projections of the domain vocabulary in [`jarvis_domain::policy`].

use crate::schema::UlidString;
use jarvis_domain::ids::{ApprovalId, RunId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Wire projection of [`jarvis_domain::policy::RiskLevel`] (docs/06 §3). An
/// approval card only ever carries `R2`/`R3` — `R0`/`R1` auto-authorize and `R4`
/// is rejected outright — but the projection is total (no `_`) so a new tier
/// forces a decision here rather than silently mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevelDto {
    R0,
    R1,
    R2,
    R3,
    R4,
}

impl From<jarvis_domain::policy::RiskLevel> for RiskLevelDto {
    fn from(risk: jarvis_domain::policy::RiskLevel) -> Self {
        use jarvis_domain::policy::RiskLevel as R;
        match risk {
            R::R0 => Self::R0,
            R::R1 => Self::R1,
            R::R2 => Self::R2,
            R::R3 => Self::R3,
            R::R4 => Self::R4,
        }
    }
}

/// Wire projection of [`jarvis_domain::policy::DataEgress`] (docs/06 §5): how far
/// the tool's data may travel, surfaced so the human approves egress knowingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DataEgressDto {
    None,
    Local,
    External,
}

impl From<jarvis_domain::policy::DataEgress> for DataEgressDto {
    fn from(egress: jarvis_domain::policy::DataEgress) -> Self {
        use jarvis_domain::policy::DataEgress as E;
        match egress {
            E::None => Self::None,
            E::Local => Self::Local,
            E::External => Self::External,
        }
    }
}

/// What a human is asked to approve (docs/06 §3). Carries the exact effect and
/// the real proposed arguments so the approval binds precisely what is shown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalCardDto {
    /// The pending approval this card asks about; echoed back on the decision.
    #[schemars(with = "UlidString")]
    pub approval_id: ApprovalId,
    #[schemars(with = "UlidString")]
    pub run_id: RunId,
    /// The namespaced tool identifier (e.g. `message.send`).
    pub tool_id: String,
    /// A human-readable rendering of exactly what will execute — the real target
    /// and payload, never a summary (docs/06 §3). Snapshot-tested.
    pub exact_effect: String,
    /// The concrete arguments the model proposed, as opaque JSON. The human may
    /// edit these on the decision; doing so rebinds the grant (docs/06 §4).
    pub proposed_arguments: serde_json::Value,
    pub risk: RiskLevelDto,
    /// Whether the tool registered a compensating undo (docs/06 §4).
    pub reversible: bool,
    pub egress: DataEgressDto,
}

/// The verb a human answers a card with (docs/05 §4). Distinct from
/// [`ApprovalResolutionDto`]'s past-tense outcome: this is the request input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

/// Body of `POST /api/v1/runs/{id}/approvals/{approval_id}` (docs/05 §4). On
/// `approve`, `editedArguments` (when present) replaces the proposed arguments —
/// the grant binds the edited set, so executing the original would fail
/// validation (invalidation by hash, not a flag; docs/06 §4). Ignored on `deny`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDecisionDto {
    pub decision: ApprovalDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edited_arguments: Option<serde_json::Value>,
}

/// How an approval was resolved (docs/05 §3), carried by the `approval.resolved`
/// event. Past tense to the [`ApprovalDecision`] request verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResolutionDto {
    Approved,
    Denied,
}
