//! The run orchestrator (docs/02 §4-§5, ADR-003). The loop is code, not model:
//! models propose, the state machine decides. Budgets are checked at the top of
//! the loop, cancellation before and *during* the model stream, and a checkpoint
//! is written at every safe transition (NFR-05). Every failure path lands the
//! run in a terminal `Failed`/`Cancelled` — the loop never panics or hangs.
//!
//! M1 wired the text path (`Received -> ContextReady -> ModelRunning ->
//! Responding -> Completed`). F2.2 added the R0/R1 tool path (`ModelRunning ->
//! PolicyReview -> ToolRunning -> Replanning`); F2.3 adds R2 approval
//! (`PolicyReview -> WaitingApproval -> ToolRunning` on approval, or
//! `-> Replanning` on denial), minting a grant that is validated immediately
//! before execution. Every proposal passes `policy::evaluate` and every R2+
//! call presents a validated grant (invariant #1) — no edge from model output
//! to tool execution skips either gate.
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
use crate::policy::{
    self, ApprovalGate, ApprovalOutcome, ApprovalRequest, AuditSink, DenyReason, GrantBinding,
    GrantMinter, GrantValidator, PolicyContext, PolicyDecision, ToolRegistry,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::grants::{ExecutionGrant, GrantError};
use jarvis_domain::ids::RunId;
use jarvis_domain::run::{Run, RunEvent, RunOutcome, RunState, TransitionError};
use jarvis_domain::tools::{ToolError, ToolInvocation, ToolProposal, ToolResult};

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
    /// A reversible R2 tool executed and registered a compensating undo
    /// (docs/06 §4); the description is surfaced in the run timeline so the undo
    /// is discoverable. Persisted (a domain event), not transient.
    CompensationRegistered {
        run_id: RunId,
        tool_id: String,
        description: String,
    },
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

/// The tool/policy ports plus the per-run authorization context (F2.2). Bundled
/// and optional so the text-only path (M1 tests; jarvisd with the CLI adapter,
/// whose built-in tools are disabled per ADR-004) constructs an orchestrator
/// with `tools: None` and never touches the policy machinery. With `None`, a
/// model tool proposal has nowhere to go and fails the run safely — there is no
/// ambient tool set (invariant #1).
pub struct ToolStack<'a> {
    pub registry: &'a ToolRegistry,
    pub audit: &'a dyn AuditSink,
    pub context: PolicyContext,
    /// The human-approval seam for R2+ (F2.3). `WaitingApproval` blocks here.
    pub approval_gate: &'a dyn ApprovalGate,
    /// Mints a single-use grant on approval; validates it immediately before
    /// execution. Split ports so the executor-side validation is the one that
    /// consumes the grant (docs/06 §4).
    pub grant_minter: &'a dyn GrantMinter,
    pub grant_validator: &'a dyn GrantValidator,
}

/// The orchestrator: borrows the ports it drives for the duration of one run.
pub struct Orchestrator<'a> {
    pub model: &'a dyn ModelProvider,
    pub context: &'a dyn ContextAssembler,
    pub checkpointer: &'a dyn Checkpointer,
    pub sink: &'a dyn RunEventSink,
    pub clock: &'a dyn Clock,
    /// Present once tools are wired (F2.2+); `None` for the text-only path.
    pub tools: Option<ToolStack<'a>>,
}

