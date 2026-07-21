//! F2.5: the approval-surface wire shapes (docs/05 §4, docs/06 §3). The card is
//! what a human reads before authorizing an R2/R3 effect, so its serialization —
//! especially the exact-effect string and the *real* proposed arguments — is
//! pinned here: invariant #1 rests on the human approving precisely what runs.

use jarvis_contracts::approvals::{
    ApprovalCardDto, ApprovalDecision, ApprovalDecisionDto, ApprovalResolutionDto, DataEgressDto,
    RiskLevelDto,
};
use serde_json::json;

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const APPROVAL: &str = "01BX5ZZKBKACTAV9WEVGEMMVS1";

fn sample_card() -> ApprovalCardDto {
    ApprovalCardDto {
        approval_id: APPROVAL.parse().unwrap(),
        run_id: RUN.parse().unwrap(),
        tool_id: "message.send".into(),
        exact_effect: "message.send {to=\"bob@example.com\", body=\"ping\"}".into(),
        proposed_arguments: json!({ "to": "bob@example.com", "body": "ping" }),
        risk: RiskLevelDto::R2,
        reversible: false,
        egress: DataEgressDto::External,
    }
}

#[test]
fn approval_card_serializes_camelcase_with_the_exact_effect_and_real_args() {
    let value = serde_json::to_value(sample_card()).unwrap();
    assert_eq!(
        value,
        json!({
            "approvalId": APPROVAL,
            "runId": RUN,
            "toolId": "message.send",
            "exactEffect": "message.send {to=\"bob@example.com\", body=\"ping\"}",
            "proposedArguments": { "to": "bob@example.com", "body": "ping" },
            "risk": "r2",
            "reversible": false,
            "egress": "external",
        })
    );
}

#[test]
fn approval_card_round_trips() {
    let card = sample_card();
    let back: ApprovalCardDto =
        serde_json::from_value(serde_json::to_value(&card).unwrap()).unwrap();
    assert_eq!(back, card);
}

#[test]
fn approve_decision_carries_edited_arguments_when_present() {
    let decision = ApprovalDecisionDto {
        decision: ApprovalDecision::Approve,
        edited_arguments: Some(json!({ "to": "carol@example.com" })),
    };
    let value = serde_json::to_value(&decision).unwrap();
    assert_eq!(
        value,
        json!({ "decision": "approve", "editedArguments": { "to": "carol@example.com" } })
    );
    let back: ApprovalDecisionDto = serde_json::from_value(value).unwrap();
    assert_eq!(back, decision);
}

#[test]
fn deny_decision_omits_edited_arguments() {
    let decision = ApprovalDecisionDto {
        decision: ApprovalDecision::Deny,
        edited_arguments: None,
    };
    let value = serde_json::to_value(&decision).unwrap();
    // `editedArguments` is skipped when absent — a denial carries no payload.
    assert_eq!(value, json!({ "decision": "deny" }));
    let back: ApprovalDecisionDto = serde_json::from_value(value).unwrap();
    assert_eq!(back, decision);
}

#[test]
fn risk_and_egress_project_every_domain_variant() {
    use jarvis_domain::policy::{DataEgress, RiskLevel};
    // Totality: the wire projection covers every domain variant (no `_` arm), so
    // a new tier/egress class fails to compile rather than mapping silently.
    for (domain, wire) in [
        (RiskLevel::R0, RiskLevelDto::R0),
        (RiskLevel::R1, RiskLevelDto::R1),
        (RiskLevel::R2, RiskLevelDto::R2),
        (RiskLevel::R3, RiskLevelDto::R3),
        (RiskLevel::R4, RiskLevelDto::R4),
    ] {
        assert_eq!(RiskLevelDto::from(domain), wire);
    }
    for (domain, wire) in [
        (DataEgress::None, DataEgressDto::None),
        (DataEgress::Local, DataEgressDto::Local),
        (DataEgress::External, DataEgressDto::External),
    ] {
        assert_eq!(DataEgressDto::from(domain), wire);
    }
}

#[test]
fn resolution_outcomes_serialize_past_tense() {
    assert_eq!(
        serde_json::to_value(ApprovalResolutionDto::Approved).unwrap(),
        json!("approved")
    );
    assert_eq!(
        serde_json::to_value(ApprovalResolutionDto::Denied).unwrap(),
        json!("denied")
    );
}
