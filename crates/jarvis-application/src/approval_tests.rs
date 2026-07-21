//! F2.3 acceptance — the R2 approval → grant → execute flow (docs/06 §4,
//! invariant #1). Proves the orchestration: an approval mints a grant threaded
//! to the executor; a denial replans; editing the effect rebinds to the edited
//! arguments; a failed grant validation blocks execution entirely; a reversible
//! tool's undo is surfaced. The real sha2/store grant lifecycle table is F2.4.

use std::sync::Arc;
use std::time::Duration;

use crate::model::{FinishReason, ModelEvent};
use crate::orchestrator::{Orchestrator, RunInput, RunUpdate, ToolStack};
use crate::policy::{PolicyContext, ToolDescriptor, ToolExecutor, ToolRegistry};
use crate::testing::{
    EchoAssembler, FakeApprovalGate, FakeGrantMinter, FakeGrantValidator, FakeModel, FakeTool,
    ManualClock, RecordingAuditSink, RecordingCheckpointer, RecordingSink,
};
use jarvis_domain::grants::GrantError;
use jarvis_domain::ids::{DeviceId, RunId, SessionId, UserId};
use jarvis_domain::policy::{DataEgress, RiskLevel, ToolPolicy};
use jarvis_domain::run::{Run, RunBudget, RunState};
use jarvis_domain::tools::{CanonicalValue as V, ToolId, ToolProposal, ToolVersion};
use tokio_util::sync::CancellationToken;

fn ulid(seed: char) -> String {
    std::iter::repeat_n(seed, 26).collect()
}

fn ctx() -> PolicyContext {
    PolicyContext {
        user_id: ulid('1').parse::<UserId>().unwrap(),
        device_id: ulid('2').parse::<DeviceId>().unwrap(),
        granted_scopes: Default::default(),
    }
}

fn r2_policy() -> ToolPolicy {
    ToolPolicy {
        risk: RiskLevel::R2,
        is_reversible: true,
        requires_user_presence: true,
        timeout: Duration::from_secs(30),
        required_scopes: Default::default(),
        egress: DataEgress::External,
    }
}

fn args_to(recipient: &str) -> V {
    V::obj([("to", V::str(recipient)), ("body", V::str("hi"))])
}

fn send_proposal(recipient: &str) -> ToolProposal {
    ToolProposal {
        tool_id: "message.send".parse::<ToolId>().unwrap(),
        arguments: args_to(recipient),
    }
}

