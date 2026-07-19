//! Run lifecycle: the `RunState` machine and per-run budgets (docs/02 §4,
//! ADR-003; FR-01/06/07, NFR-12). Pure domain logic — no I/O, no clock reads.
//!
//! The orchestrator (`jarvis-application`, F1.3) is the only caller of [`next`]:
//! it derives a [`RunEvent`] from a step outcome and asks this module for the
//! successor state. Models *propose* (text, tool calls); they never drive a
//! transition directly. In particular a model proposal can only ever produce
//! [`RunEvent::ProposalReceived`], which routes through [`RunState::PolicyReview`]
//! — there is no state edge from model output to tool execution (invariant #1).
//!
//! The FR-12 "queued / visible waiting" signal is deliberately **not** a state
//! here: queuing is an application concern (single-flight FIFO + provider
//! health). A queued run rests at its [`RunState::ContextReady`] checkpoint and
//! resumes idempotently on provider recovery; the wire carries the waiting
//! signal as `DomainEvent::RunQueued` (see `jarvis-contracts::runs`).

use std::time::Duration;
use thiserror::Error;

/// The lifecycle states of a run (docs/02 §4). The policy/tool/approval states
/// exist from M1 but their step executors are wired in M2 — the graph is whole
/// now so the transition function never has to change to add behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Input persisted, identity + idempotency validated.
    Received,
    /// Bounded context assembled within budget.
    ContextReady,
    /// Provider stream active.
    ModelRunning,
    /// Proposal validated, risk classified (docs/02 §4).
    PolicyReview,
    /// An exact approval card is published with an expiry.
    WaitingApproval,
    /// One bounded tool call is executing.
    ToolRunning,
    /// Observations returned to the model; budget decremented.
    Replanning,
    /// Final output streaming.
    Responding,
    /// Terminal: completed successfully.
    Completed,
    /// Terminal: failed (error or budget exhaustion).
    Failed,
    /// Terminal: cancelled by the user.
    Cancelled,
}

impl RunState {
    /// Terminal states are absorbing: the loop stops and [`next`] rejects any
    /// further event. Commit logic guards on the recorded outcome for
    /// idempotency (docs/02 §4).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// The events that advance a run. Each is an *observation* the orchestrator
/// makes about a completed step; the model never constructs one directly.
///
/// [`Self::Cancelled`] and [`Self::Failed`] are universal — legal from every
/// non-terminal state. All others are progression events valid only in specific
/// states (see [`next`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEvent {
    /// Context assembly finished. `Received -> ContextReady`.
    ContextAssembled,
    /// The model step started a provider stream.
    /// `ContextReady | Replanning -> ModelRunning`.
    ModelInvoked,
    /// The model proposed a tool call. `ModelRunning -> PolicyReview`.
    ProposalReceived,
    /// The model produced final text with no tool call.
    /// `ModelRunning -> Responding`.
    FinalResponseReceived,
    /// Policy requires explicit approval. `PolicyReview -> WaitingApproval`.
    ApprovalRequested,
    /// Policy auto-authorised an R0/R1 action. `PolicyReview -> ToolRunning`.
    AutoAuthorized,
    /// The user approved the exact proposed action.
    /// `WaitingApproval -> ToolRunning`.
    ApprovalGranted,
    /// The user declined; the denial is fed back as an observation.
    /// `WaitingApproval -> Replanning`.
    ApprovalDenied,
    /// A tool call finished and returned observations.
    /// `ToolRunning -> Replanning`.
    ToolObserved,
    /// The final response was committed. `Responding -> Completed`.
    ResponseCommitted,
    /// The user cancelled. Any non-terminal state `-> Cancelled`.
    Cancelled,
    /// The run failed (error or budget exhaustion). Any non-terminal state
    /// `-> Failed`.
    Failed,
}

