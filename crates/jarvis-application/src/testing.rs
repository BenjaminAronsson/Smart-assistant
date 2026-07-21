//! Test doubles for the orchestrator (docs/02 §4-§5). The `FakeModel` mirrors
//! the [`ModelProvider`] port and drives every orchestrator test and the golden
//! harness (F1.9) — kept feature-equivalent to a real provider (streaming
//! deltas, mid-stream errors, open failures, a hang for cancellation). Gated
//! behind the `fixtures` feature (and the crate's own `test` builds) so the
//! doubles never ship in a production binary.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_core::stream::{BoxStream, Stream};
use tokio_util::sync::CancellationToken;

use crate::model::{FinishReason, ModelError, ModelEvent, ModelProvider, ModelRequest, ProfileId};
use crate::orchestrator::{
    AssembledContext, CheckpointError, Checkpointer, Clock, ContextAssembler, ContextError,
    RunEventSink, RunInput, RunUpdate,
};
use crate::policy::{
    ApprovalGate, ApprovalOutcome, ApprovalRequest, AuditSink, GrantBinding, GrantMintError,
    GrantMinter, GrantValidator, ToolExecutor,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::grants::{ExecutionGrant, GrantError, GrantId, Sha256};
use jarvis_domain::run::{Run, RunState};
use jarvis_domain::tools::{CanonicalValue, ToolError, ToolInvocation, ToolResult};

/// A scripted [`ModelProvider`]: yields a fixed sequence of events, then either
/// ends the stream or (for the cancellation test) hangs. Records what it saw so
/// tests can assert on the prompt and on clean teardown.
pub struct FakeModel {
    script: Vec<ModelEvent>,
    /// Per-turn scripts for multi-turn (tool) flows: each `run` call pops the
    /// next turn's events; when empty, `script` is used. Lets one model drive
    /// `propose → observe → respond` (F2.2) without an infinite proposal loop.
    turns: Mutex<VecDeque<Vec<ModelEvent>>>,
    hang_after_drain: bool,
    open_error: Option<ModelError>,
    id: ProfileId,
    polled: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
    opened: AtomicBool,
    last_prompt: Mutex<Option<String>>,
}

impl FakeModel {
    fn new(
        script: Vec<ModelEvent>,
        hang_after_drain: bool,
        open_error: Option<ModelError>,
    ) -> Self {
        Self {
            script,
            turns: Mutex::new(VecDeque::new()),
            hang_after_drain,
            open_error,
            id: ProfileId::new("fake"),
            polled: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicBool::new(false)),
            opened: AtomicBool::new(false),
            last_prompt: Mutex::new(None),
        }
    }

    /// A model that yields a distinct event script on each successive turn (each
    /// must include its own terminal `Done`/`Error`, or a `ToolProposal` which
    /// ends the turn). After the scripted turns are exhausted, further turns end
    /// immediately (empty stream).
    pub fn scripted_turns(turns: impl IntoIterator<Item = Vec<ModelEvent>>) -> Self {
        let mut model = Self::new(Vec::new(), false, None);
        model.turns = Mutex::new(turns.into_iter().collect());
        model
    }

    /// Streams each chunk as a `TextDelta`, then a clean `Done(Stop)`.
    pub fn streaming<'a>(chunks: impl IntoIterator<Item = &'a str>) -> Self {
        let mut script: Vec<ModelEvent> = chunks
            .into_iter()
            .map(|c| ModelEvent::TextDelta(c.to_string()))
            .collect();
        script.push(ModelEvent::Done(FinishReason::Stop));
        Self::new(script, false, None)
    }

    /// Streams the chunks, then hangs forever (stream stays `Pending`) — used to
    /// prove mid-model cancellation.
    pub fn hangs_after<'a>(chunks: impl IntoIterator<Item = &'a str>) -> Self {
        let script = chunks
            .into_iter()
            .map(|c| ModelEvent::TextDelta(c.to_string()))
            .collect();
        Self::new(script, true, None)
    }

    /// Streams the chunks, then emits a mid-stream `Error`.
    pub fn streaming_then_error<'a>(
        chunks: impl IntoIterator<Item = &'a str>,
        error: ModelError,
    ) -> Self {
        let mut script: Vec<ModelEvent> = chunks
            .into_iter()
            .map(|c| ModelEvent::TextDelta(c.to_string()))
            .collect();
        script.push(ModelEvent::Error(error));
        Self::new(script, false, None)
    }

    /// Fails at open time (`run` returns `Err`).
    pub fn fails_open(error: ModelError) -> Self {
        Self::new(Vec::new(), false, Some(error))
    }

    /// A raw event script (must include its own terminal `Done`/`Error`).
    pub fn from_events(events: Vec<ModelEvent>) -> Self {
        Self::new(events, false, None)
    }

    /// Whether `run` was called (the stream was opened).
    pub fn opened(&self) -> bool {
        self.opened.load(Ordering::SeqCst)
    }
    /// Whether the produced stream has been polled at least once.
    pub fn was_polled(&self) -> bool {
        self.polled.load(Ordering::SeqCst)
    }
    /// Whether the produced stream has been dropped (no orphan on cancel).
    pub fn stream_dropped(&self) -> bool {
        self.dropped.load(Ordering::SeqCst)
    }
    /// The prompt of the most recent `run` call.
    pub fn last_prompt(&self) -> Option<String> {
        self.last_prompt.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelProvider for FakeModel {
    fn id(&self) -> ProfileId {
        self.id.clone()
    }

    async fn run(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ModelEvent>, ModelError> {
        *self.last_prompt.lock().unwrap() = Some(request.prompt);
        self.opened.store(true, Ordering::SeqCst);
        if let Some(error) = &self.open_error {
            return Err(error.clone());
        }
        // Multi-turn models pop the next turn's script; single-script models
        // replay `script`.
        let events = {
            let mut turns = self.turns.lock().unwrap();
            if turns.is_empty() {
                self.script.clone()
            } else {
                turns.pop_front().unwrap_or_default()
            }
        };
        let stream = ScriptStream {
            events: events.into(),
            hang_after_drain: self.hang_after_drain,
            polled: self.polled.clone(),
            dropped: self.dropped.clone(),
        };
        Ok(Box::pin(stream))
    }
}

/// The hand-rolled stream behind [`FakeModel`]. Built on `futures_core::Stream`
/// only — no `futures-util` (not permitted in this crate). Sets a flag on first
/// poll and on drop so tests can observe streaming and teardown.
struct ScriptStream {
    events: VecDeque<ModelEvent>,
    hang_after_drain: bool,
    polled: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl Stream for ScriptStream {
    type Item = ModelEvent;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // ScriptStream is Unpin (all fields are), so this is sound.
        let this = self.get_mut();
        this.polled.store(true, Ordering::SeqCst);
        match this.events.pop_front() {
            Some(event) => Poll::Ready(Some(event)),
            None if this.hang_after_drain => Poll::Pending,
            None => Poll::Ready(None),
        }
    }
}

impl Drop for ScriptStream {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// Context assembler that echoes the input text as the prompt (M1 minimal).
#[derive(Default)]
pub struct EchoAssembler;

#[async_trait]
impl ContextAssembler for EchoAssembler {
    async fn assemble(
        &self,
        _run: &Run,
        input: &RunInput,
        _cancel: &CancellationToken,
    ) -> Result<AssembledContext, ContextError> {
        Ok(AssembledContext {
            prompt: input.text.clone(),
        })
    }
}

/// Collects every [`RunUpdate`] for assertions.
#[derive(Default)]
pub struct RecordingSink {
    updates: Mutex<Vec<RunUpdate>>,
}

impl RecordingSink {
    /// A snapshot of all updates seen so far.
    pub fn updates(&self) -> Vec<RunUpdate> {
        self.updates.lock().unwrap().clone()
    }
    /// The concatenated streamed text (all `TextDelta`s in order).
    pub fn text(&self) -> String {
        self.updates
            .lock()
            .unwrap()
            .iter()
            .filter_map(|u| match u {
                RunUpdate::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
    /// The ordered sequence of states the run passed through.
    pub fn states(&self) -> Vec<RunState> {
        self.updates
            .lock()
            .unwrap()
            .iter()
            .filter_map(|u| match u {
                RunUpdate::StateChanged { state, .. } => Some(*state),
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
impl RunEventSink for RecordingSink {
    async fn emit(&self, update: RunUpdate) {
        self.updates.lock().unwrap().push(update);
    }
}

/// Records the state at every checkpoint (the restart-recovery source).
#[derive(Default)]
pub struct RecordingCheckpointer {
    saved: Mutex<Vec<RunState>>,
}

impl RecordingCheckpointer {
    pub fn saved_states(&self) -> Vec<RunState> {
        self.saved.lock().unwrap().clone()
    }
}

#[async_trait]
impl Checkpointer for RecordingCheckpointer {
    async fn save(&self, run: &Run) -> Result<(), CheckpointError> {
        self.saved.lock().unwrap().push(run.state);
        Ok(())
    }
}

/// A clock the test controls. Fixed by default; `advance` moves it forward so
/// duration budgets can be exercised deterministically.
pub struct ManualClock {
    now: Mutex<SystemTime>,
}

impl ManualClock {
    /// A clock parked at `secs` seconds after the Unix epoch.
    pub fn at_unix(secs: u64) -> Self {
        Self {
            now: Mutex::new(UNIX_EPOCH + Duration::from_secs(secs)),
        }
    }
    /// Move the clock forward.
    pub fn advance(&self, by: Duration) {
        let mut now = self.now.lock().unwrap();
        *now += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.now.lock().unwrap()
    }
}

/// A [`ToolExecutor`] that returns a canned result (or a canned error) and
/// records every call — including whether a grant was presented. Two properties
/// tests assert with it: the R0/R1 auto path executes with `grant = None`, and a
/// denied/approval-pending proposal never reaches `execute` at all (invariant
/// #1). Held as an `Arc` so a test keeps an inspection handle after registering.
pub struct FakeTool {
    content: String,
    fail: Option<ToolError>,
    compensation: Option<String>,
    /// If set, [`ToolExecutor::validate_args`] rejects any arguments that are not
    /// an object carrying this key — so a test can prove a malformed *edited*
    /// approval is caught before a grant binds (CF-9).
    required_key: Option<String>,
    /// Records, per call, the arguments and whether a grant was presented — so a
    /// test can assert the *approved* (possibly edited) arguments are what ran.
    calls: Mutex<Vec<(CanonicalValue, bool)>>,
}

impl FakeTool {
    /// A tool whose every call succeeds with `content`.
    pub fn returning(content: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            content: content.into(),
            fail: None,
            compensation: None,
            required_key: None,
            calls: Mutex::new(Vec::new()),
        })
    }

    /// A reversible tool that registers a compensating undo with each result.
    pub fn reversible(content: impl Into<String>, undo: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            content: content.into(),
            fail: None,
            compensation: Some(undo.into()),
            required_key: None,
            calls: Mutex::new(Vec::new()),
        })
    }

    /// A tool whose every call fails with `error`.
    pub fn failing(error: ToolError) -> Arc<Self> {
        Arc::new(Self {
            content: String::new(),
            fail: Some(error),
            compensation: None,
            required_key: None,
            calls: Mutex::new(Vec::new()),
        })
    }

    /// A tool that succeeds with `content` but whose argument validation
    /// (CF-9) rejects any arguments missing `key`. Used to prove an edited
    /// approval with malformed arguments fails at binding time, never executing.
    pub fn requiring_key(content: impl Into<String>, key: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            content: content.into(),
            fail: None,
            compensation: None,
            required_key: Some(key.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// How many times `execute` was called.
    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// For each call, whether a grant was presented (`true`) or not (`false`).
    pub fn calls_with_grant(&self) -> Vec<bool> {
        self.calls.lock().unwrap().iter().map(|(_, g)| *g).collect()
    }

    /// The arguments passed to each call (to assert the executed args).
    pub fn call_arguments(&self) -> Vec<CanonicalValue> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(a, _)| a.clone())
            .collect()
    }
}

#[async_trait]
impl ToolExecutor for FakeTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        grant: Option<ExecutionGrant>,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.calls
            .lock()
            .unwrap()
            .push((invocation.arguments.clone(), grant.is_some()));
        match &self.fail {
            Some(error) => Err(error.clone()),
            None => Ok(ToolResult {
                content: self.content.clone(),
                truncated: false,
                compensation: self.compensation.clone(),
            }),
        }
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        let Some(key) = &self.required_key else {
            return Ok(());
        };
        match arguments {
            CanonicalValue::Object(map) if map.contains_key(key) => Ok(()),
            _ => Err(ToolError::SchemaInvalid(format!("missing `{key}`"))),
        }
    }
}

/// Records every audit event the policy path emits (the F2.4 transactional sink
/// replaces this in production).
#[derive(Default)]
pub struct RecordingAuditSink {
    events: Mutex<Vec<AuditEvent>>,
}

impl RecordingAuditSink {
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events.lock().unwrap().clone()
    }

    /// The ordered `event_type`s recorded (e.g. `policy.auto_authorized`).
    pub fn event_types(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.event_type.clone())
            .collect()
    }
}

#[async_trait]
impl AuditSink for RecordingAuditSink {
    async fn record(&self, event: AuditEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// A scripted [`ApprovalGate`]: approve (echoing the proposed arguments),
/// approve with *edited* arguments (to prove the approved args bind, not the
/// proposal's), or deny.
pub struct FakeApprovalGate {
    mode: ApprovalMode,
}

enum ApprovalMode {
    ApproveEcho,
    ApproveEdited(CanonicalValue),
    Deny,
}

impl FakeApprovalGate {
    pub fn approving() -> Self {
        Self {
            mode: ApprovalMode::ApproveEcho,
        }
    }
    pub fn approving_with(edited: CanonicalValue) -> Self {
        Self {
            mode: ApprovalMode::ApproveEdited(edited),
        }
    }
    pub fn denying() -> Self {
        Self {
            mode: ApprovalMode::Deny,
        }
    }
}

#[async_trait]
impl ApprovalGate for FakeApprovalGate {
    async fn request(
        &self,
        request: ApprovalRequest,
        _cancel: CancellationToken,
    ) -> ApprovalOutcome {
        match &self.mode {
            ApprovalMode::ApproveEcho => ApprovalOutcome::Approved {
                arguments: request.proposed_arguments,
            },
            ApprovalMode::ApproveEdited(args) => ApprovalOutcome::Approved {
                arguments: args.clone(),
            },
            ApprovalMode::Deny => ApprovalOutcome::Denied,
        }
    }
}

/// A deterministic [`GrantMinter`]. The real sha2/random minter is F2.4 (infra);
/// this fake proves the *flow* (a grant is minted on approval and threaded to
/// the executor), so it uses placeholder id/hash and a far-future expiry.
pub struct FakeGrantMinter;

#[async_trait]
impl GrantMinter for FakeGrantMinter {
    async fn mint(&self, binding: GrantBinding) -> Result<ExecutionGrant, GrantMintError> {
        Ok(ExecutionGrant {
            grant_id: GrantId::from_bytes([1u8; 32]),
            user_id: binding.user_id,
            device_id: binding.device_id,
            run_id: binding.run_id,
            tool_id: binding.tool_id,
            tool_version: binding.tool_version,
            normalized_args_sha256: Sha256::from_bytes([2u8; 32]),
            target_resource: binding.target_resource,
            expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(u32::MAX as u64) + binding.ttl,
            single_use: true,
        })
    }
}

/// A [`GrantMinter`] that always faults — stands in for an infra/DB failure in
/// the real store (CF-6). Proves the orchestrator routes a mint fault to
/// `RunState::Failed` (not a panic) and never executes without a grant.
pub struct FailingGrantMinter;

#[async_trait]
impl GrantMinter for FailingGrantMinter {
    async fn mint(&self, _binding: GrantBinding) -> Result<ExecutionGrant, GrantMintError> {
        Err(GrantMintError("grant store unavailable".to_owned()))
    }
}

/// A [`GrantValidator`] that accepts or rejects by construction — enough to
/// prove the orchestrator honours validation before execution (invariant #1).
/// The real hash/expiry/consume logic + lifecycle table are F2.4 (infra).
pub struct FakeGrantValidator {
    reject: Option<GrantError>,
}

impl FakeGrantValidator {
    pub fn accepting() -> Self {
        Self { reject: None }
    }
    pub fn rejecting(error: GrantError) -> Self {
        Self {
            reject: Some(error),
        }
    }
}

#[async_trait]
impl GrantValidator for FakeGrantValidator {
    async fn validate(
        &self,
        _grant: &ExecutionGrant,
        _invocation: &ToolInvocation,
        _now: SystemTime,
    ) -> Result<(), GrantError> {
        match &self.reject {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}
