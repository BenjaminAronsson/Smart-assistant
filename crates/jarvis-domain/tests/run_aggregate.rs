//! F1.3 domain tests for the `Run` aggregate (docs/02 §4). `next` itself is
//! covered exhaustively in `transitions.rs`; here we pin the aggregate's own
//! logic: outcome recording on the first terminal transition, its idempotency,
//! and `RunOutcomeKind::from_terminal`.

use jarvis_domain::ids::{RunId, SessionId};
use jarvis_domain::run::{Run, RunBudget, RunEvent, RunOutcomeKind, RunState, TransitionError};

const RUN_ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const SESSION_ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";

fn new_run() -> Run {
    Run::new(
        RUN_ULID.parse::<RunId>().unwrap(),
        SESSION_ULID.parse::<SessionId>().unwrap(),
        RunBudget::default_interactive(),
    )
}

fn all_states() -> [RunState; 11] {
    use RunState::*;
    [
        Received,
        ContextReady,
        ModelRunning,
        PolicyReview,
        WaitingApproval,
        ToolRunning,
        Replanning,
        Responding,
        Completed,
        Failed,
        Cancelled,
    ]
}

#[test]
fn new_run_starts_received_with_zero_usage_and_no_outcome() {
    let run = new_run();
    assert_eq!(run.state, RunState::Received);
    assert_eq!(run.usage, Default::default());
    assert!(run.outcome.is_none());
}

#[test]
fn apply_advances_state_and_leaves_no_outcome_while_non_terminal() {
    let mut run = new_run();
    assert_eq!(
        run.apply(RunEvent::ContextAssembled).unwrap(),
        RunState::ContextReady
    );
    assert!(run.outcome.is_none());
    assert_eq!(
        run.apply(RunEvent::ModelInvoked).unwrap(),
        RunState::ModelRunning
    );
    assert!(run.outcome.is_none());
}

#[test]
fn reaching_a_terminal_state_records_the_matching_outcome() {
    // Drive the no-tool happy path to Completed.
    let mut run = new_run();
    run.apply(RunEvent::ContextAssembled).unwrap();
    run.apply(RunEvent::ModelInvoked).unwrap();
    run.apply(RunEvent::FinalResponseReceived).unwrap();
    run.apply(RunEvent::ResponseCommitted).unwrap();
    assert_eq!(run.state, RunState::Completed);
    let outcome = run.outcome.expect("terminal outcome recorded");
    assert_eq!(outcome.kind, RunOutcomeKind::Completed);
    assert_eq!(outcome.detail, None);
}

#[test]
fn cancel_from_non_terminal_records_cancelled_outcome() {
    let mut run = new_run();
    run.apply(RunEvent::ContextAssembled).unwrap();
    assert_eq!(run.apply(RunEvent::Cancelled).unwrap(), RunState::Cancelled);
    assert_eq!(run.outcome.map(|o| o.kind), Some(RunOutcomeKind::Cancelled));
}

#[test]
fn outcome_and_detail_are_preserved_once_terminal() {
    // First terminal transition wins; a second apply is rejected and must not
    // overwrite the recorded outcome/detail (idempotency, run.rs doc claim).
    let mut run = new_run();
    run.apply(RunEvent::Failed).unwrap();
    run.set_outcome_detail("budget exceeded: ModelTurns");
    let before = run.outcome.clone();
    assert_eq!(
        before.as_ref().map(|o| o.kind),
        Some(RunOutcomeKind::Failed)
    );

    // Any further event is rejected by `next` (terminal is absorbing) and the
    // outcome is untouched.
    assert_eq!(
        run.apply(RunEvent::Cancelled),
        Err(TransitionError::AlreadyTerminal {
            state: RunState::Failed
        })
    );
    assert_eq!(run.outcome, before);
    assert_eq!(run.state, RunState::Failed);
}

#[test]
fn set_outcome_detail_is_a_noop_before_terminal() {
    let mut run = new_run();
    run.set_outcome_detail("nothing to attach to");
    assert!(run.outcome.is_none());
}

#[test]
fn from_terminal_maps_only_terminal_states() {
    for state in all_states() {
        let expected = match state {
            RunState::Completed => Some(RunOutcomeKind::Completed),
            RunState::Failed => Some(RunOutcomeKind::Failed),
            RunState::Cancelled => Some(RunOutcomeKind::Cancelled),
            RunState::Received
            | RunState::ContextReady
            | RunState::ModelRunning
            | RunState::PolicyReview
            | RunState::WaitingApproval
            | RunState::ToolRunning
            | RunState::Replanning
            | RunState::Responding => None,
        };
        assert_eq!(RunOutcomeKind::from_terminal(state), expected, "{state:?}");
    }
}