/// Why a transition was rejected. Both variants are programmer/logic errors —
/// the orchestrator only feeds events it derived itself — surfaced (never
/// panicked) so a misuse becomes a `Failed` run with a generic detail, not a
/// crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TransitionError {
    /// `event` is not a legal progression from the non-terminal `from` state.
    #[error("illegal transition: {event:?} is not valid in state {from:?}")]
    Illegal { from: RunState, event: RunEvent },
    /// An event was applied to an already-terminal run. Idempotent re-cancels
    /// are absorbed at the API layer, not here.
    #[error("run is already terminal in state {state:?}")]
    AlreadyTerminal { state: RunState },
}

/// The transition function (docs/02 §4). Pure and total over `(RunState,
/// RunEvent)`. Exhaustive with no `_` arm: adding a state or event variant
/// fails to compile until every case is decided (invariant #2).
pub fn next(state: RunState, event: RunEvent) -> Result<RunState, TransitionError> {
    use RunEvent as E;
    use RunState as S;

    // The full table, one arm per state. `Cancelled`/`Failed` are the universal
    // events — legal from every non-terminal state — and are spelled out in each
    // arm rather than special-cased, so every arm is the complete, self-contained
    // truth for its state. Illegal progressions are grouped into the final `|`
    // pattern of each arm. No `_` anywhere: a new state or event fails to compile
    // until its transition is decided (invariant #2).
    let illegal = || TransitionError::Illegal { from: state, event };
    match state {
        S::Received => match event {
            E::ContextAssembled => Ok(S::ContextReady),
            E::Cancelled => Ok(S::Cancelled),
            E::Failed => Ok(S::Failed),
            E::ModelInvoked
            | E::ProposalReceived
            | E::FinalResponseReceived
            | E::ApprovalRequested
            | E::AutoAuthorized
            | E::ApprovalGranted
            | E::ApprovalDenied
            | E::ToolObserved
            | E::ResponseCommitted => Err(illegal()),
        },
        S::ContextReady => match event {
            E::ModelInvoked => Ok(S::ModelRunning),
            E::Cancelled => Ok(S::Cancelled),
            E::Failed => Ok(S::Failed),
            E::ContextAssembled
            | E::ProposalReceived
            | E::FinalResponseReceived
            | E::ApprovalRequested
            | E::AutoAuthorized
            | E::ApprovalGranted
            | E::ApprovalDenied
            | E::ToolObserved
            | E::ResponseCommitted => Err(illegal()),
        },
        S::ModelRunning => match event {
            E::ProposalReceived => Ok(S::PolicyReview),
            E::FinalResponseReceived => Ok(S::Responding),
            E::Cancelled => Ok(S::Cancelled),
            E::Failed => Ok(S::Failed),
            E::ContextAssembled
            | E::ModelInvoked
            | E::ApprovalRequested
            | E::AutoAuthorized
            | E::ApprovalGranted
            | E::ApprovalDenied
            | E::ToolObserved
            | E::ResponseCommitted => Err(illegal()),
        },
        S::PolicyReview => match event {
            E::ApprovalRequested => Ok(S::WaitingApproval),
            E::AutoAuthorized => Ok(S::ToolRunning),
            E::Cancelled => Ok(S::Cancelled),
            E::Failed => Ok(S::Failed),
            E::ContextAssembled
            | E::ModelInvoked
            | E::ProposalReceived
            | E::FinalResponseReceived
            | E::ApprovalGranted
            | E::ApprovalDenied
            | E::ToolObserved
            | E::ResponseCommitted => Err(illegal()),
        },
        S::WaitingApproval => match event {
            E::ApprovalGranted => Ok(S::ToolRunning),
            E::ApprovalDenied => Ok(S::Replanning),
            E::Cancelled => Ok(S::Cancelled),
            E::Failed => Ok(S::Failed),
            E::ContextAssembled
            | E::ModelInvoked
            | E::ProposalReceived
            | E::FinalResponseReceived
            | E::ApprovalRequested
            | E::AutoAuthorized
            | E::ToolObserved
            | E::ResponseCommitted => Err(illegal()),
        },
        S::ToolRunning => match event {
            E::ToolObserved => Ok(S::Replanning),
            E::Cancelled => Ok(S::Cancelled),
            E::Failed => Ok(S::Failed),
            E::ContextAssembled
            | E::ModelInvoked
            | E::ProposalReceived
            | E::FinalResponseReceived
            | E::ApprovalRequested
            | E::AutoAuthorized
            | E::ApprovalGranted
            | E::ApprovalDenied
            | E::ResponseCommitted => Err(illegal()),
        },
        S::Replanning => match event {
            E::ModelInvoked => Ok(S::ModelRunning),
            E::Cancelled => Ok(S::Cancelled),
            E::Failed => Ok(S::Failed),
            E::ContextAssembled
            | E::ProposalReceived
            | E::FinalResponseReceived
            | E::ApprovalRequested
            | E::AutoAuthorized
            | E::ApprovalGranted
            | E::ApprovalDenied
            | E::ToolObserved
            | E::ResponseCommitted => Err(illegal()),
        },
        S::Responding => match event {
            E::ResponseCommitted => Ok(S::Completed),
            E::Cancelled => Ok(S::Cancelled),
            E::Failed => Ok(S::Failed),
            E::ContextAssembled
            | E::ModelInvoked
            | E::ProposalReceived
            | E::FinalResponseReceived
            | E::ApprovalRequested
            | E::AutoAuthorized
            | E::ApprovalGranted
            | E::ApprovalDenied
            | E::ToolObserved => Err(illegal()),
        },
        // Terminal states are absorbing: every event is rejected. Named
        // explicitly (no `_`) so a new terminal state is a compile error here.
        S::Completed | S::Failed | S::Cancelled => Err(TransitionError::AlreadyTerminal { state }),
    }
}

