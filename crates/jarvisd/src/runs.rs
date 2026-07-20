//! Run REST surface + the host run engine (docs/05 §1, FR-01/06/07). The engine
//! is the composition root of the orchestrator: it spawns a tracked, cancellable
//! task per run, wires the neutral orchestrator ports to their infra/host
//! implementations, and — on a completed run — persists the assistant message so
//! it survives a reconnect (transient deltas are never replayed, docs/05 §3).
//!
//! Endpoints (all behind the bearer middleware):
//! * `POST /api/v1/sessions/{id}/messages` — persist the input, start a run,
//!   acknowledge in well under 100 ms (NFR-03); streaming follows on the WS.
//! * `GET  /api/v1/runs/{id}` — durable run snapshot (resync source).
//! * `POST /api/v1/runs/{id}/cancel` — request cancellation of an active run.
//! * `GET  /api/v1/sessions/{id}/timeline` — persisted messages + run events.
//!
//! Text never grants authority (invariant 1): these routes only *start* and
//! *cancel* runs and *mirror* their events. The loop advances solely through the
//! orchestrator; there is no path from a request body to a state transition.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::{Extension, Json};
use futures_util::stream::BoxStream;
use jarvis_application::health::ProviderHealthTracker;
use jarvis_application::model::{
    FinishReason, ModelError, ModelEvent, ModelProvider, ModelRequest, ProfileId,
};
use jarvis_application::orchestrator::{
    AssembledContext, Checkpointer, Clock, ContextAssembler, ContextError, Orchestrator,
    RunEventSink, RunInput, RunUpdate,
};
use jarvis_application::ports::{MessageStore, RepositoryError, RunStore, SessionStore};
use jarvis_application::queue::{RunPriority, RunQueue};
use jarvis_contracts::content::ContentBlock;
use jarvis_contracts::errors::ErrorCode;
use jarvis_contracts::events::DomainEvent;
use jarvis_contracts::messages::{MessageDto, SubmitMessageRequest};
use jarvis_contracts::runs::{RunAck, RunBudgetDto, RunDto};
use jarvis_contracts::timeline::{TimelineItem, TimelineResponse};
use jarvis_domain::conversations::{Message, MessageRole};
use jarvis_domain::ids::{MessageId, RunId, SessionId};
use jarvis_domain::run::{Run, RunBudget, RunState};
use jarvis_infra::dispatcher::OutboxRecord;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::Instrument;

use crate::auth::{DeviceContext, fresh_id};
use crate::problem::problem;
use crate::ws::{EventReader, WsHub};

/// The default page size for a timeline read when the client gives no `limit`.
const TIMELINE_DEFAULT_LIMIT: u32 = 200;
const TIMELINE_MAX_LIMIT: u32 = 500;

/// The host run engine: owns the orchestrator ports and the live-run registry.
pub struct RunEngine {
    model: Arc<dyn ModelProvider>,
    context: Arc<dyn ContextAssembler>,
    checkpointer: Arc<dyn Checkpointer>,
    messages: Arc<dyn MessageStore>,
    hub: Arc<WsHub>,
    clock: Arc<dyn Clock>,
    /// Active runs → their cancellation token, for `POST /runs/{id}/cancel`.
    active: Mutex<HashMap<RunId, CancellationToken>>,
    /// Tracks spawned run tasks so shutdown can drain them (invariant 4).
    tracker: TaskTracker,
    /// Parent of every run's token: cancelling it cancels all in-flight runs.
    shutdown: CancellationToken,
    /// Single-flight queue (F1.6): only one run can invoke the model at a time
    /// (prevent concurrent quota consumption, auth conflicts, billing issues).
    model_permit: Semaphore,
    /// Run queue (F1.7): park runs when provider unavailable, dequeue on recovery.
    queue: Mutex<RunQueue>,
    /// Provider health tracker (F1.7): classify errors, track state per profile.
    health: Arc<ProviderHealthTracker>,
}

