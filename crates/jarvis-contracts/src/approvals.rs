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
#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    // The concrete arguments the model proposed, as opaque JSON (the same shape
    // the envelope payload uses). The human may edit these on the decision, which
    // rebinds the grant (docs/06 §4). A plain `//` comment, not `///`: a doc
    // comment would attach a schema `description`, which the codegen tool renders
    // as an object-only TS type — but these arguments are arbitrary JSON, so this
    // must generate `unknown`, matching `edited_arguments` and `EventEnvelope.payload`.
    pub proposed_arguments: serde_json::Value,
    pub risk: RiskLevelDto,
    /// Whether the tool registered a compensating undo (docs/06 §4).
    pub reversible: bool,
    pub egress: DataEgressDto,
}

// CF-12: the wire twin of the redacted domain `ApprovalRequest`. `exact_effect`
// (real target + payload the human sees) and `proposed_arguments` are the same
// sensitive values the domain type redacts — keep them out of `Debug`, which can
// reach logs on the REST/WS approval path (invariant #5). `Serialize` still
// carries them to the UI verbatim; only `Debug` is redacted.
impl std::fmt::Debug for ApprovalCardDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalCardDto")
            .field("approval_id", &self.approval_id)
            .field("run_id", &self.run_id)
            .field("tool_id", &self.tool_id)
            .field("exact_effect", &"<redacted>")
            .field("proposed_arguments", &"<redacted>")
            .field("risk", &self.risk)
            .field("reversible", &self.reversible)
            .field("egress", &self.egress)
            .finish()
    }
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
#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDecisionDto {
    pub decision: ApprovalDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edited_arguments: Option<serde_json::Value>,
}

// CF-12: `edited_arguments` is the human's (possibly secret) rebinding payload —
// redact it from `Debug` while `Serialize` still carries it (invariant #5).
impl std::fmt::Debug for ApprovalDecisionDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalDecisionDto")
            .field("decision", &self.decision)
            .field(
                "edited_arguments",
                &self.edited_arguments.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// How an approval was resolved (docs/05 §3), carried by the `approval.resolved`
/// event. Past tense to the [`ApprovalDecision`] request verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResolutionDto {
    Approved,
    Denied,
}

#[cfg(test)]
mod cf12_debug_redaction {
    use super::*;

    const APPROVAL_ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const RUN_ULID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";

    #[test]
    fn approval_card_dto_debug_redacts_effect_and_arguments() {
        let card = ApprovalCardDto {
            approval_id: APPROVAL_ULID.parse().unwrap(),
            run_id: RUN_ULID.parse().unwrap(),
            tool_id: "message.send".to_owned(),
            exact_effect: "Email carol@example.com: secret-body-text".to_owned(),
            proposed_arguments: serde_json::json!({ "body": "secret-body-text" }),
            risk: RiskLevelDto::R2,
            reversible: false,
            egress: DataEgressDto::External,
        };
        let rendered = format!("{card:?}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(
            !rendered.contains("carol@example.com"),
            "leaked effect: {rendered}"
        );
        assert!(
            !rendered.contains("secret-body-text"),
            "leaked args: {rendered}"
        );
        // Serialize must still carry the real values to the UI.
        let json = serde_json::to_string(&card).unwrap();
        assert!(
            json.contains("carol@example.com"),
            "serialize must keep the effect"
        );
    }

    #[test]
    fn approval_decision_dto_debug_redacts_edited_arguments() {
        let decision = ApprovalDecisionDto {
            decision: ApprovalDecision::Approve,
            edited_arguments: Some(serde_json::json!({ "body": "secret-body-text" })),
        };
        let rendered = format!("{decision:?}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(!rendered.contains("secret-body-text"), "leaked: {rendered}");
    }
}
