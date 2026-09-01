//! S3 acceptance — the orchestrator labels what a run's answer may be spoken by
//! (ADR-033 §4, `docs/milestones/carried-gaps-plan.md` §2).
//!
//! The routing constraint itself has existed since F8.11 and is correct: a
//! synthesizer that reaches a third party refuses `Sensitive`. What was missing
//! is anything that ever *says* `Sensitive` — every spoken answer was labelled
//! `Normal`, so a run that read private content aloud went out in the same voice
//! as a weather answer. These tests pin the producer side of that label.
//!
//! Two producers, not one. The carried-gaps plan named "mail/calendar-reading
//! tools", but there is no mail tool in the registry and **calendar is not a
//! tool at all** — an agenda arrives through `ContextAssembler` as
//! `AssembledContext::agenda`. A per-tool field alone would therefore never fire
//! for the one case ADR-033 §4 names by name, which is why `assemble_step`
//! escalates too.

use std::sync::Arc;
use std::time::Duration;

use crate::calendar::CalendarEvent;
use crate::model::{FinishReason, ModelEvent};
use crate::orchestrator::{
    AgendaPayload, AssembledContext, ContextAssembler, ContextError, Orchestrator, RunInput,
    RunUpdate, ToolStack,
};
use crate::policy::{PolicyContext, ToolDescriptor, ToolExecutor, ToolRegistry};
use crate::testing::{
    EchoAssembler, FakeApprovalGate, FakeGrantMinter, FakeGrantValidator, FakeModel, FakeTool,
    ManualClock, RecordingAuditSink, RecordingCheckpointer, RecordingSink,
};
use async_trait::async_trait;
use jarvis_domain::ids::{DeviceId, RunId, SessionId, UserId};
use jarvis_domain::location::Sensitivity;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, SpeechSensitivity, ToolPolicy};
use jarvis_domain::run::{Run, RunBudget, RunState};
use jarvis_domain::tools::{CanonicalValue as V, ToolId, ToolProposal, ToolVersion};
use tokio_util::sync::CancellationToken;

fn ulid(seed: char) -> String {
    std::iter::repeat_n(seed, 26).collect()
}

fn ctx_with(scopes: &[&str]) -> PolicyContext {
    PolicyContext {
        user_id: ulid('1').parse::<UserId>().unwrap(),
        device_id: ulid('2').parse::<DeviceId>().unwrap(),
        granted_scopes: scopes.iter().map(|s| Scope::new(*s).unwrap()).collect(),
    }
}

fn policy(speech_sensitivity: SpeechSensitivity) -> ToolPolicy {
    ToolPolicy {
        risk: RiskLevel::R0,
        is_reversible: true,
        requires_user_presence: false,
        timeout: Duration::from_secs(5),
        required_scopes: [Scope::new("files:read").unwrap()].into_iter().collect(),
        egress: DataEgress::None,
        speech_sensitivity,
    }
}

fn descriptor(id: &str, policy: ToolPolicy, executor: Arc<dyn ToolExecutor>) -> ToolDescriptor {
    ToolDescriptor {
        id: id.parse::<ToolId>().unwrap(),
        version: ToolVersion::new(1, 0, 0),
        policy: Some(policy),
        executor,
    }
}

fn proposal(id: &str) -> ToolProposal {
    ToolProposal {
        tool_id: id.parse::<ToolId>().unwrap(),
        arguments: V::obj([("path", V::str("/projects/jarvis/README.md"))]),
    }
}

fn new_run() -> Run {
    Run::new(
        ulid('3').parse::<RunId>().unwrap(),
        ulid('4').parse::<SessionId>().unwrap(),
        RunBudget::default_interactive(),
    )
}

