//! The WebSocket hub and `/ws/v1` upgrade (docs/05 §1-§3). One
//! token-authenticated fan-out carries the owner's run events. Two producers
//! converge here:
//!
//! * committed **domain events** arrive via [`OutboxPublisher`] — the dispatcher
//!   calls us after commit. They are persisted and replayable; `seq` is the
//!   outbox row `id`, the same global cursor the timeline `since` uses.
//! * transient **text deltas** and voice recognition hypotheses arrive through
//!   bounded in-process streams. They are never persisted and never replayed.
//!
//! The hub owns every envelope field (docs/05 §3); payload authors never set
//! `seq`/`occurredAt`/etc. Run **state** changes are deliberately NOT emitted
//! through the sink — they are persisted by the checkpointer and delivered on
//! the outbox path, so the sink drops `StateChanged`/`Finished` to avoid the
//! double-emit the F1.4 review flagged.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::SystemTime;

use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderValue;
use axum::response::Response;
use futures_util::stream::{BoxStream, StreamExt, poll_fn};
use jarvis_application::orchestrator::{RunEventSink, RunUpdate};
use jarvis_application::ports::{DisplayDirectiveSink, RepositoryError};
use jarvis_application::voice::{AudioFormat, SpeechTranscriber, TranscriptEvent};
use jarvis_contracts::CONTRACT_VERSION;
use jarvis_contracts::cards::{AgendaEventDto, HudCardDto};
use jarvis_contracts::deepdive::{CanvasActionDto, HudCanvasDto};
use jarvis_contracts::display::{DisplayDirective, SurfaceDto};
use jarvis_contracts::envelope::{Channel, EventEnvelope};
use jarvis_contracts::events::TransientEvent;
use jarvis_domain::display::Surface;
use jarvis_domain::ids::RunId;
use jarvis_infra::dispatcher::{OutboxPublisher, OutboxRecord, PublishError};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Bounded fan-out buffer. A client that falls this far behind is disconnected
/// (`broadcast::Lagged`) and resyncs via REST — never unbounded buffering
/// (low-power / DoS guard). Generous for a single owner's devices.
const CHANNEL_CAPACITY: usize = 1024;

/// Rows per page when replaying persisted events on a `?since=` reconnect.
const REPLAY_PAGE: i64 = 256;

/// Inbound WS frame/message ceiling. Voice PCM chunks are intentionally kept
/// below this bound; the browser emits 20–40 ms frames (docs/05 §1), far below
/// the 64 MiB tungstenite default (DoS hardening, security-auditor F1.5).
const MAX_INBOUND_FRAME_BYTES: usize = 64 * 1024;

/// Read side of the persisted event log (docs/05 §3), abstracted so the hub and
/// timeline endpoint can be driven by a fake in tests. Implemented by
/// `jarvis_infra::events::PgEventLog`; returns raw outbox rows which jarvisd
/// maps to the wire types (infra cannot depend on `jarvis-contracts`).
#[async_trait]
pub trait EventReader: Send + Sync {
    /// Every committed event with `id > since`, oldest first (the WS replay).
    async fn since(&self, since: i64, limit: i64) -> Result<Vec<OutboxRecord>, RepositoryError>;
    /// The persisted timeline for one session with `id > since`, oldest first.
    async fn timeline(
        &self,
        session_id: &str,
        since: i64,
        limit: i64,
    ) -> Result<Vec<OutboxRecord>, RepositoryError>;
}

#[async_trait]
impl EventReader for jarvis_infra::events::PgEventLog {
    async fn since(&self, since: i64, limit: i64) -> Result<Vec<OutboxRecord>, RepositoryError> {
        jarvis_infra::events::PgEventLog::since(self, since, limit).await
    }
    async fn timeline(
        &self,
        session_id: &str,
        since: i64,
        limit: i64,
    ) -> Result<Vec<OutboxRecord>, RepositoryError> {
        jarvis_infra::events::PgEventLog::timeline(self, session_id, since, limit).await
    }
}