/// Per-run resource caps (docs/05 §4, NFR-12). Checked at the top of the
/// orchestrator loop, never inside a step (state-machine skill rule 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunBudget {
    pub max_model_turns: u8,
    pub max_tool_calls: u16,
    pub max_duration: Duration,
    pub max_artifact_bytes: u64,
}

impl RunBudget {
    /// The default budget for an interactive run. Conservative caps that keep a
    /// single request bounded in quota and wall-clock; background/automation
    /// budgets are set explicitly where they are created.
    pub fn default_interactive() -> Self {
        Self {
            max_model_turns: 8,
            max_tool_calls: 16,
            max_duration: Duration::from_secs(120),
            max_artifact_bytes: 8 * 1024 * 1024,
        }
    }

    /// The first budget dimension `usage` has exceeded, or `None` if the run is
    /// within budget. "Exceeded" is strictly greater than the cap: usage exactly
    /// at a cap is still within budget. Dimensions are checked in the order of
    /// [`BudgetDimension`] and the first tripped one is returned.
    pub fn exceeded(&self, usage: &RunUsage) -> Option<BudgetDimension> {
        if usage.model_turns > self.max_model_turns {
            Some(BudgetDimension::ModelTurns)
        } else if usage.tool_calls > self.max_tool_calls {
            Some(BudgetDimension::ToolCalls)
        } else if usage.elapsed > self.max_duration {
            Some(BudgetDimension::Duration)
        } else if usage.artifact_bytes > self.max_artifact_bytes {
            Some(BudgetDimension::ArtifactBytes)
        } else {
            None
        }
    }
}

/// Consumption accrued by a run so far. Time is passed in — the domain never
/// reads a clock (the orchestrator computes `elapsed` from the run start).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunUsage {
    pub model_turns: u8,
    pub tool_calls: u16,
    pub elapsed: Duration,
    pub artifact_bytes: u64,
}

/// Which budget dimension a run exceeded (checked in this declaration order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetDimension {
    ModelTurns,
    ToolCalls,
    Duration,
    ArtifactBytes,
}