/// Loop-local state that must persist across transitions but is not part of the
/// durable run (the assembled prompt, the live provider stream, and the tool
/// proposal/invocation/observation as a proposal moves policy → execute →
/// replan).
struct Active {
    prompt: Option<String>,
    stream: Option<BoxStream<'static, ModelEvent>>,
    /// A model tool proposal awaiting policy evaluation (`PolicyReview`).
    proposal: Option<ToolProposal>,
    /// The authorized invocation awaiting execution (`ToolRunning`).
    invocation: Option<ToolInvocation>,
    /// The grant minted at approval, to validate before execution. `None` for
    /// the R0/R1 auto path (no grant); `Some` for an approved R2+ call.
    grant: Option<ExecutionGrant>,
    /// The tool result to fold back into the next model turn (`Replanning`).
    observation: Option<ToolResult>,
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
    #[error("state {0:?} has no executor wired yet")]
    Unwired(RunState),
    #[error("policy denied the tool call: {0}")]
    PolicyDenied(DenyReason),
    #[error("grant validation failed: {0}")]
    Grant(GrantError),
    #[error("tool execution failed: {0}")]
    Tool(#[from] ToolError),
    /// A loop-invariant violation (e.g. reaching `ToolRunning` with no
    /// invocation staged). Never expected; surfaced as a generic failure.
    #[error("internal orchestration error: {0}")]
    Internal(&'static str),
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
                // Reduce to a STABLE reason code — never the adapter's raw text,
                // which may carry provider/driver/OS detail (invariant #5,
                // docs/06 §5). The host matches this same "provider unavailable:"
                // prefix for degraded-mode queueing; the code (not the raw tail)
                // is what reaches the WS/timeline/run snapshot.
                format!("provider unavailable: {}", crate::health::reason_code(msg))
            }
            Self::Model(ModelError::Malformed(_)) => "model stream malformed".to_owned(),
            Self::StreamEnded => "model stream ended before completion".to_owned(),
            Self::Unwired(_) => "requested step is not available".to_owned(),
            // Stable, non-sensitive: the denial reason code, never the rendered
            // exact-effect (which could echo tool arguments — docs/06 §5).
            Self::PolicyDenied(reason) => format!("policy denied: {}", reason.code()),
            Self::Grant(err) => format!("grant rejected: {}", err.code()),
            Self::Tool(_) => "tool execution failed".to_owned(),
            Self::Internal(_) => "internal orchestration error".to_owned(),
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
            proposal: None,
            invocation: None,
            grant: None,
            observation: None,
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
                RunState::PolicyReview => self.policy_step(&mut run, &mut active).await,
                RunState::ToolRunning => self.tool_step(&mut run, &mut active, &cancel).await,
                RunState::Replanning => self.replan_step(&mut run, &mut active, &cancel).await,
                RunState::WaitingApproval => {
                    self.approval_step(&mut run, &mut active, &cancel).await
                }
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
                ModelEvent::ToolProposal(proposal) => {
                    // A proposal ends this model turn and yields control to the
                    // policy engine — it is a request, never an authorization
                    // (invariant #1). Drop the stream; stage the proposal.
                    active.stream = None;
                    active.proposal = Some(proposal);
                    run.apply(RunEvent::ProposalReceived)?;
                    self.after_transition(run).await;
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

    /// `PolicyReview`: the sole authorization point (invariant #1). Evaluate the
    /// staged proposal, audit the decision unconditionally (no read-only
    /// shortcut, docs/06 §3), then route it: R0/R1 auto-authorize to
    /// `ToolRunning`; R2+ request approval (executor F2.3); a denial fails the
    /// run — the mutation never executes.
    async fn policy_step(&self, run: &mut Run, active: &mut Active) -> Result<StepFlow, StepError> {
        let stack = self
            .tools
            .as_ref()
            .ok_or(StepError::Unwired(RunState::PolicyReview))?;
        let proposal = active
            .proposal
            .take()
            .ok_or(StepError::Internal("policy review with no staged proposal"))?;

        let decision = policy::evaluate(&proposal, stack.registry, &stack.context);
        stack
            .audit
            .record(self.policy_audit_event(run, &proposal, &decision, stack))
            .await;

        match decision {
            PolicyDecision::Auto => {
                let (version, _executor) =
                    stack
                        .registry
                        .resolve(&proposal.tool_id)
                        .ok_or(StepError::Internal(
                            "auto-authorized tool absent from registry",
                        ))?;
                active.invocation = Some(ToolInvocation {
                    tool_id: proposal.tool_id,
                    tool_version: version,
                    arguments: proposal.arguments,
                });
                run.apply(RunEvent::AutoAuthorized)?;
                self.after_transition(run).await;
                Ok(StepFlow::Continue)
            }
            PolicyDecision::NeedsApproval { .. } => {
                // Re-stage the proposal for `approval_step` (WaitingApproval),
                // which presents the exact effect and, on approval, mints the
                // grant. The run cannot execute until a human decides.
                active.proposal = Some(proposal);
                run.apply(RunEvent::ApprovalRequested)?;
                self.after_transition(run).await;
                Ok(StepFlow::Continue)
            }
            PolicyDecision::Reject { reason } => Err(StepError::PolicyDenied(reason)),
        }
    }

    /// `WaitingApproval`: present the exact effect to the human and act on the
    /// decision (F2.3, docs/06 §4). A denial feeds back as an observation and
    /// replans; an approval mints a single-use grant bound to the *approved*
    /// (possibly edited) arguments and advances to execution. Editing the effect
    /// therefore rebinds the grant — executing the original args would fail
    /// validation (invalidation by hash, not a flag).
    async fn approval_step(
        &self,
        run: &mut Run,
        active: &mut Active,
        cancel: &CancellationToken,
    ) -> Result<StepFlow, StepError> {
        let stack = self
            .tools
            .as_ref()
            .ok_or(StepError::Unwired(RunState::WaitingApproval))?;
        let proposal = active
            .proposal
            .take()
            .ok_or(StepError::Internal("approval with no staged proposal"))?;
        let policy = stack
            .registry
            .policy_of(&proposal.tool_id)
            .ok_or(StepError::Internal("approval tool absent from registry"))?;
        let ttl = policy.risk.default_grant_ttl();
        let (version, _executor) = stack
            .registry
            .resolve(&proposal.tool_id)
            .ok_or(StepError::Internal("approval tool absent from registry"))?;
        let target_resource = proposal
            .tool_id
            .as_str()
            .parse()
            .map_err(|_| StepError::Internal("tool id is not a resource pattern"))?;

        let request = ApprovalRequest {
            run_id: run.id.clone(),
            tool_id: proposal.tool_id.clone(),
            exact_effect: policy::exact_effect(&proposal),
            proposed_arguments: proposal.arguments.clone(),
        };

        match run_or_cancel(stack.approval_gate.request(request, cancel.clone()), cancel).await {
            Ran::Cancelled => Ok(StepFlow::Cancelled),
            Ran::Done(ApprovalOutcome::Denied) => {
                stack
                    .audit
                    .record(self.tool_audit_event(run, "approval.denied", &proposal.tool_id, stack))
                    .await;
                // Feed the denial back so the model can replan (e.g. explain it
                // cannot proceed) rather than the run simply failing.
                active.observation = Some(ToolResult {
                    content: "The user denied the requested action.".to_owned(),
                    truncated: false,
                    compensation: None,
                });
                run.apply(RunEvent::ApprovalDenied)?;
                self.after_transition(run).await;
                Ok(StepFlow::Continue)
            }
            Ran::Done(ApprovalOutcome::Approved { arguments }) => {
                let grant = stack
                    .grant_minter
                    .mint(GrantBinding {
                        user_id: stack.context.user_id.clone(),
                        device_id: stack.context.device_id.clone(),
                        run_id: run.id.clone(),
                        tool_id: proposal.tool_id.clone(),
                        tool_version: version,
                        arguments: arguments.clone(),
                        target_resource,
                        ttl,
                    })
                    .await;
                stack
                    .audit
                    .record(self.tool_audit_event(run, "grant.minted", &proposal.tool_id, stack))
                    .await;
                active.invocation = Some(ToolInvocation {
                    tool_id: proposal.tool_id,
                    tool_version: version,
                    arguments,
                });
                active.grant = Some(grant);
                run.apply(RunEvent::ApprovalGranted)?;
                self.after_transition(run).await;
                Ok(StepFlow::Continue)
            }
        }
    }

    /// `ToolRunning`: execute one authorized invocation with cancellation. The
    /// R0/R1 auto path presents no grant; an R2+ call carries a grant that is
    /// validated + consumed immediately before execution — a mismatch, expiry,
    /// or replay means the executor is never called (invariant #1, docs/06 §4).
    async fn tool_step(
        &self,
        run: &mut Run,
        active: &mut Active,
        cancel: &CancellationToken,
    ) -> Result<StepFlow, StepError> {
        let stack = self
            .tools
            .as_ref()
            .ok_or(StepError::Unwired(RunState::ToolRunning))?;
        let invocation = active
            .invocation
            .take()
            .ok_or(StepError::Internal("tool run with no staged invocation"))?;
        let grant = active.grant.take();
        let (_version, executor) = stack
            .registry
            .resolve(&invocation.tool_id)
            .ok_or(StepError::Internal("invocation tool absent from registry"))?;

        // Validate + consume the grant right here, at the executor boundary —
        // not at decision time (docs/06 §4). No valid grant ⇒ no execution.
        if let Some(grant) = &grant
            && let Err(err) = stack
                .grant_validator
                .validate(grant, &invocation, self.clock.now())
                .await
        {
            stack
                .audit
                .record(self.tool_audit_event(run, "grant.rejected", &invocation.tool_id, stack))
                .await;
            return Err(StepError::Grant(err));
        }

        // Counts against `max_tool_calls`, enforced at the next loop top.
        run.usage.tool_calls = run.usage.tool_calls.saturating_add(1);
        let tool_id = invocation.tool_id.clone();

        // Race execution against cancellation so a hung tool is abandoned
        // promptly (invariant #4); dropping the future cancels it.
        match run_or_cancel(executor.execute(invocation, grant, cancel.clone()), cancel).await {
            Ran::Cancelled => Ok(StepFlow::Cancelled),
            Ran::Done(result) => {
                let result = result?;
                // A reversible tool's registered undo is surfaced in the timeline.
                if let Some(description) = &result.compensation {
                    self.sink
                        .emit(RunUpdate::CompensationRegistered {
                            run_id: run.id.clone(),
                            tool_id: tool_id.to_string(),
                            description: description.clone(),
                        })
                        .await;
                }
                active.observation = Some(result);
                run.apply(RunEvent::ToolObserved)?;
                self.after_transition(run).await;
                Ok(StepFlow::Continue)
            }
        }
    }

    /// `Replanning`: fold the tool observation into the next turn and re-invoke
    /// the model. M2 keeps context assembly minimal (the observation becomes the
    /// prompt); interleaved history/retrieval is M4.
    async fn replan_step(
        &self,
        run: &mut Run,
        active: &mut Active,
        cancel: &CancellationToken,
    ) -> Result<StepFlow, StepError> {
        let observation = active
            .observation
            .take()
            .ok_or(StepError::Internal("replan with no tool observation"))?;
        active.prompt = Some(format!("Tool result: {}", observation.content));
        // ModelInvoked is legal from Replanning as well as ContextReady.
        self.open_model_step(run, active, cancel).await
    }

    /// Build the audit record for a policy decision (invariant #6). The payload
    /// carries only controlled, non-sensitive values (validated tool id, a
    /// decision code) — never the rendered exact-effect, which could echo tool
    /// arguments (docs/06 §5).
    fn policy_audit_event(
        &self,
        run: &Run,
        proposal: &ToolProposal,
        decision: &PolicyDecision,
        stack: &ToolStack<'_>,
    ) -> AuditEvent {
        let (event_type, code) = match decision {
            PolicyDecision::Auto => ("policy.auto_authorized", "auto"),
            PolicyDecision::NeedsApproval { .. } => ("policy.approval_requested", "needs_approval"),
            PolicyDecision::Reject { reason } => ("policy.denied", reason.code()),
        };
        AuditEvent {
            occurred_at: self.clock.now(),
            actor: format!("user:{}", stack.context.user_id),
            event_type: event_type.to_owned(),
            target: format!("tool:{}", proposal.tool_id),
            correlation_id: Some(run.id.to_string()),
            payload_json: format!("{{\"decision\":\"{code}\"}}"),
        }
    }

    /// Audit record for an approval/grant lifecycle event (invariant #6). Like
    /// [`Self::policy_audit_event`], the payload carries only controlled values
    /// (validated tool id, event name) — never arguments or a grant secret.
    fn tool_audit_event(
        &self,
        run: &Run,
        event_type: &str,
        tool_id: &jarvis_domain::tools::ToolId,
        stack: &ToolStack<'_>,
    ) -> AuditEvent {
        AuditEvent {
            occurred_at: self.clock.now(),
            actor: format!("user:{}", stack.context.user_id),
            event_type: event_type.to_owned(),
            target: format!("tool:{tool_id}"),
            correlation_id: Some(run.id.to_string()),
            payload_json: "{}".to_owned(),
        }
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

/// The result of racing an arbitrary future against cancellation.
enum Ran<T> {
    Done(T),
    Cancelled,
}

/// Run `fut` to completion, but resolve to [`Ran::Cancelled`] the moment the
/// token fires — dropping (cancelling) `fut`. Used for tool execution so a hung
/// tool cannot outlive a cancel (invariant #4), without a `tokio::select!`.
async fn run_or_cancel<F: std::future::Future>(
    fut: F,
    cancel: &CancellationToken,
) -> Ran<F::Output> {
    use std::future::poll_fn;
    use std::task::Poll;

    let mut fut = std::pin::pin!(fut);
    let mut cancelled = std::pin::pin!(cancel.cancelled());
    poll_fn(|cx| {
        if cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Ran::Cancelled);
        }
        match fut.as_mut().poll(cx) {
            Poll::Ready(out) => Poll::Ready(Ran::Done(out)),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
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