pub struct WsHub {
    tx: broadcast::Sender<Arc<EventEnvelope>>,
    /// The largest outbox `id` broadcast so far — the domain resync high-water.
    /// Transient deltas ride at this value; they never advance the cursor, so a
    /// client tracks its `since` from domain events only (docs/05 §3).
    high_water: AtomicU64,
}

impl WsHub {
    pub fn new() -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Arc::new(Self {
            tx,
            high_water: AtomicU64::new(0),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<EventEnvelope>> {
        self.tx.subscribe()
    }

    pub fn high_water(&self) -> u64 {
        self.high_water.load(Ordering::SeqCst)
    }

    /// Envelope for a committed outbox row. `seq` is the row `id` (global,
    /// monotonic, == timeline `since`); `occurredAt` is the row's stored commit
    /// time, so a replayed event keeps its ORIGINAL time, not "now". The payload
    /// is forwarded verbatim, with the discriminator on the envelope `type`
    /// (never re-typed — F1.4 note). Shared by live delivery and `?since=` replay
    /// so the two never disagree.
    fn domain_envelope(&self, record: &OutboxRecord) -> EventEnvelope {
        EventEnvelope {
            v: CONTRACT_VERSION,
            seq: seq_of(record.id),
            channel: Channel::Session,
            event_type: record.event_type.clone(),
            occurred_at: rfc3339(record.created_at),
            trace_id: None,
            resource_version: None,
            payload: record.payload.clone(),
        }
    }

    /// Broadcast a committed domain event. No subscribers is success: the event
    /// is durable and any client resyncs via REST — we never re-deliver just
    /// because nobody is currently listening.
    fn broadcast_domain(&self, record: &OutboxRecord) {
        self.high_water
            .fetch_max(seq_of(record.id), Ordering::SeqCst);
        let _ = self.tx.send(Arc::new(self.domain_envelope(record)));
    }

    /// Broadcast a display directive on the `display` channel (FR-09/10). Like a
    /// text delta it is transient — a command to the agent, not a replayable
    /// timeline event — so it rides at the current high-water `seq` and never
    /// advances the resync cursor. Returns true if at least one WS client was
    /// subscribed (best-effort delivery; no agent connected ⇒ audited-but-
    /// undelivered). `app_id` is derived server-side from the closed surface set,
    /// never from model/user text.
    fn broadcast_display(&self, placement: &jarvis_domain::display::SurfacePlacement) -> bool {
        let directive = DisplayDirective::PlaceSurface {
            surface: surface_dto(placement.surface),
            app_id: placement.surface.app_id().to_owned(),
            monitor: placement.monitor.as_str().to_owned(),
        };
        let (event_type, payload) =
            split_tagged(serde_json::to_value(&directive).expect("directive serializes"));
        let envelope = EventEnvelope {
            v: CONTRACT_VERSION,
            seq: self.high_water.load(Ordering::SeqCst),
            channel: Channel::Display,
            event_type,
            occurred_at: now_rfc3339(),
            trace_id: None,
            resource_version: None,
            payload,
        };
        self.tx.send(Arc::new(envelope)).is_ok()
    }

    /// Broadcast the transient `media.state` event (F3a.7, FR-22). Like a text
    /// delta it rides at the current high-water `seq` and never advances the
    /// resync cursor: it is a current-value readout, not timeline history, and a
    /// client that missed one recovers by reading `GET /api/v1/media/state`
    /// rather than by replay (docs/05 §3).
    pub fn broadcast_media_state(&self, state: jarvis_contracts::media::MediaStateDto) {
        let event = TransientEvent::MediaState { state };
        let (event_type, payload) =
            split_tagged(serde_json::to_value(&event).expect("transient event serializes"));
        let envelope = EventEnvelope {
            v: CONTRACT_VERSION,
            seq: self.high_water.load(Ordering::SeqCst),
            channel: Channel::Session,
            event_type,
            occurred_at: now_rfc3339(),
            trace_id: None,
            resource_version: None,
            payload,
        };
        let _ = self.tx.send(Arc::new(envelope));
    }

    /// Broadcast a disposable recognition hypothesis on the voice channel.
    /// The durable user message, once a voice turn is bound to a session, is
    /// the source of truth; partials are intentionally never replayed.
    fn broadcast_voice_transcript(&self, stream_id: &str, text: String, is_final: bool) {
        let event = TransientEvent::VoiceTranscript {
            stream_id: stream_id.to_owned(),
            text,
            is_final,
        };
        let (event_type, payload) =
            split_tagged(serde_json::to_value(&event).expect("transient event serializes"));
        let envelope = EventEnvelope {
            v: CONTRACT_VERSION,
            seq: self.high_water.load(Ordering::SeqCst),
            channel: Channel::Voice,
            event_type,
            occurred_at: now_rfc3339(),
            trace_id: None,
            resource_version: None,
            payload,
        };
        let _ = self.tx.send(Arc::new(envelope));
    }

    /// Broadcast a transient `hud.canvas` instruction (F3b.6, FR-27/ADR-017):
    /// what this turn does to the materialization canvas, plus the cards that
    /// belong on it. Like a text delta it rides at the current high-water `seq`
    /// and never advances the resync cursor — see
    /// [`jarvis_contracts::events::TransientEvent::HudCanvas`] for why a canvas
    /// instruction cannot honestly be a replayable domain event.
    pub fn broadcast_hud_canvas(&self, canvas: jarvis_contracts::deepdive::HudCanvasDto) {
        let event = TransientEvent::HudCanvas { canvas };
        let (event_type, payload) =
            split_tagged(serde_json::to_value(&event).expect("transient event serializes"));
        let envelope = EventEnvelope {
            v: CONTRACT_VERSION,
            seq: self.high_water.load(Ordering::SeqCst),
            channel: Channel::Session,
            event_type,
            occurred_at: now_rfc3339(),
            trace_id: None,
            resource_version: None,
            payload,
        };
        let _ = self.tx.send(Arc::new(envelope));
    }

    fn broadcast_agenda(
        &self,
        run_id: &RunId,
        events: Vec<jarvis_application::calendar::CalendarEvent>,
    ) {
        let card = HudCardDto::Agenda {
            id: format!("agenda-{run_id}"),
            title: "Today".to_owned(),
            events: events
                .into_iter()
                .map(|event| AgendaEventDto {
                    title: event.title,
                    start: rfc3339_system_time(event.start),
                    end: rfc3339_system_time(event.end),
                    all_day: event.all_day,
                })
                .collect(),
        };
        self.broadcast_hud_canvas(HudCanvasDto {
            session_id: None,
            action: CanvasActionDto::Extend,
            label: "Today".to_owned(),
            cards: vec![card],
            offer: None,
            handoff: None,
        });
    }

    /// Broadcast a transient text delta at the current high-water `seq` (it does
    /// not advance the resync cursor; a lost delta is re-derived, docs/05 §3).
    fn broadcast_delta(&self, run_id: &RunId, text: &str) {
        let event = TransientEvent::TextDelta {
            run_id: run_id.clone(),
            text: text.to_owned(),
        };
        // Split the `type` tag out of the payload so the wire matches the outbox
        // convention: discriminator on the envelope, fields in the payload.
        let (event_type, payload) =
            split_tagged(serde_json::to_value(&event).expect("transient event serializes"));
        let envelope = EventEnvelope {
            v: CONTRACT_VERSION,
            seq: self.high_water.load(Ordering::SeqCst),
            channel: Channel::Session,
            event_type,
            occurred_at: now_rfc3339(),
            trace_id: None,
            resource_version: None,
            payload,
        };
        let _ = self.tx.send(Arc::new(envelope));
    }

    /// Broadcast the live degraded-mode queue notice. It shares the transient
    /// sequence semantics of token deltas; a reconnect gets the durable run
    /// snapshot and a fresh provider poll instead (FR-12).
    fn broadcast_queued(&self, run_id: &RunId, reason: &str, position: usize) {
        let event = TransientEvent::DegradedQueued {
            run_id: run_id.clone(),
            reason: reason.to_owned(),
            position,
        };
        let (event_type, payload) =
            split_tagged(serde_json::to_value(&event).expect("transient event serializes"));
        let envelope = EventEnvelope {
            v: CONTRACT_VERSION,
            seq: self.high_water.load(Ordering::SeqCst),
            channel: Channel::Session,
            event_type,
            occurred_at: now_rfc3339(),
            trace_id: None,
            resource_version: None,
            payload,
        };
        let _ = self.tx.send(Arc::new(envelope));
    }
}

/// Deep-dive turns and list commands publish canvas instructions through this
/// impl (F3b.6).
impl crate::cards::CanvasSink for WsHub {
    fn publish(&self, canvas: jarvis_contracts::deepdive::HudCanvasDto) {
        self.broadcast_hud_canvas(canvas);
    }
}

/// The dispatcher publishes committed domain events through this impl.
#[async_trait]
impl OutboxPublisher for WsHub {
    async fn publish(&self, record: &OutboxRecord) -> Result<(), PublishError> {
        // Broadcast never fails per-subscriber, and "no subscribers" is success
        // (durable + REST resync). The `Result` exists for a future delivery
        // path with a fallible durable step; there is none in M1.
        self.broadcast_domain(record);
        Ok(())
    }
}

/// jarvisd dispatches resolved display placements to connected agents here.
#[async_trait]
impl DisplayDirectiveSink for WsHub {
    async fn dispatch(&self, placement: &jarvis_domain::display::SurfacePlacement) -> bool {
        self.broadcast_display(placement)
    }
}

/// Cast-a-link dispatch (F3a.7, ADR-012): the media window's URL rides the same
/// display channel as a placement. The URL was validated (`https`, bounded, no
/// control characters) by the tool before it got here, and the agent validates
/// it again before launching anything.
#[async_trait]
impl jarvis_application::ports::MediaWindowSink for WsHub {
    async fn open_url(&self, url: &str, monitor: &jarvis_domain::display::MonitorId) -> bool {
        let directive = DisplayDirective::OpenMediaUrl {
            url: url.to_owned(),
            monitor: monitor.as_str().to_owned(),
        };
        let (event_type, payload) =
            split_tagged(serde_json::to_value(&directive).expect("directive serializes"));
        let envelope = EventEnvelope {
            v: CONTRACT_VERSION,
            seq: self.high_water.load(Ordering::SeqCst),
            channel: Channel::Display,
            event_type,
            occurred_at: now_rfc3339(),
            trace_id: None,
            resource_version: None,
            payload,
        };
        self.tx.send(Arc::new(envelope)).is_ok()
    }
}

/// Map the domain surface to its wire mirror. Exhaustive on purpose: a new
/// `Surface` variant forces a wire mapping decision here (no `_` arm).
fn surface_dto(surface: Surface) -> SurfaceDto {
    match surface {
        Surface::Conversation => SurfaceDto::Conversation,
        Surface::RunTimeline => SurfaceDto::RunTimeline,
        Surface::ApprovalTray => SurfaceDto::ApprovalTray,
        Surface::ArtifactCanvas => SurfaceDto::ArtifactCanvas,
        Surface::AmbientStatus => SurfaceDto::AmbientStatus,
        Surface::Diagnostics => SurfaceDto::Diagnostics,
        Surface::MediaWindow => SurfaceDto::MediaWindow,
    }
}

/// The orchestrator emits run updates through this impl.
#[async_trait]
impl RunEventSink for WsHub {
    async fn emit(&self, update: RunUpdate) {
        match update {
            RunUpdate::TextDelta { run_id, text } => self.broadcast_delta(&run_id, &text),
            RunUpdate::Queued {
                run_id,
                reason,
                position,
            } => self.broadcast_queued(&run_id, &reason, position),
            RunUpdate::Agenda { run_id, events } => self.broadcast_agenda(&run_id, events),
            // Persisted by the checkpointer and delivered on the outbox path —
            // dropping them here is the double-emit reconciliation (F1.4).
            // CompensationRegistered (F2.3) is likewise a persisted domain event;
            // its outbox delivery + approval-tray rendering lands in F2.5. No
            // tools are wired into jarvisd yet (tools: None), so it never fires.
            RunUpdate::StateChanged { .. }
            | RunUpdate::Finished { .. }
            | RunUpdate::CompensationRegistered { .. } => {}
        }
    }
}

/// `?since=` cursor for the WS reconnect replay. Absent = live-only from now.
#[derive(Debug, Deserialize)]
pub struct WsParams {
    pub since: Option<i64>,
}

/// State the `/ws/v1` route carries: the hub to subscribe to, the event log for
/// replay, and the process shutdown token for a clean close on drain.
#[derive(Clone)]
pub struct WsState {
    pub hub: Arc<WsHub>,
    pub events: Arc<dyn EventReader>,
    pub shutdown: CancellationToken,
    /// Optional M5 STT adapter. `None` keeps voice capture visible but disabled
    /// at the daemon boundary; the browser still fails closed on no transcript.
    pub transcriber: Option<Arc<dyn SpeechTranscriber>>,
}

struct ActiveVoiceStream {
    stream_id: String,
    audio_tx: Option<mpsc::Sender<Vec<u8>>>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

fn audio_stream(rx: mpsc::Receiver<Vec<u8>>) -> BoxStream<'static, Vec<u8>> {
    let mut rx = rx;
    Box::pin(poll_fn(move |cx| rx.poll_recv(cx)))
}

fn start_voice_stream(
    transcriber: Arc<dyn SpeechTranscriber>,
    hub: Arc<WsHub>,
    stream_id: String,
    format: AudioFormat,
    cancel: CancellationToken,
) -> ActiveVoiceStream {
    let (tx, rx) = mpsc::channel(32);
    let task_stream_id = stream_id.clone();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        let result = transcriber
            .transcribe(audio_stream(rx), format, task_cancel.clone())
            .await;
        let mut transcript = match result {
            Ok(transcript) => transcript,
            Err(error) => {
                tracing::warn!(%error, stream_id = %task_stream_id, "voice transcription could not start");
                return;
            }
        };
        while let Some(event) = transcript.next().await {
            match event {
                TranscriptEvent::Partial(text) => {
                    hub.broadcast_voice_transcript(&task_stream_id, text, false)
                }
                TranscriptEvent::Final(text) => {
                    hub.broadcast_voice_transcript(&task_stream_id, text, true)
                }
                // A mid-stream STT failure. Logged rather than dropped, but the
                // browser currently sees only "no transcript" — indistinguishable
                // from silence. Surfacing it to the user needs a `voice.error`
                // transient event in jarvis-contracts (not added here: the voice
                // contract surface is still in flight). Until then this is a
                // known, deliberate gap, not an oversight.
                TranscriptEvent::Error(error) => {
                    tracing::warn!(%error, stream_id = %task_stream_id, "voice transcription failed mid-stream");
                    break;
                }
            }
        }
    });
    ActiveVoiceStream {
        stream_id,
        audio_tx: Some(tx),
        cancel,
        task,
    }
}

