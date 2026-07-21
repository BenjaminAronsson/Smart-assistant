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
use crate::policy::{AuditSink, ToolExecutor};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::run::{Run, RunState};
use jarvis_domain::tools::{ToolError, ToolInvocation, ToolResult};

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
    calls: Mutex<Vec<bool>>,
}

impl FakeTool {
    /// A tool whose every call succeeds with `content`.
    pub fn returning(content: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            content: content.into(),
            fail: None,
            calls: Mutex::new(Vec::new()),
        })
    }

    /// A tool whose every call fails with `error`.
    pub fn failing(error: ToolError) -> Arc<Self> {
        Arc::new(Self {
            content: String::new(),
            fail: Some(error),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// How many times `execute` was called.
    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// For each call, whether a grant was presented (`true`) or not (`false`).
    pub fn calls_with_grant(&self) -> Vec<bool> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ToolExecutor for FakeTool {
    async fn execute(
        &self,
        _invocation: ToolInvocation,
        grant: Option<ExecutionGrant>,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.calls.lock().unwrap().push(grant.is_some());
        match &self.fail {
            Some(error) => Err(error.clone()),
            None => Ok(ToolResult {
                content: self.content.clone(),
                truncated: false,
            }),
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