/// Drive a run whose single tool declares `sensitivity`, and return every
/// update the sink saw, in order.
async fn updates_for_run_using_tool(sensitivity: SpeechSensitivity) -> Vec<RunUpdate> {
    let model = FakeModel::scripted_turns([
        vec![ModelEvent::ToolProposal(proposal("fs.read"))],
        vec![
            ModelEvent::TextDelta("your file says hello".into()),
            ModelEvent::Done(FinishReason::Stop),
        ],
    ]);
    let asm = EchoAssembler;
    let cp = RecordingCheckpointer::default();
    let sink = RecordingSink::default();
    let clock = ManualClock::at_unix(1_000_000);
    let audit = RecordingAuditSink::default();
    let gate = FakeApprovalGate::approving();
    let minter = FakeGrantMinter;
    let validator = FakeGrantValidator::accepting();

    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor(
            "fs.read",
            policy(sensitivity),
            FakeTool::returning("hello"),
        ))
        .unwrap();

    let orch = Orchestrator {
        model: &model,
        context: &asm,
        checkpointer: &cp,
        sink: &sink,
        clock: &clock,
        user_id: None,
        tools: Some(ToolStack {
            registry: &registry,
            audit: &audit,
            context: ctx_with(&["files:read"]),
            approval_gate: &gate,
            grant_minter: &minter,
            grant_validator: &validator,
            arg_digest: &crate::testing::FoldingArgumentDigest,
        }),
    };

    let final_run = orch
        .drive(
            new_run(),
            RunInput {
                text: "what does the readme say".into(),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(final_run.state, RunState::Completed);
    sink.updates()
}

fn escalation_index(updates: &[RunUpdate]) -> Option<usize> {
    updates
        .iter()
        .position(|u| matches!(u, RunUpdate::SpeechSensitivityEscalated { .. }))
}

fn first_text_delta_index(updates: &[RunUpdate]) -> Option<usize> {
    updates
        .iter()
        .position(|u| matches!(u, RunUpdate::TextDelta { .. }))
}

// ---- the tool producer ---------------------------------------------------

#[tokio::test]
async fn a_sensitive_tool_escalates_the_run_before_any_answer_text() {
    let updates = updates_for_run_using_tool(SpeechSensitivity::Sensitive).await;

    let escalated = escalation_index(&updates).expect(
        "a run that used a tool declaring Sensitive must announce it — otherwise the answer \
         is spoken as Normal and ADR-033 §4's routing constraint has nothing to route on",
    );
    let first_text = first_text_delta_index(&updates)
        .expect("the scripted model streams an answer, so there is a text delta");

    // Ordering is the whole protection, not a nicety. The socket assembles
    // clauses from text deltas as they arrive; an escalation that landed after
    // the clause quoting the tool result would reach the synthesizer one
    // utterance too late, and the sensitive sentence would already be in
    // flight to a third party.
    assert!(
        escalated < first_text,
        "escalation must precede the first text delta, got escalation at {escalated} \
         and first delta at {first_text}: {updates:#?}"
    );
}

#[tokio::test]
async fn escalation_names_the_run_it_belongs_to() {
    let updates = updates_for_run_using_tool(SpeechSensitivity::Sensitive).await;
    let run_id = ulid('3').parse::<RunId>().unwrap();
    let found = updates
        .iter()
        .any(|u| matches!(u, RunUpdate::SpeechSensitivityEscalated { run_id: r } if *r == run_id));
    assert!(found, "escalation must carry its run id: {updates:#?}");
}

#[tokio::test]
async fn a_normal_tool_never_escalates() {
    // The complement matters as much as the positive case: if everything
    // escalated, the label would carry no information and the local voice would
    // silently become the only voice — which is a different product, not a
    // safer one (ADR-033 rejects "replacing Piper" explicitly).
    let updates = updates_for_run_using_tool(SpeechSensitivity::Normal).await;
    assert_eq!(
        escalation_index(&updates),
        None,
        "a tool declaring Normal must not escalate: {updates:#?}"
    );
}

// ---- the agenda producer -------------------------------------------------

/// An assembler that returns a calendar agenda, the way the real one does for
/// "what's on today" (`jarvisd::orchestrator_ports`).
struct AgendaAssembler;

#[async_trait]
impl ContextAssembler for AgendaAssembler {
    async fn assemble(
        &self,
        _run: &Run,
        input: &RunInput,
        _cancel: &CancellationToken,
    ) -> Result<AssembledContext, ContextError> {
        Ok(AssembledContext {
            prompt: input.text.clone(),
            agenda: Some(AgendaPayload {
                // Deliberately `Sensitivity::Normal`: an agenda escalates
                // because it *is* the user's schedule, not because CalDAV
                // happened to carry a `CLASS:` property. Almost no real entry
                // is flagged, so honouring only the flag would read every
                // unflagged appointment to a third party — the fail-open
                // heuristic ADR-033 §4 rules out.
                events: vec![
                    CalendarEvent::new(
                        "Mediation",
                        std::time::UNIX_EPOCH + Duration::from_secs(1_000_100),
                        std::time::UNIX_EPOCH + Duration::from_secs(1_003_700),
                        false,
                        Sensitivity::Normal,
                    )
                    .expect("fixture event is valid"),
                ],
            }),
        })
    }
}

#[tokio::test]
async fn an_agenda_escalates_the_run_even_though_no_tool_ran() {
    // The case ADR-033 §4 names by name ("calendar entries") reaches the model
    // through context assembly, not through the tool registry. A per-tool field
    // alone would leave exactly this one unlabelled.
    let model = FakeModel::streaming(["you have divorce mediation at ten"]);
    let cp = RecordingCheckpointer::default();
    let sink = RecordingSink::default();
    let clock = ManualClock::at_unix(1_000_000);

    let orch = Orchestrator {
        model: &model,
        context: &AgendaAssembler,
        checkpointer: &cp,
        sink: &sink,
        clock: &clock,
        user_id: None,
        tools: None,
    };
    let final_run = orch
        .drive(
            new_run(),
            RunInput {
                text: "what's on today".into(),
            },
            CancellationToken::new(),
        )
        .await;
    assert_eq!(final_run.state, RunState::Completed);

    let updates = sink.updates();
    let escalated = escalation_index(&updates)
        .expect("an agenda in the assembled context must escalate the run");
    let first_text = first_text_delta_index(&updates).expect("the model streamed an answer");
    assert!(
        escalated < first_text,
        "escalation must precede the answer text: {updates:#?}"
    );
}

#[tokio::test]
async fn a_run_with_no_agenda_and_no_tool_stays_normal() {
    let model = FakeModel::streaming(["it is sunny"]);
    let cp = RecordingCheckpointer::default();
    let sink = RecordingSink::default();
    let clock = ManualClock::at_unix(1_000_000);

    let orch = Orchestrator {
        model: &model,
        context: &EchoAssembler,
        checkpointer: &cp,
        sink: &sink,
        clock: &clock,
        user_id: None,
        tools: None,
    };
    orch.drive(
        new_run(),
        RunInput {
            text: "what's the weather".into(),
        },
        CancellationToken::new(),
    )
    .await;

    assert_eq!(escalation_index(&sink.updates()), None);
}