async fn stop_voice_stream(active: &mut Option<ActiveVoiceStream>) {
    let Some(mut stream) = active.take() else {
        return;
    };
    stream.audio_tx.take();
    if tokio::time::timeout(Duration::from_secs(5), &mut stream.task)
        .await
        .is_err()
    {
        stream.cancel.cancel();
        let _ = stream.task.await;
    }
}

/// `GET /ws/v1` — authenticated WebSocket upgrade (the bearer middleware has
/// already validated the device when this runs).
///
/// A browser's native `WebSocket` constructor cannot set an `Authorization`
/// header on the handshake request, so `require_device` accepts the device
/// token as a WS subprotocol instead, behind the `WS_DEVICE_TOKEN_PROTOCOL`
/// sentinel (`crate::auth::ws_subprotocol_token`): a browser opens the
/// socket with `new WebSocket(url, [WS_DEVICE_TOKEN_PROTOCOL, token])`. The
/// handshake only *completes* if the server selects one of the offered
/// subprotocols, so the sentinel — and only the sentinel, never the token —
/// is echoed back here to complete it. Echoing the token itself would put a
/// bearer secret in a response header no log/proxy redaction list expects
/// (unlike `Authorization`); the sentinel carries no authority on its own.
pub async fn ws_upgrade(
    State(state): State<WsState>,
    Query(params): Query<WsParams>,
    ws: WebSocketUpgrade,
) -> Response {
    // Absent `since` = live-only from now; `since=0` = replay everything (outbox
    // ids start at 1 and the filter is `id > since`); a negative value clamps to
    // a full replay rather than being rejected.
    let since = params.since.map(|s| s.max(0));
    // `requested_protocols()` is a `BTreeSet` internally (sorted, not
    // offer-order), so this is a presence check only — the sentinel's
    // *position* (must be offered first) is enforced order-sensitively in
    // `auth::ws_subprotocol_token`, which reads the raw header directly
    // rather than through this extractor.
    let offered_token_protocol = ws
        .requested_protocols()
        .any(|p| p.as_bytes() == crate::auth::WS_DEVICE_TOKEN_PROTOCOL.as_bytes());
    // Run control remains REST-only, but voice control frames and bounded PCM
    // chunks are legitimate inbound messages (docs/05 §1).
    let mut ws = ws
        .max_message_size(MAX_INBOUND_FRAME_BYTES)
        .max_frame_size(MAX_INBOUND_FRAME_BYTES);
    if offered_token_protocol {
        ws.set_selected_protocol(HeaderValue::from_static(
            crate::auth::WS_DEVICE_TOKEN_PROTOCOL,
        ));
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state, since))
}