impl RunEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: Arc<dyn ModelProvider>,
        context: Arc<dyn ContextAssembler>,
        checkpointer: Arc<dyn Checkpointer>,
        messages: Arc<dyn MessageStore>,
        hub: Arc<WsHub>,
        clock: Arc<dyn Clock>,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            model,
            context,
            checkpointer,
            messages,
            hub,
            clock,
            active: Mutex::new(HashMap::new()),
            tracker: TaskTracker::new(),
            shutdown,
            model_permit: Semaphore::new(1),
            queue: Mutex::new(RunQueue::new(100)),
            health: ProviderHealthTracker::new(),
        })
    }

    /// Spawn a tracked task that drives `run` to a terminal state. The token is a
    /// child of the shutdown token, so a graceful drain cancels every run.
    pub fn spawn(self: &Arc<Self>, run: Run, input: RunInput) {
        let run_id = run.id.clone();
        let cancel = self.shutdown.child_token();
        self.register(run_id.clone(), cancel.clone());

        let engine = Arc::clone(self);
        self.tracker.spawn(async move {
            engine.drive(run, input, cancel).await;
            engine.deregister(&run_id);
        });
    }

    /// The token for an actively-driven run, if any.
    pub fn active_token(&self, run_id: &RunId) -> Option<CancellationToken> {
        self.lock_active().get(run_id).cloned()
    }

    /// Stop accepting new runs and wait for the in-flight ones to drain. The
    /// caller cancels the shutdown token first (so each run winds down promptly).
    pub async fn drain(&self) {
        self.tracker.close();
        self.tracker.wait().await;
    }

    /// Get the current health state for the primary provider.
    pub fn get_provider_health(&self) -> (jarvis_application::health::HealthState, String) {
        let (state, reason) = self.health.get(&self.model.id());
        (state, reason)
    }

    /// Dequeue the next queued run (if any). Returns true if a run was dequeued.
    /// Caller is responsible for spawning the dequeued run (if any).
    pub fn try_dequeue(&self) -> Option<(Run, RunInput)> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.dequeue().map(|q| (q.run, q.input))
    }

    async fn drive(&self, run: Run, input: RunInput, cancel: CancellationToken) {
        let run_id = run.id.clone();
        let session_id = run.session_id.clone();
        let span = tracing::info_span!("run", run_id = %run_id, session_id = %session_id);

        // Single-flight queue (F1.6): acquire permit before invoking model,
        // hold it through the entire run so only one model invocation happens at a time.
        let _permit = match self.model_permit.acquire().await {
            Ok(p) => p,
            Err(_) => {
                // Semaphore closed (only on shutdown).
                tracing::warn!("run started after model permit closed");
                return;
            }
        };

        // A sink that broadcasts transient deltas (via the hub) AND accumulates
        // the response text so the host can commit the assistant message.
        let sink = RecordingSink {
            hub: Arc::clone(&self.hub),
            text: Mutex::new(String::new()),
        };
        let orchestrator = Orchestrator {
            model: &*self.model,
            context: &*self.context,
            checkpointer: &*self.checkpointer,
            sink: &sink,
            clock: &*self.clock,
        };

        let terminal = orchestrator
            .drive(run, input.clone(), cancel.clone())
            .instrument(span.clone())
            .await;

        // F1.7: if the run failed due to provider unavailability, enqueue it instead of failing.
        if terminal.state == RunState::Failed {
            if let Some(outcome) = &terminal.outcome {
                if let Some(detail) = &outcome.detail {
                    if detail.starts_with("provider unavailable:") {
                        // Record the error in health tracker
                        self.health
                            .record_error(&self.model.id(), &ModelError::Unavailable(
                                detail[21..].to_owned(), // strip "provider unavailable: " prefix
                            ));
                        // Enqueue the run at ContextReady checkpoint for retry (F1.7).
                        // F1.7 treats all queued runs as interactive (user-initiated).
                        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                        queue.enqueue(terminal, input, RunPriority::Interactive);
                        // Don't commit the assistant message; the run will retry when provider recovers
                        return;
                    }
                }
            }
        }

        if terminal.state == RunState::Completed {
            self.commit_assistant_message(&session_id, sink.take_text())
                .instrument(span)
                .await;
        }
    }

    /// Persist the assistant's message (docs/02 §5: the host commits the
    /// message the orchestrator produced). This emits `message.created` on the
    /// outbox → the WS, so a reconnecting client sees the response even though
    /// the token deltas were never persisted.
    async fn commit_assistant_message(&self, session_id: &SessionId, text: String) {
        let message = Message::new(
            fresh_id::<MessageId>(),
            session_id.clone(),
            MessageRole::Assistant,
            text,
            truncate_to_micros(self.clock.now()),
        );
        if let Err(error) = self.messages.append(&message).await {
            // Best-effort: the run already completed durably. A failed commit is
            // logged, never fatal (the run outcome stands).
            tracing::error!(%error, "assistant message commit failed");
        }
    }

    fn register(&self, run_id: RunId, cancel: CancellationToken) {
        self.lock_active().insert(run_id, cancel);
    }

    fn deregister(&self, run_id: &RunId) {
        self.lock_active().remove(run_id);
    }

    fn lock_active(&self) -> std::sync::MutexGuard<'_, HashMap<RunId, CancellationToken>> {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A [`RunEventSink`] that both broadcasts (through the hub) and records the
