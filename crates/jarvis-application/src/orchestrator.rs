//! The run orchestrator (docs/02 §4-§5, ADR-003). The loop is code, not model:
//! models propose, the state machine decides. Budgets are checked at the top of
//! the loop, cancellation before and *during* the model stream, and a checkpoint
//! is written at every safe transition (NFR-05). Every failure path lands the
//! run in a terminal `Failed`/`Cancelled` — the loop never panics or hangs.
//!
//! M1 wires the text path only: `Received -> ContextReady -> ModelRunning ->
//! Responding -> Completed`. The policy/tool/approval states exist in the type
//! (F1.2) but their executors are unwired here — reaching one fails the run,
//! so there is no path from model output to tool execution yet (invariant #1).
//!
//! Observability: this layer stays pure (no `tracing` dependency — arch-test
//! keeps the runtime out of the application crate). It emits structured
//! [`RunUpdate`]s; the host (jarvisd, F1.5) opens the spans and maps updates to
//! wire events (docs/05 §3) and the outbox.

use std::time::SystemTime;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::model::{ModelError, ModelEvent, ModelProvider, ModelRequest};
use jarvis_domain::ids::RunId;
use jarvis_domain::run::{Run, RunEvent, RunOutcome, RunState, TransitionError};

/// The user input that starts a run. M1 is text-only; richer inputs (voice,
/// referenced artifacts) extend this additively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInput {
    pub text: String,
}

/// The bounded, inspectable context assembled for a model turn (docs/02 §5
/// step 3). M1 carries just the prompt; provenance/token-budget metadata land
/// with memory/retrieval in M4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledContext {
    pub prompt: String,
}

/// A structured update the orchestrator emits as a run progresses. The host
/// maps these onto the wire `DomainEvent`/`TransientEvent` split (docs/05 §3):
/// [`Self::TextDelta`] is transient (never replayed); the others are persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunUpdate {
    /// The run entered a new lifecycle state.
    StateChanged { run_id: RunId, state: RunState },
    /// One incremental chunk of streamed model output (transient).
    TextDelta { run_id: RunId, text: String },
    /// The run reached a terminal state (completed, failed, or cancelled);
    /// carries the outcome.
    Finished { run_id: RunId, outcome: RunOutcome },
}

/// Context assembly (docs/02 §5 step 3). Implemented in infra/host; the fake in
/// [`crate::testing`] echoes the input. Takes the [`CancellationToken`] because
/// assembly can be slow in M4 (retrieval/embeddings) and must abort promptly
/// (invariant #4, state-machine skill rule 5).
#[async_trait]
pub trait ContextAssembler: Send + Sync {
    async fn assemble(
        &self,
        run: &Run,
        input: &RunInput,
        cancel: &CancellationToken,
    ) -> Result<AssembledContext, ContextError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("context assembly failed: {0}")]
pub struct ContextError(pub String);

/// Durable run checkpoints at safe transitions (NFR-05). On restart the run
/// reloads from the last checkpoint and reconciles by idempotency rather than
/// re-executing blindly (recovery wiring is F1.4).
///
/// Intentionally **not** cancellable: a checkpoint is the short durable write
/// that makes a boundary recoverable, and it must complete even while the run is
/// being cancelled (so the terminal `Cancelled` state itself is persisted).
#[async_trait]
pub trait Checkpointer: Send + Sync {
    async fn save(&self, run: &Run) -> Result<(), CheckpointError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("checkpoint save failed: {0}")]
pub struct CheckpointError(pub String);

/// Sink for run updates (docs/05 §3). Best-effort broadcast; the durable record
/// is the checkpoint + (F1.4) the transactional outbox.
#[async_trait]
pub trait RunEventSink: Send + Sync {
    async fn emit(&self, update: RunUpdate);
}

/// Injected time source — the application never reads the system clock directly
/// (kept testable and deterministic; the domain is clock-free entirely).
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

/// The orchestrator: borrows the ports it drives for the duration of one run.
pub struct Orchestrator<'a> {
    pub model: &'a dyn ModelProvider,
    pub context: &'a dyn ContextAssembler,
    pub checkpointer: &'a dyn Checkpointer,
    pub sink: &'a dyn RunEventSink,
    pub clock: &'a dyn Clock,
}

/// Loop-local state that must persist across transitions but is not part of the
/// durable run (the assembled prompt and the live provider stream).
struct Active {
    prompt: Option<String>,
    stream: Option<BoxStream<'static, ModelEvent>>,
}