async fn handle_socket(mut socket: WebSocket, state: WsState, since: Option<i64>) {
    // Subscribe BEFORE replaying so no live event slips through the gap. Any
    // overlap between replay and live is deduped by the client on `seq` (the
    // outbox id is unique and monotonic).
    let mut rx = state.hub.subscribe();
    let mut voice_stream: Option<ActiveVoiceStream> = None;

    if let Some(since) = since
        && replay_since(&mut socket, &state, since).await.is_err()
    {
        return; // client gone (or replay failed → it can REST-resync)
    }

    loop {
        tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => {
                stop_voice_stream(&mut voice_stream).await;
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            received = rx.recv() => match received {
                Ok(envelope) => {
                    if send_envelope(&mut socket, &envelope).await.is_err() {
                        stop_voice_stream(&mut voice_stream).await;
                        return;
                    }
                }
                // Too far behind: close so the client reconnects and resyncs
                // (persisted events are recovered via `?since=` / REST).
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    stop_voice_stream(&mut voice_stream).await;
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    stop_voice_stream(&mut voice_stream).await;
                    return;
                }
            },
            // Inbound voice frames are the one exception to REST-only commands;
            // run control remains on the audited REST surface.
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Close(_))) | None => {
                    stop_voice_stream(&mut voice_stream).await;
                    return;
                }
                Some(Ok(Message::Text(text))) => {
                    let Ok(control) = serde_json::from_str::<jarvis_contracts::voice::VoiceControlDto>(&text) else {
                        continue;
                    };
                    match control {
                        jarvis_contracts::voice::VoiceControlDto::StreamStart {
                            stream_id,
                            sample_rate_hz,
                            sample_width_bytes,
                            channels,
                            ..
                        } => {
                            stop_voice_stream(&mut voice_stream).await;
                            if let Some(transcriber) = &state.transcriber {
                                let cancel = state.shutdown.child_token();
                                voice_stream = Some(start_voice_stream(
                                    Arc::clone(transcriber),
                                    Arc::clone(&state.hub),
                                    stream_id,
                                    AudioFormat {
                                        sample_rate_hz,
                                        sample_width_bytes,
                                        channels,
                                    },
                                    cancel,
                                ));
                            }
                        }
                        jarvis_contracts::voice::VoiceControlDto::StreamStop { stream_id } => {
                            if voice_stream
                                .as_ref()
                                .is_some_and(|active| active.stream_id == stream_id)
                            {
                                stop_voice_stream(&mut voice_stream).await;
                            }
                        }
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    if let Some(tx) = voice_stream.as_mut().and_then(|active| active.audio_tx.as_ref()) {
                        let _ = tx.send(bytes.to_vec()).await;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => {
                    stop_voice_stream(&mut voice_stream).await;
                    return;
                }
            },
        }
    }
}