/// streamed text, so the host can commit the assistant message on completion.
struct RecordingSink {
    hub: Arc<WsHub>,
    text: Mutex<String>,
}

impl RecordingSink {
    fn take_text(&self) -> String {
        std::mem::take(
            &mut self
                .text
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

#[async_trait]
impl RunEventSink for RecordingSink {
    async fn emit(&self, update: RunUpdate) {
        if let RunUpdate::TextDelta { text, .. } = &update {
            self.text
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_str(text);
        }
        // The hub handles broadcast + the StateChanged/Finished drop (F1.4).
        self.hub.emit(update).await;
    }
}

/// The run REST surface state.
#[derive(Clone)]
pub struct RunApi {
    sessions: Arc<dyn SessionStore>,
    messages: Arc<dyn MessageStore>,
    runs: Arc<dyn RunStore>,
    events: Arc<dyn EventReader>,
    engine: Arc<RunEngine>,
}

impl RunApi {
    pub fn new(
        sessions: Arc<dyn SessionStore>,
        messages: Arc<dyn MessageStore>,
        runs: Arc<dyn RunStore>,
        events: Arc<dyn EventReader>,
        engine: Arc<RunEngine>,
    ) -> Self {
        Self {
            sessions,
            messages,
            runs,
            events,
            engine,
        }
    }
}

/// `POST /api/v1/sessions/{id}/messages` — submit input and start a run.
pub async fn submit_message(
    State(api): State<RunApi>,
    Path(session_id): Path<String>,
    Extension(_device): Extension<DeviceContext>,
    Json(request): Json<SubmitMessageRequest>,
) -> Result<(StatusCode, Json<RunAck>), Response> {
    let session_id: SessionId = session_id
        .parse()
        .map_err(|_| not_found("no such session"))?;

    // A clean 404 for an unknown session rather than surfacing a storage FK.
    if api
        .sessions
        .get(&session_id)
        .await
        .map_err(repository_problem)?
        .is_none()
    {
        return Err(not_found("no such session"));
    }

    let text = first_text(&request);
    if text.trim().is_empty() {
        return Err(problem(
            StatusCode::BAD_REQUEST,
            ErrorCode::ValidationFailed,
            "content must include a non-empty text block",
            None,
        ));
    }

    let now = truncate_to_micros(SystemTime::now());
    let user_message = Message::new(
        fresh_id::<MessageId>(),
        session_id.clone(),
        MessageRole::User,
        text.clone(),
        now,
    );
    api.messages
        .append(&user_message)
        .await
        .map_err(repository_problem)?;

    let run = Run::new(
        fresh_id::<RunId>(),
        session_id.clone(),
        RunBudget::default_interactive(),
    );
    api.runs.create(&run).await.map_err(repository_problem)?;

    let ack = RunAck {
        run_id: run.id.clone(),
        session_id,
        state: run.state.into(),
    };
    // Spawn AFTER the durable create so the run is recoverable even if the
    // process dies before the first checkpoint.
    api.engine.spawn(run, RunInput { text });

    Ok((StatusCode::ACCEPTED, Json(ack)))
}

/// `GET /api/v1/runs/{id}` — durable run snapshot.
pub async fn get_run(
    State(api): State<RunApi>,
    Path(id): Path<String>,
    Extension(_device): Extension<DeviceContext>,
) -> Result<Json<RunDto>, Response> {
    let id: RunId = id.parse().map_err(|_| not_found("no such run"))?;
    match api.runs.view(&id).await.map_err(repository_problem)? {
        Some(view) => Ok(Json(to_run_dto(&id, &view))),
        None => Err(not_found("no such run")),
    }
}

/// `POST /api/v1/runs/{id}/cancel` — request cancellation of an active run.
pub async fn cancel_run(
    State(api): State<RunApi>,
    Path(id): Path<String>,
    Extension(_device): Extension<DeviceContext>,
) -> Result<StatusCode, Response> {
    let id: RunId = id.parse().map_err(|_| not_found("no such run"))?;

    // Flip the token if we are actively driving the run; the terminal
    // `run.completed` (cancelled) event follows on the WS.
    if let Some(token) = api.engine.active_token(&id) {
        token.cancel();
        return Ok(StatusCode::ACCEPTED);
    }

    // Not active: distinguish unknown (404) from already-terminal / not
    // currently running (409 run.not_cancellable).
    match api.runs.view(&id).await.map_err(repository_problem)? {
        None => Err(not_found("no such run")),
        Some(_) => Err(problem(
            StatusCode::CONFLICT,
            ErrorCode::RunNotCancellable,
            "run is not currently running",
            None,
        )),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct TimelineParams {
    since: Option<i64>,
    limit: Option<u32>,
}

/// `GET /api/v1/sessions/{id}/timeline` — persisted messages + run events since
/// a cursor (the resync source; transient deltas are intentionally absent).
pub async fn get_timeline(
    State(api): State<RunApi>,
    Path(session_id): Path<String>,
    Query(params): Query<TimelineParams>,
    Extension(_device): Extension<DeviceContext>,
) -> Result<Json<TimelineResponse>, Response> {
    let session_id: SessionId = session_id
        .parse()
        .map_err(|_| not_found("no such session"))?;
    let since = params.since.unwrap_or(0).max(0);
    let limit = params
        .limit
        .unwrap_or(TIMELINE_DEFAULT_LIMIT)
        .clamp(1, TIMELINE_MAX_LIMIT);

    let rows = api
        .events
        .timeline(session_id.as_str(), since, i64::from(limit))
        .await
        .map_err(repository_problem)?;

    let full_page = rows.len() == limit as usize;
    let mut max_id = since;
    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        max_id = max_id.max(row.id);
        if let Some(item) = timeline_item(row) {
            items.push(item);
        }
    }
    // Only advertise a cursor when the page was full — otherwise it is the head.
    let next_since = full_page.then(|| u64::try_from(max_id).unwrap_or(0));

    Ok(Json(TimelineResponse { items, next_since }))
}

/// Map a persisted outbox row to a timeline item. A `message.created` row is a
/// [`MessageDto`]; every `run.*` row reconstructs a [`DomainEvent`] by folding
/// the envelope `type` back into the payload. Rows that do not map (an unknown
/// future type) are skipped rather than failing the whole page.
fn timeline_item(row: &OutboxRecord) -> Option<TimelineItem> {
    if row.event_type == "message.created" {
        let message: MessageDto =
            serde_json::from_value(row.payload.get("message")?.clone()).ok()?;
        return Some(TimelineItem::Message { message });
    }
    domain_event(row).map(|event| TimelineItem::RunEvent { event })
}

/// The persisted `DomainEvent` tags jarvisd itself writes (docs/05 §3). Kept
/// beside [`domain_event`] so a decode failure on a KNOWN tag — a real bug that
/// would drop a registered event from a resync page — is logged, while a
/// genuinely unknown future tag stays a silent forward-compatible skip. Mirror
/// `jarvis_contracts::events::DomainEvent::event_type()`.
const KNOWN_DOMAIN_EVENT_TAGS: &[&str] = &[
    "run.started",
    "run.state_changed",
    "run.queued",
    "run.completed",
    "message.created",
    "provider.health_changed",
    "run.checkpoint_saved",
];

fn domain_event(row: &OutboxRecord) -> Option<DomainEvent> {
    let mut object = row.payload.as_object()?.clone();
    object.insert(
        "type".to_owned(),
        serde_json::Value::String(row.event_type.clone()),
    );
    match serde_json::from_value(serde_json::Value::Object(object)) {
        Ok(event) => Some(event),
        Err(error) => {
            // A registered tag that fails to decode is a real bug (a persisted
            // event silently missing from resync); surface it. An unknown future
            // tag is expected forward-compat and stays a silent skip.
            if KNOWN_DOMAIN_EVENT_TAGS.contains(&row.event_type.as_str()) {
                tracing::warn!(
                    event_type = %row.event_type,
                    id = row.id,
                    %error,
                    "dropping malformed persisted event from resync"
                );
            }
            None
        }
    }
}

fn to_run_dto(id: &RunId, view: &jarvis_application::ports::RunView) -> RunDto {
    let run = &view.run;
    RunDto {
        id: id.clone(),
        session_id: run.session_id.clone(),
        state: run.state.into(),
        budget: RunBudgetDto {
            max_model_turns: run.budget.max_model_turns,
            max_tool_calls: run.budget.max_tool_calls,
            max_duration_secs: run.budget.max_duration.as_secs(),
            max_artifact_bytes: run.budget.max_artifact_bytes,
        },
        outcome: run.outcome.as_ref().map(Into::into),
        created_at: rfc3339(view.created_at),
        updated_at: rfc3339(view.updated_at),
    }
}

/// The concatenated text of every `text` block (M1 messages carry exactly one);
/// unknown/forward-compat blocks contribute nothing.
fn first_text(request: &SubmitMessageRequest) -> String {
    request
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Unknown => None,
        })
        .collect()
}

fn not_found(detail: &'static str) -> Response {
    problem(
        StatusCode::NOT_FOUND,
        ErrorCode::ResourceNotFound,
        detail,
        None,
    )
}

/// One mapping for every RepositoryError crossing the boundary (docs/05 §7);
/// storage internals never reach the client.
fn repository_problem(error: RepositoryError) -> Response {
    match error {
        RepositoryError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            ErrorCode::IdempotencyConflict,
            "idempotency key reused with a different payload",
            None,
        ),
        RepositoryError::Conflict(_) => problem(
            StatusCode::CONFLICT,
            ErrorCode::ResourceVersionConflict,
            "resource conflict",
            None,
        ),
        RepositoryError::Storage(error) => {
            tracing::error!(%error, "run storage failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "storage unavailable",
                None,
            )
        }
    }
}

fn rfc3339(t: SystemTime) -> String {
    OffsetDateTime::from(t)
        .format(&Rfc3339)
        .expect("UTC timestamp formats")
}

/// Truncate to timestamptz precision so the stored value and every later render
/// produce the identical RFC 3339 string (mirrors the sessions surface).
fn truncate_to_micros(t: SystemTime) -> SystemTime {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => std::time::UNIX_EPOCH + std::time::Duration::from_micros(d.as_micros() as u64),
        Err(_) => t,
    }
}

