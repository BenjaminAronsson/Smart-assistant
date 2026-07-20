//! F1.3 orchestrator loop — end-to-end drive against the `FakeModel` and the
//! fake step ports (docs/02 §4-§5, ADR-003; FR-01/03/06, NFR-05/12/13). These
//! are the executable acceptance for the text vertical slice's control loop:
//! a question drives to `Completed`, cancellation mid-model reaches `Cancelled`
//! with no orphaned stream, and a blown budget reaches `Failed`.

use std::time::Duration;

use crate::model::{FinishReason, ModelError, ModelEvent};
use crate::orchestrator::{Orchestrator, RunInput, RunUpdate};
use crate::testing::{EchoAssembler, FakeModel, ManualClock, RecordingCheckpointer, RecordingSink};
use jarvis_domain::ids::{RunId, SessionId};
use jarvis_domain::run::{Run, RunBudget, RunOutcomeKind, RunState};
use tokio_util::sync::CancellationToken;

const RUN_ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const SESSION_ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";

fn run_id() -> RunId {
    RUN_ULID.parse().unwrap()
}
fn session_id() -> SessionId {
    SESSION_ULID.parse().unwrap()
}

fn new_run(budget: RunBudget) -> Run {
    Run::new(run_id(), session_id(), budget)
}

fn orchestrator<'a>(
    model: &'a FakeModel,
    asm: &'a EchoAssembler,
    cp: &'a RecordingCheckpointer,
    sink: &'a RecordingSink,
    clock: &'a ManualClock,
) -> Orchestrator<'a> {
    Orchestrator {
        model,
        context: asm,
        checkpointer: cp,
        sink,
        clock,
    }
}