/// Replay persisted domain events with `id > since`, paging through the log.
async fn replay_since(socket: &mut WebSocket, state: &WsState, since: i64) -> Result<(), ()> {
    let mut cursor = since;
    loop {
        let rows = match state.events.since(cursor, REPLAY_PAGE).await {
            Ok(rows) => rows,
            // Replay is best-effort; the client can always REST-resync.
            Err(_) => return Ok(()),
        };
        if rows.is_empty() {
            return Ok(());
        }
        let n = rows.len();
        for row in &rows {
            let envelope = state.hub.domain_envelope(row);
            send_envelope(socket, &envelope).await?;
            cursor = row.id;
        }
        if (n as i64) < REPLAY_PAGE {
            return Ok(());
        }
    }
}

async fn send_envelope(socket: &mut WebSocket, envelope: &EventEnvelope) -> Result<(), ()> {
    let text = serde_json::to_string(envelope).expect("envelope serializes");
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

/// `id` (BIGINT, always ≥ 1 for a real row) → `seq`. Negatives cannot occur for
/// an identity column; clamped defensively rather than wrapping.
fn seq_of(id: i64) -> u64 {
    u64::try_from(id).unwrap_or(0)
}

/// A transient event has no stored timestamp — its occurrence *is* now.
fn now_rfc3339() -> String {
    rfc3339(OffsetDateTime::now_utc())
}

fn rfc3339_system_time(at: SystemTime) -> String {
    time::OffsetDateTime::from(at)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).expect("UTC timestamp formats")
}