fn registry_with(tool: Arc<dyn ToolExecutor>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry
        .register(ToolDescriptor {
            id: "message.send".parse::<ToolId>().unwrap(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(r2_policy()),
            executor: tool,
        })
        .unwrap();
    registry
}

fn new_run() -> Run {
    Run::new(
        ulid('R').parse::<RunId>().unwrap(),
        ulid('S').parse::<SessionId>().unwrap(),
        RunBudget::default_interactive(),
    )
}

/// Turn 1 proposes message.send; turn 2 (after the tool observation) answers.
fn propose_then_answer(recipient: &str, answer: &str) -> FakeModel {
    FakeModel::scripted_turns([
        vec![ModelEvent::ToolProposal(send_proposal(recipient))],
        vec![
            ModelEvent::TextDelta(answer.into()),
            ModelEvent::Done(FinishReason::Stop),
        ],
    ])
}

#[tokio::test]
async fn approved_r2_mints_grant_and_executes_with_it() {
    let model = propose_then_answer("alice@example.com", "sent");
    let (asm, cp, sink, clock) = (
        EchoAssembler,
        RecordingCheckpointer::default(),
        RecordingSink::default(),
        ManualClock::at_unix(1_000_000),
    );
    let audit = RecordingAuditSink::default();
    let gate = FakeApprovalGate::approving();
    let minter = FakeGrantMinter;
    let validator = FakeGrantValidator::accepting();
    let tool = FakeTool::returning("ok");
    let registry = registry_with(tool.clone());

    let orch = Orchestrator {
        model: &model,
        context: &asm,
        checkpointer: &cp,
        sink: &sink,
        clock: &clock,
        tools: Some(ToolStack {
            registry: &registry,
            audit: &audit,
            context: ctx(),
            approval_gate: &gate,
            grant_minter: &minter,
            grant_validator: &validator,
        }),
    };

    let run = orch
        .drive(
            new_run(),
            RunInput {
                text: "email alice".into(),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(run.state, RunState::Completed);
    // The tool ran once, WITH a grant (R2 path).
    assert_eq!(tool.calls_with_grant(), vec![true]);
    let states = sink.states();
    assert!(states.contains(&RunState::WaitingApproval));
    assert!(states.contains(&RunState::ToolRunning));
    assert_eq!(sink.text(), "sent");
    // Approval was requested, then a grant minted.
    assert_eq!(
        audit.event_types(),
        vec!["policy.approval_requested", "grant.minted"]
    );
}

#[tokio::test]
async fn denied_r2_never_executes_and_replans() {
    let model = propose_then_answer("alice@example.com", "I could not send it.");
    let (asm, cp, sink, clock) = (
        EchoAssembler,
        RecordingCheckpointer::default(),
        RecordingSink::default(),
        ManualClock::at_unix(1_000_000),
    );
    let audit = RecordingAuditSink::default();
    let gate = FakeApprovalGate::denying();
    let minter = FakeGrantMinter;
    let validator = FakeGrantValidator::accepting();
    let tool = FakeTool::returning("SHOULD NOT SEND");
    let registry = registry_with(tool.clone());

    let orch = Orchestrator {
        model: &model,
        context: &asm,
        checkpointer: &cp,
        sink: &sink,
        clock: &clock,
        tools: Some(ToolStack {
            registry: &registry,
            audit: &audit,
            context: ctx(),
            approval_gate: &gate,
            grant_minter: &minter,
            grant_validator: &validator,
        }),
    };

    let run = orch
        .drive(
            new_run(),
            RunInput {
                text: "email alice".into(),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(run.state, RunState::Completed);
    assert_eq!(tool.call_count(), 0, "denied tool must not execute");
    assert!(sink.states().contains(&RunState::Replanning));
    assert_eq!(sink.text(), "I could not send it.");
    assert_eq!(
        audit.event_types(),
        vec!["policy.approval_requested", "approval.denied"]
    );
}

#[tokio::test]
async fn edited_arguments_bind_the_grant_and_execute() {
    // Proposal targets alice; the human edits the recipient to bob at approval.
    let model = propose_then_answer("alice@example.com", "sent");
    let (asm, cp, sink, clock) = (
        EchoAssembler,
        RecordingCheckpointer::default(),
        RecordingSink::default(),
        ManualClock::at_unix(1_000_000),
    );
    let audit = RecordingAuditSink::default();
    let gate = FakeApprovalGate::approving_with(args_to("bob@example.com"));
    let minter = FakeGrantMinter;
    let validator = FakeGrantValidator::accepting();
    let tool = FakeTool::returning("ok");
    let registry = registry_with(tool.clone());

    let orch = Orchestrator {
        model: &model,
        context: &asm,
        checkpointer: &cp,
        sink: &sink,
        clock: &clock,
        tools: Some(ToolStack {
            registry: &registry,
            audit: &audit,
            context: ctx(),
            approval_gate: &gate,
            grant_minter: &minter,
            grant_validator: &validator,
        }),
    };

    let run = orch
        .drive(
            new_run(),
            RunInput {
                text: "email alice".into(),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(run.state, RunState::Completed);
    // The EDITED arguments are what executed — not the proposal's.
    assert_eq!(tool.call_arguments(), vec![args_to("bob@example.com")]);
}

#[tokio::test]
async fn failed_grant_validation_blocks_execution() {
    let model = propose_then_answer("alice@example.com", "unused");
    let (asm, cp, sink, clock) = (
        EchoAssembler,
        RecordingCheckpointer::default(),
        RecordingSink::default(),
        ManualClock::at_unix(1_000_000),
    );
    let audit = RecordingAuditSink::default();
    let gate = FakeApprovalGate::approving();
    let minter = FakeGrantMinter;
    // The grant is minted but validation rejects it right before execution.
    let validator = FakeGrantValidator::rejecting(GrantError::ArgsMismatch);
    let tool = FakeTool::returning("SHOULD NOT SEND");
    let registry = registry_with(tool.clone());

    let orch = Orchestrator {
        model: &model,
        context: &asm,
        checkpointer: &cp,
        sink: &sink,
        clock: &clock,
        tools: Some(ToolStack {
            registry: &registry,
            audit: &audit,
            context: ctx(),
            approval_gate: &gate,
            grant_minter: &minter,
            grant_validator: &validator,
        }),
    };

    let run = orch
        .drive(
            new_run(),
            RunInput {
                text: "email alice".into(),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(run.state, RunState::Failed);
    assert_eq!(tool.call_count(), 0, "invalid grant must block execution");
    let detail = run.outcome.and_then(|o| o.detail).unwrap_or_default();
    assert!(
        detail.contains("grant.args_mismatch"),
        "detail was {detail:?}"
    );
    assert_eq!(
        audit.event_types(),
        vec![
            "policy.approval_requested",
            "grant.minted",
            "grant.rejected"
        ]
    );
}

#[tokio::test]
async fn reversible_tool_registers_a_compensation_in_the_timeline() {
    let model = propose_then_answer("alice@example.com", "sent");
    let (asm, cp, sink, clock) = (
        EchoAssembler,
        RecordingCheckpointer::default(),
        RecordingSink::default(),
        ManualClock::at_unix(1_000_000),
    );
    let audit = RecordingAuditSink::default();
    let gate = FakeApprovalGate::approving();
    let minter = FakeGrantMinter;
    let validator = FakeGrantValidator::accepting();
    let tool = FakeTool::reversible("ok", "unsend the message to alice@example.com");
    let registry = registry_with(tool.clone());

    let orch = Orchestrator {
        model: &model,
        context: &asm,
        checkpointer: &cp,
        sink: &sink,
        clock: &clock,
        tools: Some(ToolStack {
            registry: &registry,
            audit: &audit,
            context: ctx(),
            approval_gate: &gate,
            grant_minter: &minter,
            grant_validator: &validator,
        }),
    };

    let run = orch
        .drive(
            new_run(),
            RunInput {
                text: "email alice".into(),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(run.state, RunState::Completed);
    let compensation = sink.updates().into_iter().find_map(|u| match u {
        RunUpdate::CompensationRegistered { description, .. } => Some(description),
        _ => None,
    });
    assert_eq!(
        compensation.as_deref(),
        Some("unsend the message to alice@example.com")
    );
}
