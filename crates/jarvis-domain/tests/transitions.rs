//! F1.2 transition table — the executable spec for the run state machine
//! (docs/02 §4, ADR-003; FR-01/06/07). Every (RunState, RunEvent) pair is
//! enumerated and asserted against an expectation encoded here independently of
//! the production `next` fn, so the test is a spec and not a mirror.
//!
//! No `_` wildcard arms over RunState/RunEvent anywhere in this file: adding a
//! variant must fail to compile here so the table is revisited by hand. This
//! mirrors invariant #2 — the state machine owns the loop, and every transition
//! is an explicit, reviewed decision.

use jarvis_domain::run::{RunEvent, RunState, TransitionError, next};

/// Every state, listed by hand (no strum) so a new variant forces an edit here.
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

/// Every event, listed by hand for the same reason.
fn all_events() -> [RunEvent; 12] {
    use RunEvent::*;
    [
        ContextAssembled,
        ModelInvoked,
        ProposalReceived,
        FinalResponseReceived,
        ApprovalRequested,
        AutoAuthorized,
        ApprovalGranted,
        ApprovalDenied,
        ToolObserved,
        ResponseCommitted,
        Cancelled,
        Failed,
    ]
}

/// The legal progression edges as a flat allow-list of `(from, event, to)`
/// triples — deliberately a *different representation* from the production fn's
/// nested `match`, so a transcription slip in one cannot hide behind the same
/// slip in the other (the two agree only if both are right). The universal
/// `Cancelled`/`Failed` edges and terminal rejection are derived in [`expected`]
/// rather than listed here.
fn legal_progressions() -> [(RunState, RunEvent, RunState); 11] {
    use RunEvent::*;
    use RunState::*;
    [
        (Received, ContextAssembled, ContextReady),
        (ContextReady, ModelInvoked, ModelRunning),
        (ModelRunning, ProposalReceived, PolicyReview),
        (ModelRunning, FinalResponseReceived, Responding),
        (PolicyReview, ApprovalRequested, WaitingApproval),
        (PolicyReview, AutoAuthorized, ToolRunning),
        (WaitingApproval, ApprovalGranted, ToolRunning),
        (WaitingApproval, ApprovalDenied, Replanning),
        (ToolRunning, ToolObserved, Replanning),
        (Replanning, ModelInvoked, ModelRunning),
        (Responding, ResponseCommitted, Completed),
    ]
}

/// The oracle: derive the expected transition from the flat allow-list above.
/// Terminal states reject everything; `Cancelled`/`Failed` are universal from
/// any non-terminal state; a listed edge is `Ok(to)`; anything else is illegal.
fn expected(state: RunState, event: RunEvent) -> Result<RunState, TransitionError> {
    use RunEvent as E;

    if state.is_terminal() {
        return Err(TransitionError::AlreadyTerminal { state });
    }
    // Universal events. Enumerated (no `_`) so a new event forces a decision.
    match event {
        E::Cancelled => return Ok(RunState::Cancelled),
        E::Failed => return Ok(RunState::Failed),
        E::ContextAssembled
        | E::ModelInvoked
        | E::ProposalReceived
        | E::FinalResponseReceived
        | E::ApprovalRequested
        | E::AutoAuthorized
        | E::ApprovalGranted
        | E::ApprovalDenied
        | E::ToolObserved
        | E::ResponseCommitted => {}
    }
    match legal_progressions()
        .into_iter()
        .find(|&(from, ev, _)| from == state && ev == event)
    {
        Some((_, _, to)) => Ok(to),
        None => Err(TransitionError::Illegal { from: state, event }),
    }
}

#[test]
fn full_transition_table_matches_the_spec() {
    let mut checked = 0;
    for state in all_states() {
        for event in all_events() {
            assert_eq!(
                next(state, event),
                expected(state, event),
                "next({state:?}, {event:?}) disagreed with the spec table"
            );
            checked += 1;
        }
    }
    assert_eq!(
        checked,
        11 * 12,
        "the cartesian product must be fully covered"
    );
}