/// Split a `#[serde(tag = "type")]` value into its discriminator and the
/// remaining fields, so the envelope carries the type and the payload carries
/// only the event's own fields (matching the outbox payload convention).
fn split_tagged(value: serde_json::Value) -> (String, serde_json::Value) {
    match value {
        serde_json::Value::Object(mut map) => {
            let event_type = map
                .remove("type")
                .and_then(|t| t.as_str().map(str::to_owned))
                .unwrap_or_default();
            (event_type, serde_json::Value::Object(map))
        }
        // Typed events always serialize to an object; keep the value as payload.
        other => (String::new(), other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::run::{RunOutcome, RunOutcomeKind, RunState};
    use serde_json::json;

    const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    #[tokio::test]
    async fn publish_builds_a_domain_envelope_carrying_the_outbox_seq() {
        let hub = WsHub::new();
        let mut rx = hub.subscribe();

        hub.publish(&OutboxRecord {
            id: 42,
            event_type: "run.state_changed".to_owned(),
            payload: json!({ "runId": RUN, "state": "model_running" }),
            created_at: OffsetDateTime::UNIX_EPOCH,
        })
        .await
        .unwrap();

        let env = rx.recv().await.unwrap();
        assert_eq!(env.seq, 42);
        // occurredAt reflects the stored commit time, not "now".
        assert_eq!(env.occurred_at, "1970-01-01T00:00:00Z");
        assert_eq!(env.v, CONTRACT_VERSION);
        assert_eq!(env.channel, Channel::Session);
        assert_eq!(env.event_type, "run.state_changed");
        // Payload forwarded verbatim; the type stays on the envelope only.
        assert_eq!(
            env.payload,
            json!({ "runId": RUN, "state": "model_running" })
        );
        assert_eq!(hub.high_water(), 42);
    }

    #[tokio::test]
    async fn sink_broadcasts_deltas_and_drops_state_and_finished() {
        let hub = WsHub::new();
        let mut rx = hub.subscribe();
        let run_id: RunId = RUN.parse().unwrap();

        // State + finished are owned by the outbox path — dropped here.
        hub.emit(RunUpdate::StateChanged {
            run_id: run_id.clone(),
            state: RunState::ModelRunning,
        })
        .await;
        hub.emit(RunUpdate::Finished {
            run_id: run_id.clone(),
            outcome: RunOutcome {
                kind: RunOutcomeKind::Completed,
                detail: None,
            },
        })
        .await;
        // Only the transient delta is broadcast.
        hub.emit(RunUpdate::TextDelta {
            run_id: run_id.clone(),
            text: "hi".to_owned(),
        })
        .await;

        let env = rx.recv().await.unwrap();
        assert_eq!(env.event_type, "text.delta");
        assert_eq!(env.payload, json!({ "runId": RUN, "text": "hi" }));
        assert!(
            rx.try_recv().is_err(),
            "state/finished must not be broadcast"
        );
    }

    #[tokio::test]
    async fn sink_maps_agenda_to_a_sensitivity_safe_hud_card() {
        let hub = WsHub::new();
        let mut rx = hub.subscribe();
        let run_id: RunId = RUN.parse().unwrap();
        let event = jarvis_application::calendar::CalendarEvent::new(
            "Dentist",
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(3_600),
            false,
            jarvis_domain::location::Sensitivity::Sensitive,
        )
        .unwrap();

        hub.emit(RunUpdate::Agenda {
            run_id,
            events: vec![event],
        })
        .await;

        let envelope = rx.recv().await.unwrap();
        assert_eq!(envelope.event_type, "hud.canvas");
        assert_eq!(
            envelope.payload["canvas"]["cards"][0]["type"],
            "card.agenda"
        );
        assert_eq!(
            envelope.payload["canvas"]["cards"][0]["events"][0],
            json!({
                "title": "Dentist",
                "start": "1970-01-01T00:00:00Z",
                "end": "1970-01-01T01:00:00Z",
                "allDay": false
            })
        );
        assert!(!envelope.payload.to_string().contains("sensitivity"));
    }

    #[test]
    fn split_tagged_moves_the_type_out_of_the_payload() {
        let (event_type, payload) =
            split_tagged(json!({ "type": "text.delta", "runId": RUN, "text": "x" }));
        assert_eq!(event_type, "text.delta");
        assert_eq!(payload, json!({ "runId": RUN, "text": "x" }));
    }

    struct FakeTranscriber;

    #[async_trait]
    impl SpeechTranscriber for FakeTranscriber {
        fn id(&self) -> &str {
            "fake-stt"
        }

        async fn transcribe(
            &self,
            mut audio: BoxStream<'static, Vec<u8>>,
            _format: AudioFormat,
            _cancel: CancellationToken,
        ) -> Result<BoxStream<'static, TranscriptEvent>, jarvis_application::voice::VoiceError>
        {
            while audio.next().await.is_some() {}
            Ok(Box::pin(futures_util::stream::iter([
                TranscriptEvent::Partial("hello".to_owned()),
                TranscriptEvent::Final("hello Jarvis".to_owned()),
            ])))
        }
    }

    #[tokio::test]
    async fn voice_stream_routes_pcm_to_the_transcriber_and_broadcasts_hypotheses() {
        let hub = WsHub::new();
        let mut rx = hub.subscribe();
        let mut active = Some(start_voice_stream(
            Arc::new(FakeTranscriber),
            Arc::clone(&hub),
            "stream-1".to_owned(),
            AudioFormat {
                sample_rate_hz: 16_000,
                sample_width_bytes: 2,
                channels: 1,
            },
            CancellationToken::new(),
        ));
        active
            .as_ref()
            .unwrap()
            .audio_tx
            .as_ref()
            .unwrap()
            .send(vec![0, 1, 2, 3])
            .await
            .unwrap();
        stop_voice_stream(&mut active).await;

        let partial = rx.recv().await.unwrap();
        let final_event = rx.recv().await.unwrap();
        assert_eq!(partial.channel, Channel::Voice);
        assert_eq!(partial.event_type, "voice.transcript");
        assert_eq!(
            partial.payload,
            json!({
                "streamId": "stream-1",
                "text": "hello",
                "final": false,
            })
        );
        assert_eq!(final_event.payload["final"], json!(true));
        assert_eq!(final_event.payload["text"], json!("hello Jarvis"));
    }

    #[test]
    fn seq_clamps_a_nonpositive_id() {
        assert_eq!(seq_of(7), 7);
        assert_eq!(seq_of(0), 0);
        assert_eq!(seq_of(-1), 0);
    }
}