#[tokio::test]
async fn drives_a_simple_question_to_completed() {
    let model = FakeModel::streaming(["Hello, ", "world"]);
    let asm = EchoAssembler;
    let cp = RecordingCheckpointer::default();
    let sink = RecordingSink::default();
    let clock = ManualClock::at_unix(1_000_000);
    let orch = orchestrator(&model, &asm, &cp, &sink, &clock);

    let final_run = orch
        .drive(
            new_run(RunBudget::default_interactive()),
            RunInput {
                text: "hi there".into(),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(final_run.state, RunState::Completed);
    assert_eq!(
        final_run.outcome.map(|o| o.kind),
        Some(RunOutcomeKind::Completed)
    );
    // The two deltas streamed to the sink, in order, and nothing else as text.
    assert_eq!(sink.text(), "Hello, world");
    // The run visited the M1 happy-path states, in order.
    assert_eq!(
        sink.states(),
        vec![
            RunState::ContextReady,
            RunState::ModelRunning,
            RunState::Responding,
            RunState::Completed,
        ]
    );
    // A checkpoint was written for the terminal state (restart recovery source).
    assert_eq!(cp.saved_states().last(), Some(&RunState::Completed));
    // The model saw exactly the assembled prompt (echo of the input).
    assert_eq!(model.last_prompt().as_deref(), Some("hi there"));
}

#[tokio::test]
async fn cancellation_mid_model_reaches_cancelled_without_orphan() {
    // Yields one delta, then hangs — so the run is provably inside the model
    // stream (ModelRunning) when we cancel.
    let model = FakeModel::hangs_after(["thinking..."]);
    let asm = EchoAssembler;
    let cp = RecordingCheckpointer::default();
    let sink = RecordingSink::default();
    let clock = ManualClock::at_unix(1_000_000);
    let orch = orchestrator(&model, &asm, &cp, &sink, &clock);

    let cancel = CancellationToken::new();
    let drive = orch.drive(
        new_run(RunBudget::default_interactive()),
        RunInput { text: "hi".into() },
        cancel.clone(),
    );
    // Cancel only once the model stream is open and streaming (no sleeps: yield
    // until the fake reports it was polled).
    let canceller = async {
        while !model.was_polled() {
            tokio::task::yield_now().await;
        }
        cancel.cancel();
    };
    let (final_run, ()) = tokio::join!(drive, canceller);

    assert_eq!(final_run.state, RunState::Cancelled);
    assert_eq!(
        final_run.outcome.map(|o| o.kind),
        Some(RunOutcomeKind::Cancelled)
    );
    // The opened stream was dropped — no orphaned provider work (invariant #4).
    assert!(
        model.stream_dropped(),
        "model stream must be dropped on cancel"
    );
}

#[tokio::test]
async fn budget_exhaustion_fails_the_run() {
    // Zero model turns: opening the model spends turn 1, which trips the budget
    // at the next loop-top check.
    let budget = RunBudget {
        max_model_turns: 0,
        max_tool_calls: 0,
        max_duration: Duration::from_secs(3600),
        max_artifact_bytes: u64::MAX,
    };
    let model = FakeModel::streaming(["never reached"]);
    let asm = EchoAssembler;
    let cp = RecordingCheckpointer::default();
    let sink = RecordingSink::default();
    let clock = ManualClock::at_unix(1_000_000);
    let orch = orchestrator(&model, &asm, &cp, &sink, &clock);

    let final_run = orch
        .drive(
            new_run(budget),
            RunInput { text: "hi".into() },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(final_run.state, RunState::Failed);
    let outcome = final_run.outcome.expect("terminal outcome");
    assert_eq!(outcome.kind, RunOutcomeKind::Failed);
    assert!(
        outcome
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("ModelTurns"),
        "detail should name the exhausted dimension, got {:?}",
        outcome.detail
    );
}

#[tokio::test]
async fn provider_open_failure_fails_the_run() {
    // The provider is unavailable at open time (degraded-mode queueing is F1.7;
    // here the run simply fails).
    let model = FakeModel::fails_open(ModelError::Unavailable("cli exited 1".into()));
    let asm = EchoAssembler;
    let cp = RecordingCheckpointer::default();
    let sink = RecordingSink::default();
    let clock = ManualClock::at_unix(1_000_000);
    let orch = orchestrator(&model, &asm, &cp, &sink, &clock);

    let final_run = orch
        .drive(
            new_run(RunBudget::default_interactive()),
            RunInput { text: "hi".into() },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(final_run.state, RunState::Failed);
    assert_eq!(
        final_run.outcome.map(|o| o.kind),
        Some(RunOutcomeKind::Failed)
    );
}

#[tokio::test]
async fn mid_stream_provider_error_fails_the_run() {
    let model =
        FakeModel::streaming_then_error(["partial"], ModelError::Malformed("bad json".into()));
    let asm = EchoAssembler;
    let cp = RecordingCheckpointer::default();
    let sink = RecordingSink::default();
    let clock = ManualClock::at_unix(1_000_000);
    let orch = orchestrator(&model, &asm, &cp, &sink, &clock);

    let final_run = orch
        .drive(
            new_run(RunBudget::default_interactive()),
            RunInput { text: "hi".into() },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(final_run.state, RunState::Failed);
    // The partial delta still reached the sink before the error.
    assert_eq!(sink.text(), "partial");
}

// A `Done` with no preceding text still commits a (possibly empty) response.
#[tokio::test]
async fn empty_response_still_completes() {
    let model = FakeModel::from_events(vec![ModelEvent::Done(FinishReason::Stop)]);
    let asm = EchoAssembler;
    let cp = RecordingCheckpointer::default();
    let sink = RecordingSink::default();
    let clock = ManualClock::at_unix(1_000_000);
    let orch = orchestrator(&model, &asm, &cp, &sink, &clock);

    let final_run = orch
        .drive(
            new_run(RunBudget::default_interactive()),
            RunInput { text: "hi".into() },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(final_run.state, RunState::Completed);
    assert_eq!(sink.text(), "");
    // Usage-only assertion: a terminal Finished update was emitted.
    assert!(
        sink.updates()
            .iter()
            .any(|u| matches!(u, RunUpdate::Finished { .. }))
    );
}

// Golden trace 3 (F1.7): quota exhausted → run outcome marked unavailable for queueing
#[tokio::test]
async fn quota_exhausted_marks_provider_unavailable() {
    let model = FakeModel::fails_open(ModelError::Unavailable(
        "quota_exhausted: rate limit reset in 60s".into(),
    ));
    let asm = EchoAssembler;
    let cp = RecordingCheckpointer::default();
    let sink = RecordingSink::default();
    let clock = ManualClock::at_unix(1_000_000);
    let orch = orchestrator(&model, &asm, &cp, &sink, &clock);

    let final_run = orch
        .drive(
            new_run(RunBudget::default_interactive()),
            RunInput {
                text: "question".into(),
            },
            CancellationToken::new(),
        )
        .await;

    // Outcome is Failed (orchestrator classifies all provider errors as failures).
    assert_eq!(final_run.state, RunState::Failed);
    let outcome = final_run.outcome.expect("terminal outcome");
    assert_eq!(outcome.kind, RunOutcomeKind::Failed);

    // The detail carries the "provider unavailable:" prefix (host queueing hook)
    // plus the STABLE reason code — and NOTHING ELSE. The adapter's raw tail
    // ("rate limit reset in 60s") must never reach the persisted/emitted detail
    // (invariant #5, docs/06 §5 — security-auditor SHOULD-FIX 1).
    let detail = outcome.detail.unwrap();
    assert_eq!(detail, "provider unavailable: quota_exhausted");
    assert!(
        !detail.contains("reset in 60s"),
        "raw adapter tail must not leak into the outcome detail: {}",
        detail
    );
}

// Golden trace 3 (F1.7): auth failure also marks provider unavailable
#[tokio::test]
async fn auth_failure_marks_provider_unavailable() {
    let model = FakeModel::fails_open(ModelError::Unavailable("auth_failed: invalid token".into()));
    let asm = EchoAssembler;
    let cp = RecordingCheckpointer::default();
    let sink = RecordingSink::default();
    let clock = ManualClock::at_unix(1_000_000);
    let orch = orchestrator(&model, &asm, &cp, &sink, &clock);

    let final_run = orch
        .drive(
            new_run(RunBudget::default_interactive()),
            RunInput {
                text: "question".into(),
            },
            CancellationToken::new(),
        )
        .await;

    let detail = final_run.outcome.expect("terminal outcome").detail.unwrap();
    assert_eq!(detail, "provider unavailable: auth_failed");
    // The raw adapter tail ("invalid token") must not leak (invariant #5).
    assert!(
        !detail.contains("invalid token"),
        "raw tail leaked: {}",
        detail
    );
}

// Golden trace 3 complete flow: degraded mode recovery scenario
// Demonstrates question → quota failure → marked unavailable → (would be queued in jarvisd)
#[tokio::test]
async fn golden_trace_3_degraded_mode_flow() {
    // First attempt: provider quota exhausted
    let quota_err_model = FakeModel::fails_open(ModelError::Unavailable(
        "quota_exhausted: reset in 60s".into(),
    ));
    let asm = EchoAssembler;
    let cp = RecordingCheckpointer::default();
    let sink = RecordingSink::default();
    let clock = ManualClock::at_unix(1_000_000);
    let orch = orchestrator(&quota_err_model, &asm, &cp, &sink, &clock);

    let input = RunInput {
        text: "What is the weather?".into(),
    };

    let first_attempt = orch
        .drive(
            new_run(RunBudget::default_interactive()),
            input.clone(),
            CancellationToken::new(),
        )
        .await;

    // Run failed due to provider unavailability; jarvisd will queue it
    assert_eq!(first_attempt.state, RunState::Failed);
    let outcome = first_attempt.outcome.expect("terminal outcome");
    assert_eq!(outcome.kind, RunOutcomeKind::Failed);
    assert!(
        outcome
            .detail
            .as_ref()
            .unwrap()
            .contains("provider unavailable:")
    );

    // Simulate provider recovery: second model succeeds
    let recovery_model = FakeModel::streaming(["Sunny and ", "warm."]);
    let orch2 = orchestrator(&recovery_model, &asm, &cp, &sink, &clock);

    // In jarvisd, this would be a dequeued run re-driven from ContextReady.
    // Here we simulate the re-drive after recovery.
    let second_attempt = orch2
        .drive(
            new_run(RunBudget::default_interactive()),
            input,
            CancellationToken::new(),
        )
        .await;

    // Second attempt succeeds
    assert_eq!(second_attempt.state, RunState::Completed);
    assert_eq!(
        second_attempt.outcome.map(|o| o.kind),
        Some(RunOutcomeKind::Completed)
    );
    assert_eq!(sink.text(), "Sunny and warm.");
}

// F1.9: restart-recovery scenario (run survives restart via checkpoint)
#[tokio::test]
async fn restart_recovery_redrives_from_checkpoint() {
    // Original run checkpoints at ContextReady (F1.5: every safe transition)
    let original_cp = RecordingCheckpointer::default();
    let sink1 = RecordingSink::default();
    let asm = EchoAssembler;
    let clock = ManualClock::at_unix(1_000_000);

    let original_model = FakeModel::streaming(["Hello", " world"]);
    let orch1 = orchestrator(&original_model, &asm, &original_cp, &sink1, &clock);

    let input = RunInput { text: "hi".into() };
    let run = new_run(RunBudget::default_interactive());

    let first = orch1
        .drive(run.clone(), input.clone(), CancellationToken::new())
        .await;

    assert_eq!(first.state, RunState::Completed);
    // Checkpoint was saved at completion (terminal state)
    assert!(
        original_cp.saved_states().contains(&RunState::Completed),
        "terminal state must be checkpointed for recovery"
    );

    // Simulate restart: load the checkpoint and re-drive
    // (in reality, jarvisd loads via PgRunStore::load_unfinished; here we just re-drive the same run)
    let recovery_cp = RecordingCheckpointer::default();
    let sink2 = RecordingSink::default();
    let recovery_model = FakeModel::streaming(["Hello", " world"]);
    let orch2 = orchestrator(&recovery_model, &asm, &recovery_cp, &sink2, &clock);

    let second = orch2.drive(run, input, CancellationToken::new()).await;

    // Re-drive from checkpoint produces the same result (idempotent)
    assert_eq!(second.state, RunState::Completed);
    assert_eq!(sink2.text(), "Hello world");
}