/// Outcome of a single step: keep looping, or the run was cancelled mid-step.
enum StepFlow {
    Continue,
    Cancelled,
}

/// A recoverable step failure; the orchestrator turns any of these into a
/// terminal `Failed`. The `Display`/inner text is for the host's span/log; the
/// user-facing outcome detail comes from [`StepError::user_detail`].
#[derive(Debug, thiserror::Error)]
enum StepError {
    #[error("{0}")]
    Context(#[from] ContextError),
    #[error("{0}")]
    Model(#[from] ModelError),
    #[error("model stream ended before a completion signal")]
    StreamEnded,
    #[error("state {0:?} has no executor in M1")]
    UnwiredInM1(RunState),
    #[error("invalid transition: {0}")]
    Transition(#[from] TransitionError),
}

impl StepError {
    /// A stable, non-sensitive reason for the run outcome — never the adapter's
    /// own error text, which may contain provider/driver detail (docs/06 §5,
    /// invariant #5). For ModelError, distinguish unavailable (queueable in F1.7)
    /// from malformed. Exhaustive (no `_`) so a new variant forces a decision.
    fn user_detail(&self) -> String {
        match self {
            Self::Context(_) => "context assembly failed".to_owned(),
            Self::Model(ModelError::Unavailable(msg)) => {
                // Preserve the reason prefix from adapter: "timeout:", "network_error:", etc.
                format!("provider unavailable: {}", msg)
            }
            Self::Model(ModelError::Malformed(_)) => "model stream malformed".to_owned(),
            Self::StreamEnded => "model stream ended before completion".to_owned(),
            Self::UnwiredInM1(_) => "requested step is not available".to_owned(),
            Self::Transition(_) => "internal orchestration error".to_owned(),
        }
    }
}

impl Orchestrator<'_> {
    /// Drive a run to a terminal state and return it. Always terminates: a run
    /// ends `Completed`, `Failed`, or `Cancelled` — never `Err`.
    pub async fn drive(&self, mut run: Run, input: RunInput, cancel: CancellationToken) -> Run {
        let started = self.clock.now();
        let mut active = Active {
            prompt: None,
            stream: None,
        };

        while !run.state.is_terminal() {
            // (1) Cancellation, before any work this iteration (invariant #4).
            if cancel.is_cancelled() {
                self.finish_cancelled(&mut run).await;
                break;
            }
            // (2) Budgets, at the loop top only (state-machine skill rule 4).
            run.usage.elapsed = self.clock.now().duration_since(started).unwrap_or_default();
            if let Some(dim) = run.budget.exceeded(&run.usage) {
                self.fail(&mut run, format!("budget exceeded: {dim:?}"))
                    .await;
                break;
            }
            // (3) One transition.
            let step = match run.state {
                RunState::Received => {
                    self.assemble_step(&mut run, &input, &mut active, &cancel)
                        .await
                }
                RunState::ContextReady => {
                    self.open_model_step(&mut run, &mut active, &cancel).await
                }
                RunState::ModelRunning => {
                    self.pull_model_step(&mut run, &mut active, &cancel).await
                }
                RunState::Responding => self.commit_step(&mut run).await,
                // M2 executors are not wired in M1. Named explicitly (no `_`).
                RunState::Replanning
                | RunState::PolicyReview
                | RunState::WaitingApproval
                | RunState::ToolRunning => Err(StepError::UnwiredInM1(run.state)),
                // The while-guard excludes terminals; listed for exhaustiveness.
                RunState::Completed | RunState::Failed | RunState::Cancelled => break,
            };
            match step {
                Ok(StepFlow::Continue) => {}
                Ok(StepFlow::Cancelled) => {
                    self.finish_cancelled(&mut run).await;
                    break;
                }
                Err(err) => {
                    // A generic, non-sensitive reason only — never the adapter's
                    // own error text (docs/06 §5, invariant #5). The full error
                    // is for the host's span/log (F1.5), not the user outcome.
                    self.fail(&mut run, err.user_detail()).await;
                    break;
                }
            }
        }
        // `active` (and any open stream) drops here — no orphaned provider work.
        run
    }

    async fn assemble_step(
        &self,
        run: &mut Run,
        input: &RunInput,
        active: &mut Active,
        cancel: &CancellationToken,
    ) -> Result<StepFlow, StepError> {
        let ctx = self.context.assemble(run, input, cancel).await?;
        active.prompt = Some(ctx.prompt);
        run.apply(RunEvent::ContextAssembled)?;
        self.after_transition(run).await;
        Ok(StepFlow::Continue)
    }

    async fn open_model_step(
        &self,
        run: &mut Run,
        active: &mut Active,
        cancel: &CancellationToken,
    ) -> Result<StepFlow, StepError> {
        let request = ModelRequest {
            prompt: active.prompt.take().unwrap_or_default(),
        };
        // One model turn is consumed at invocation; the next loop-top budget
        // check enforces `max_model_turns`.
        run.usage.model_turns = run.usage.model_turns.saturating_add(1);
        let stream = self.model.run(request, cancel.clone()).await?;
        active.stream = Some(stream);
        run.apply(RunEvent::ModelInvoked)?;
        self.after_transition(run).await;
        Ok(StepFlow::Continue)
    }

    async fn pull_model_step(
        &self,
        run: &mut Run,
        active: &mut Active,
        cancel: &CancellationToken,
    ) -> Result<StepFlow, StepError> {
        let stream = active.stream.as_mut().ok_or(StepError::StreamEnded)?;
        match pull_or_cancel(stream, cancel).await {
            Pulled::Cancelled => Ok(StepFlow::Cancelled),
            Pulled::Event(None) => {
                active.stream = None;
                Err(StepError::StreamEnded)
            }
            Pulled::Event(Some(event)) => match event {
                ModelEvent::TextDelta(text) => {
                    self.sink
                        .emit(RunUpdate::TextDelta {
                            run_id: run.id.clone(),
                            text,
                        })
                        .await;
                    Ok(StepFlow::Continue)
                }
                // Usage recording lands with persistence (F1.4); ignored here.
                ModelEvent::Usage(_) => Ok(StepFlow::Continue),
                ModelEvent::Done(_reason) => {
                    active.stream = None;
                    run.apply(RunEvent::FinalResponseReceived)?;
                    self.after_transition(run).await;
                    Ok(StepFlow::Continue)
                }
                ModelEvent::Error(err) => {
                    active.stream = None;
                    Err(StepError::Model(err))
                }
            },
        }
    }

    async fn commit_step(&self, run: &mut Run) -> Result<StepFlow, StepError> {
        // M1 has no external commit effect (the assistant message is persisted
        // by the host in F1.4); committing just finalizes the run.
        run.apply(RunEvent::ResponseCommitted)?;
        self.after_transition(run).await;
        Ok(StepFlow::Continue)
    }

    /// Emit the state change, checkpoint the safe boundary, and — if the run is
    /// now terminal — emit the outcome. Called after every successful `apply`.
    async fn after_transition(&self, run: &Run) {
        self.sink
            .emit(RunUpdate::StateChanged {
                run_id: run.id.clone(),
                state: run.state,
            })
            .await;
        // Checkpoint durability is hardened in F1.4; a fake never fails here.
        let _ = self.checkpointer.save(run).await;
        if let Some(outcome) = &run.outcome {
            self.sink
                .emit(RunUpdate::Finished {
                    run_id: run.id.clone(),
                    outcome: outcome.clone(),
                })
                .await;
        }
    }

    async fn fail(&self, run: &mut Run, detail: String) {
        // `Failed` is legal from every non-terminal state; if the run somehow
        // already terminalised, keep the first outcome.
        if run.apply(RunEvent::Failed).is_ok() {
            run.set_outcome_detail(detail);
            self.after_transition(run).await;
        }
    }

    async fn finish_cancelled(&self, run: &mut Run) {
        if run.apply(RunEvent::Cancelled).is_ok() {
            self.after_transition(run).await;
        }
    }
}

/// The result of racing the model stream against cancellation.
enum Pulled {
    Event(Option<ModelEvent>),
    Cancelled,
}

/// Pull the next model event, but resolve immediately to [`Pulled::Cancelled`]
/// if the token fires first — even while the stream is `Pending`. This is what
/// makes a hung/slow provider promptly cancellable (invariant #4) without a
/// `tokio::select!` (the runtime is not a dependency of this crate).
async fn pull_or_cancel(
    stream: &mut BoxStream<'static, ModelEvent>,
    cancel: &CancellationToken,
) -> Pulled {
    use std::future::poll_fn;
    use std::task::Poll;

    let mut cancelled = std::pin::pin!(cancel.cancelled());
    poll_fn(|cx| {
        if cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Pulled::Cancelled);
        }
        match stream.as_mut().poll_next(cx) {
            Poll::Ready(event) => Poll::Ready(Pulled::Event(event)),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}