/// `GET /api/v1/providers` — return current health state for all known providers (F1.7).
pub async fn get_providers(
    State(api): State<RunApi>,
    Extension(_device): Extension<DeviceContext>,
) -> Result<Json<jarvis_contracts::providers::ProvidersResponse>, Response> {
    use jarvis_application::health::HealthState;
    use jarvis_contracts::providers::{ProviderDto, ProviderState};

    let (app_state, reason) = api.engine.get_provider_health();
    let model_id = api.engine.model.id();

    // Convert from application HealthState to wire ProviderState
    let wire_state = match app_state {
        HealthState::Healthy => ProviderState::Healthy,
        HealthState::Degraded => ProviderState::Degraded,
        HealthState::Unavailable => ProviderState::Unavailable,
    };

    let provider_dto = ProviderDto {
        id: model_id.0.clone(),
        state: wire_state,
        quota: None, // F1.7 does not track quota windows yet
        reason: if reason.is_empty() { None } else { Some(reason) },
    };

    Ok(Json(jarvis_contracts::providers::ProvidersResponse {
        providers: vec![provider_dto],
    }))
}

// ---------------------------------------------------------------------------
// Interim orchestrator ports for the M1 text slice. The Claude CLI adapter
// (F1.6) replaces `EchoModel`; richer context assembly (memory/retrieval) lands
// in M4. `SystemClock` is the production clock. None of these are test doubles —
// they are the real (minimal) M1 behaviour, and they never bypass the port.
// ---------------------------------------------------------------------------