#[test]
fn happy_path_no_tools() {
    // Received -> ContextReady -> ModelRunning -> Responding -> Completed.
    let s = next(RunState::Received, RunEvent::ContextAssembled).unwrap();
    assert_eq!(s, RunState::ContextReady);
    let s = next(s, RunEvent::ModelInvoked).unwrap();
    assert_eq!(s, RunState::ModelRunning);
    let s = next(s, RunEvent::FinalResponseReceived).unwrap();
    assert_eq!(s, RunState::Responding);
    let s = next(s, RunEvent::ResponseCommitted).unwrap();
    assert_eq!(s, RunState::Completed);
    assert!(s.is_terminal());
}

#[test]
fn tool_loop_auto_authorized() {
    // ModelRunning -> PolicyReview -> ToolRunning -> Replanning -> ModelRunning.
    let s = next(RunState::ModelRunning, RunEvent::ProposalReceived).unwrap();
    assert_eq!(s, RunState::PolicyReview);
    let s = next(s, RunEvent::AutoAuthorized).unwrap();
    assert_eq!(s, RunState::ToolRunning);
    let s = next(s, RunEvent::ToolObserved).unwrap();
    assert_eq!(s, RunState::Replanning);
    let s = next(s, RunEvent::ModelInvoked).unwrap();
    assert_eq!(s, RunState::ModelRunning);
}

#[test]
fn approval_branch_grant_and_deny() {
    let waiting = next(RunState::PolicyReview, RunEvent::ApprovalRequested).unwrap();
    assert_eq!(waiting, RunState::WaitingApproval);
    assert_eq!(
        next(waiting, RunEvent::ApprovalGranted).unwrap(),
        RunState::ToolRunning
    );
    // A denial is fed back to the model as an observation, not a run failure.
    assert_eq!(
        next(waiting, RunEvent::ApprovalDenied).unwrap(),
        RunState::Replanning
    );
}

#[test]
fn cancellation_reaches_cancelled_from_every_nonterminal_state() {
    for state in all_states() {
        if state.is_terminal() {
            continue;
        }
        assert_eq!(
            next(state, RunEvent::Cancelled),
            Ok(RunState::Cancelled),
            "cancel from {state:?}"
        );
    }
}

#[test]
fn failure_reaches_failed_from_every_nonterminal_state() {
    for state in all_states() {
        if state.is_terminal() {
            continue;
        }
        assert_eq!(
            next(state, RunEvent::Failed),
            Ok(RunState::Failed),
            "fail from {state:?}"
        );
    }
}

#[test]
fn terminal_states_reject_every_event() {
    for state in all_states() {
        if !state.is_terminal() {
            continue;
        }
        for event in all_events() {
            assert_eq!(
                next(state, event),
                Err(TransitionError::AlreadyTerminal { state }),
                "{state:?} must reject {event:?}"
            );
        }
    }
}

#[test]
fn model_output_cannot_skip_policy_review() {
    // Invariant #1 guard at the state level: nothing the model emits can jump
    // straight from reasoning to tool execution — PolicyReview is unavoidable.
    assert_eq!(
        next(RunState::ModelRunning, RunEvent::AutoAuthorized),
        Err(TransitionError::Illegal {
            from: RunState::ModelRunning,
            event: RunEvent::AutoAuthorized,
        })
    );
    assert_eq!(
        next(RunState::ModelRunning, RunEvent::ApprovalGranted),
        Err(TransitionError::Illegal {
            from: RunState::ModelRunning,
            event: RunEvent::ApprovalGranted,
        })
    );
}

#[test]
fn representative_illegal_transitions_carry_from_and_event() {
    assert_eq!(
        next(RunState::Received, RunEvent::ModelInvoked),
        Err(TransitionError::Illegal {
            from: RunState::Received,
            event: RunEvent::ModelInvoked,
        })
    );
    assert_eq!(
        next(RunState::PolicyReview, RunEvent::ToolObserved),
        Err(TransitionError::Illegal {
            from: RunState::PolicyReview,
            event: RunEvent::ToolObserved,
        })
    );
    assert_eq!(
        next(RunState::ContextReady, RunEvent::ResponseCommitted),
        Err(TransitionError::Illegal {
            from: RunState::ContextReady,
            event: RunEvent::ResponseCommitted,
        })
    );
}

#[test]
fn is_terminal_is_exactly_completed_failed_cancelled() {
    for state in all_states() {
        let want = matches!(
            state,
            RunState::Completed | RunState::Failed | RunState::Cancelled
        );
        assert_eq!(state.is_terminal(), want, "{state:?}");
    }
}