/// A deterministic interim provider: echoes the prompt back as one streamed
/// chunk, then completes. Lets the vertical slice run end-to-end before the real
/// Claude CLI adapter (F1.6) is wired.
pub struct EchoModel {
    id: ProfileId,
}

impl Default for EchoModel {
    fn default() -> Self {
        Self {
            id: ProfileId::new("deterministic"),
        }
    }
}

#[async_trait]
impl ModelProvider for EchoModel {
    fn id(&self) -> ProfileId {
        self.id.clone()
    }

    async fn run(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ModelEvent>, ModelError> {
        let reply = format!("echo: {}", request.prompt);
        let events = vec![
            ModelEvent::TextDelta(reply),
            ModelEvent::Done(FinishReason::Stop),
        ];
        Ok(Box::pin(futures_util::stream::iter(events)))
    }
}

/// Minimal M1 context assembly: the prompt is the input text. Retrieval and
/// token-budget provenance land with memory in M4.
#[derive(Default)]
pub struct PassthroughAssembler;

#[async_trait]
impl ContextAssembler for PassthroughAssembler {
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

/// The production clock — the one place jarvisd reads wall time for runs.
#[derive(Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";

    fn row(id: i64, event_type: &str, payload: serde_json::Value) -> OutboxRecord {
        OutboxRecord {
            id,
            event_type: event_type.to_owned(),
            payload,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn message_created_row_maps_to_a_message_item() {
        let record = row(
            1,
            "message.created",
            json!({ "message": {
                "id": RUN, "sessionId": SESSION, "role": "user",
                "content": [{ "type": "text", "text": "hi" }],
                "createdAt": "2026-07-20T00:00:00Z"
            }}),
        );
        match timeline_item(&record).expect("maps") {
            TimelineItem::Message { message } => {
                assert_eq!(message.session_id.as_str(), SESSION);
                assert!(matches!(
                    message.role,
                    jarvis_contracts::messages::MessageRole::User
                ));
            }
            other => panic!("expected a message item, got {other:?}"),
        }
    }

    #[test]
    fn run_event_row_folds_type_back_into_the_event() {
        let record = row(
            2,
            "run.state_changed",
            json!({ "runId": RUN, "state": "model_running" }),
        );
        match timeline_item(&record).expect("maps") {
            TimelineItem::RunEvent { event } => {
                assert!(matches!(event, DomainEvent::RunStateChanged { .. }));
            }
            other => panic!("expected a run event, got {other:?}"),
        }
    }

    #[test]
    fn run_completed_row_carries_the_outcome() {
        let record = row(
            3,
            "run.completed",
            json!({ "runId": RUN, "outcome": { "kind": "completed" } }),
        );
        match timeline_item(&record).expect("maps") {
            TimelineItem::RunEvent {
                event: DomainEvent::RunCompleted { outcome, .. },
            } => assert!(matches!(
                outcome.kind,
                jarvis_contracts::runs::RunOutcomeKind::Completed
            )),
            other => panic!("expected a completed run event, got {other:?}"),
        }
    }

    #[test]
    fn unrecognized_event_type_is_skipped_not_fatal() {
        let record = row(4, "run.mystery_future_event", json!({ "runId": RUN }));
        assert!(timeline_item(&record).is_none());
    }
}
