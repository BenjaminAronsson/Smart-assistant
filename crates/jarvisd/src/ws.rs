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
use axum::Extension;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderValue;
use axum::response::Response;
use futures_util::stream::{BoxStream, StreamExt, poll_fn};
use jarvis_application::orchestrator::{RunEventSink, RunUpdate};
use jarvis_application::ports::{DisplayDirectiveSink, RepositoryError};
use jarvis_application::voice::{
    AudioFormat, ClauseSegmenter, SpeechSynthesizer, SpeechTranscriber, TranscriptEvent, VoiceError,
};
use jarvis_contracts::CONTRACT_VERSION;
use jarvis_contracts::cards::{AgendaEventDto, HudCardDto};
use jarvis_contracts::deepdive::{CanvasActionDto, HudCanvasDto};
use jarvis_contracts::display::{DisplayDirective, SurfaceDto};
use jarvis_contracts::envelope::{Channel, EventEnvelope};
use jarvis_contracts::events::TransientEvent;
use jarvis_contracts::voice::{VoiceControlDto, VoiceErrorCodeDto, VoiceSpeakEndDto};
use jarvis_domain::display::Surface;
use jarvis_domain::ids::{RunId, SessionId};
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

/// Outbound synthesized-audio frame ceiling. A Wyoming `audio-chunk` is normally
/// a few KB, but the adapter tolerates up to `MAX_PAYLOAD_BYTES` (4 MiB), so the
/// socket re-chunks rather than emitting one frame a client (or an intermediary)
/// might refuse. Kept below [`MAX_INBOUND_FRAME_BYTES`] so both directions of the
/// voice channel obey the same order of magnitude.
const MAX_OUTBOUND_AUDIO_FRAME_BYTES: usize = 32 * 1024;

/// Clauses that may sit queued for the synthesizer before backpressure is a real
/// problem rather than jitter. A model answer of 64 unspoken clauses means
/// synthesis has fallen hopelessly behind the response; the socket loop must
/// never *block* on this channel (it also carries every other client's events),
/// so an overflow is reported as a TTS failure instead of being waited out or
/// silently dropped.
const CLAUSE_QUEUE_CAPACITY: usize = 64;

/// PCM chunks buffered between the synthesis task and the socket loop.
const AUDIO_QUEUE_CAPACITY: usize = 32;

/// Bound on how long a cancelled synthesis task is awaited before the socket
/// loop stops waiting for it. Barge-in must be prompt (docs/02 §9: TTS "stops
/// immediately on barge-in"), and the task is already detached from the audio
/// path by then — the receiver is dropped, so it can emit nothing further.
const SPEECH_CANCEL_GRACE: Duration = Duration::from_millis(250);

/// Bound on how long a capture stream's transcription task is awaited after end
/// of speech (`voice.stream.stop`, barge-in, socket close, shutdown). Closing
/// the audio channel is what makes the STT service settle the utterance, so the
/// task is given room to produce it — but the settled turn reaches the socket
/// loop through the `finals` queue and the hub, never through this await, so the
/// bound is a pure liveness guard.
const VOICE_STREAM_SETTLE_GRACE: Duration = Duration::from_secs(5);

/// Bound on how long a **cancelled** transcription task is awaited before it is
/// abandoned. Same reasoning as [`SPEECH_CANCEL_GRACE`]: cancellation is already
/// signalled and the audio path is already severed, so the socket loop — which
/// also carries every other event for this client — must not stall on a slow
/// speech service winding down. Without this bound a task that cannot observe
/// its token (one blocked on a full `finals` queue, say) wedges the socket
/// permanently: no inbound frames, no outbound events, and no graceful drain.
const VOICE_STREAM_CANCEL_GRACE: Duration = Duration::from_millis(250);

/// Ceiling on a client-supplied `streamId`. The id is echoed into every
/// `voice.transcript`/`voice.error` envelope, and the hub **broadcasts those to
/// every connected socket** — so an id bounded only by the 64 KiB frame cap is
/// an amplification lever: one `voice.stream.start` becomes one oversized copy
/// per partial transcript per connected client. A real id is a short opaque
/// handle.
const MAX_STREAM_ID_CHARS: usize = 64;

/// The PCM format this daemon accepts on `voice.stream.start`.
/// `Config::validate` pins `[voice].audio` to `s16le` with a positive rate and
/// channel count, but that constrains the *daemon's* configuration only: the
/// per-stream format on this frame is client-controlled and is forwarded
/// verbatim to the speech service, so it is validated here rather than trusted.
/// A mismatch is rejected rather than coerced, so the audio the service receives
/// stays exactly the audio the client said it was sending.
const VOICE_SAMPLE_WIDTH_BYTES: u16 = 2; // s16le
const VOICE_MAX_CHANNELS: u16 = 2;
const VOICE_MIN_SAMPLE_RATE_HZ: u32 = 8_000;
const VOICE_MAX_SAMPLE_RATE_HZ: u32 = 48_000;

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

    /// Broadcast a voice-pipeline failure (F5.2). Without this the browser sees
    /// only the absence of a transcript, which is indistinguishable from the
    /// user having said nothing — the dishonest failure state
    /// [`jarvis_application::voice::TranscriptEvent::Error`] exists to prevent.
    /// Only the stable code crosses the wire, never the adapter's message.
    fn broadcast_voice_error(&self, stream_id: &str, code: VoiceErrorCodeDto) {
        let event = TransientEvent::VoiceError {
            stream_id: stream_id.to_owned(),
            code,
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
    /// Optional M5 TTS adapter (F5.2). `None` means the round trip still works
    /// end to end — transcript, run, streamed text — it is simply not spoken.
    pub synthesizer: Option<Arc<dyn SpeechSynthesizer>>,
    /// The run surface, so a **final transcript starts a run through exactly the
    /// path typed text takes** (`RunApi::start_turn`). `None` in deployments/
    /// tests that mount the socket without the run surface: the transcript is
    /// still broadcast, but nothing is started.
    pub runs: Option<crate::runs::RunApi>,
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

/// Whether a client-supplied `streamId` may be adopted — and therefore echoed
/// into broadcast envelopes (see [`MAX_STREAM_ID_CHARS`]). Bounded, non-empty,
/// and free of control characters, which have no place in an opaque handle and
/// could corrupt a client's rendering of it. Rejected rather than truncated: a
/// truncated id still echoes attacker-chosen bytes and silently renames the
/// stream out from under the client that opened it.
fn stream_id_is_acceptable(stream_id: &str) -> bool {
    !stream_id.is_empty()
        && stream_id.chars().count() <= MAX_STREAM_ID_CHARS
        && !stream_id.chars().any(char::is_control)
}

/// The client's declared capture format, or `None` when it is not one this
/// daemon will forward to a speech service (see [`VOICE_SAMPLE_WIDTH_BYTES`]).
fn accepted_audio_format(
    sample_rate_hz: u32,
    sample_width_bytes: u16,
    channels: u16,
) -> Option<AudioFormat> {
    let acceptable = sample_width_bytes == VOICE_SAMPLE_WIDTH_BYTES
        && (1..=VOICE_MAX_CHANNELS).contains(&channels)
        && (VOICE_MIN_SAMPLE_RATE_HZ..=VOICE_MAX_SAMPLE_RATE_HZ).contains(&sample_rate_hz);
    acceptable.then_some(AudioFormat {
        sample_rate_hz,
        sample_width_bytes,
        channels,
    })
}

/// Map an adapter-side failure to the stable wire code for its leg. The
/// adapter's own message is deliberately dropped here (it is only ever logged),
/// so no transport text reaches the browser.
fn stt_error_code(error: &VoiceError) -> VoiceErrorCodeDto {
    match error {
        VoiceError::Unavailable(_) => VoiceErrorCodeDto::SttUnavailable,
        VoiceError::Malformed(_) | VoiceError::Cancelled => VoiceErrorCodeDto::SttFailed,
    }
}

fn tts_error_code(error: &VoiceError) -> VoiceErrorCodeDto {
    match error {
        VoiceError::Unavailable(_) => VoiceErrorCodeDto::TtsUnavailable,
        VoiceError::Malformed(_) | VoiceError::Cancelled => VoiceErrorCodeDto::TtsFailed,
    }
}

/// A settled voice turn on its way from the transcription task to the socket
/// loop. It carries its own session because the capture stream it came from is
/// already torn down by the time the loop sees it (release-to-talk *is* the end
/// of the stream) — looking the session up from the live stream would lose it.
struct VoiceTurn {
    stream_id: String,
    session_id: Option<SessionId>,
    text: String,
}

/// `session_id` is the conversation this push-to-talk turn belongs to, from the
/// `voice.stream.start` frame. Absent ⇒ the transcript is displayed but no run
/// is started: a run needs a session, and inventing one server-side would be a
/// second, weaker way to create conversations than the audited REST endpoint.
fn start_voice_stream(
    transcriber: Arc<dyn SpeechTranscriber>,
    hub: Arc<WsHub>,
    stream_id: String,
    session_id: Option<SessionId>,
    format: AudioFormat,
    cancel: CancellationToken,
    finals: mpsc::Sender<VoiceTurn>,
) -> ActiveVoiceStream {
    let (tx, rx) = mpsc::channel(32);
    let task_stream_id = stream_id.clone();
    let task_session_id = session_id.clone();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        let result = transcriber
            .transcribe(audio_stream(rx), format, task_cancel.clone())
            .await;
        let mut transcript = match result {
            Ok(transcript) => transcript,
            Err(error) => {
                tracing::warn!(%error, stream_id = %task_stream_id, "voice transcription could not start");
                hub.broadcast_voice_error(&task_stream_id, stt_error_code(&error));
                return;
            }
        };
        while let Some(event) = transcript.next().await {
            match event {
                TranscriptEvent::Partial(text) => {
                    hub.broadcast_voice_transcript(&task_stream_id, text, false)
                }
                TranscriptEvent::Final(text) => {
                    hub.broadcast_voice_transcript(&task_stream_id, text.clone(), true);
                    // Hand the settled transcript to the socket loop, which owns
                    // the authenticated device identity and therefore the only
                    // path that may start a run. One final per turn: a service
                    // emitting more would otherwise be able to start unbounded
                    // runs from a single button press.
                    //
                    // Handed over WITHOUT waiting, deliberately. `finals` is
                    // bounded, and the socket loop is not always draining it —
                    // it is, for instance, inside this very stream's teardown
                    // awaiting this task. A blocking `send` there is an
                    // unbounded await that no `CancellationToken` can reach
                    // (cancelling a token does not interrupt a blocked `send`),
                    // which is exactly the "not cancellable" case invariant #4
                    // forbids: it wedged the socket loop permanently — no
                    // inbound frames, no outbound events, no graceful drain.
                    //
                    // A full queue means the loop is already four settled turns
                    // behind, which push-to-talk cannot reach without
                    // pipelining. The transcript itself was broadcast above, so
                    // the user still sees what was heard; only the run is not
                    // started, and that is reported rather than waited out.
                    let turn = VoiceTurn {
                        stream_id: task_stream_id.clone(),
                        session_id: task_session_id.clone(),
                        text,
                    };
                    if let Err(error) = finals.try_send(turn) {
                        tracing::warn!(
                            stream_id = %task_stream_id,
                            reason = match error {
                                mpsc::error::TrySendError::Full(_) => "queue full",
                                mpsc::error::TrySendError::Closed(_) => "socket closed",
                            },
                            "settled voice transcript starts no run"
                        );
                    }
                    break;
                }
                // A mid-stream STT failure. A stream that simply ends means the
                // service finished normally, so this must surface as its own
                // event — otherwise a dead STT service is indistinguishable from
                // silence to the user (F5.1's `TranscriptEvent::Error` doc).
                TranscriptEvent::Error(error) => {
                    tracing::warn!(%error, stream_id = %task_stream_id, "voice transcription failed mid-stream");
                    hub.broadcast_voice_error(&task_stream_id, stt_error_code(&error));
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

/// End one capture stream: close its audio channel (end of speech), then stop
/// waiting for its transcription task within a bounded, cancellable staircase.
///
/// Every wait here is bounded. Dropping the audio sender is what lets the STT
/// service settle the utterance, so the task gets [`VOICE_STREAM_SETTLE_GRACE`]
/// to finish normally; past that it is cancelled, given
/// [`VOICE_STREAM_CANCEL_GRACE`] to unwind, and finally aborted — the same
/// escalation [`cancel_speech`] applies to synthesis. The second wait used to be
/// an unbounded `task.await`, which meant a task that could not observe its
/// token wedged the socket loop for good: no inbound frames, no outbound events,
/// and the `state.shutdown` branch never reached, so graceful drain never
/// completed for that connection.
async fn stop_voice_stream(active: &mut Option<ActiveVoiceStream>) {
    let Some(mut stream) = active.take() else {
        return;
    };
    stream.audio_tx.take();
    if tokio::time::timeout(VOICE_STREAM_SETTLE_GRACE, &mut stream.task)
        .await
        .is_err()
    {
        stream.cancel.cancel();
        if tokio::time::timeout(VOICE_STREAM_CANCEL_GRACE, &mut stream.task)
            .await
            .is_err()
        {
            tracing::warn!(
                stream_id = %stream.stream_id,
                "voice transcription task did not settle after cancellation; abandoning it"
            );
            stream.task.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Spoken response leg (F5.2, docs/02 §9)
// ---------------------------------------------------------------------------

/// One item on the synthesis task → socket-loop path. The task never touches the
/// `WebSocket` (the socket loop owns it exclusively); it reports what it
/// produced and the loop decides what reaches the client.
enum SpeechChunk {
    /// The first clause synthesized; carries the negotiated PCM format.
    Started(AudioFormat),
    Audio(Vec<u8>),
    Ended(VoiceSpeakEndDto, Option<VoiceErrorCodeDto>),
}

/// Spoken output for one run's response, in flight on this socket.
struct ActiveSpeech {
    utterance_id: String,
    run_id: RunId,
    /// Cancelled on barge-in / socket close / shutdown. A child of the socket's
    /// token, so every ancestor cancellation reaches it (invariant #4) — this is
    /// the existing `CancellationToken` plumbing, not a second mechanism.
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    /// `None` once the response finished and the queue was closed.
    clauses: Option<mpsc::Sender<String>>,
    audio: mpsc::Receiver<SpeechChunk>,
    segmenter: ClauseSegmenter,
    announced: bool,
}

/// Drive synthesis for one utterance: clauses in, PCM out, strictly in order.
///
/// Sequential by construction — clause N+1 is not synthesized until clause N's
/// audio has been handed over — because spoken output that arrives out of order
/// is worse than spoken output that arrives late.
async fn speak_task(
    synthesizer: Arc<dyn SpeechSynthesizer>,
    mut clauses: mpsc::Receiver<String>,
    out: mpsc::Sender<SpeechChunk>,
    cancel: CancellationToken,
) {
    let mut announced = false;
    let mut ended = VoiceSpeakEndDto::Completed;
    let mut failure: Option<VoiceErrorCodeDto> = None;

    'utterance: loop {
        let clause = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            clause = clauses.recv() => clause,
        };
        let Some(clause) = clause else { break };

        let (format, mut pcm) = match synthesizer.synthesize(&clause, cancel.clone()).await {
            Ok(started) => started,
            Err(VoiceError::Cancelled) => {
                ended = VoiceSpeakEndDto::Cancelled;
                break;
            }
            Err(error) => {
                tracing::warn!(%error, "speech synthesis could not start");
                ended = VoiceSpeakEndDto::Failed;
                failure = Some(tts_error_code(&error));
                break;
            }
        };
        if !announced {
            if out.send(SpeechChunk::Started(format)).await.is_err() {
                return; // socket loop gone; nothing left to report to
            }
            announced = true;
        }

        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    ended = VoiceSpeakEndDto::Cancelled;
                    break 'utterance;
                }
                next = pcm.next() => next,
            };
            match next {
                Some(Ok(bytes)) => {
                    if out.send(SpeechChunk::Audio(bytes)).await.is_err() {
                        return;
                    }
                }
                // A truncated utterance is reported, never passed off as a
                // complete one (the `Result` chunk exists for exactly this).
                Some(Err(error)) => {
                    tracing::warn!(%error, "speech synthesis failed mid-utterance");
                    ended = VoiceSpeakEndDto::Failed;
                    failure = Some(tts_error_code(&error));
                    break 'utterance;
                }
                None => break, // this clause finished; go on to the next
            }
        }
    }

    if cancel.is_cancelled() {
        ended = VoiceSpeakEndDto::Cancelled;
        failure = None;
    }
    let _ = out.send(SpeechChunk::Ended(ended, failure)).await;
}

fn begin_speech(
    synthesizer: Arc<dyn SpeechSynthesizer>,
    run_id: RunId,
    cancel: CancellationToken,
) -> ActiveSpeech {
    let (clause_tx, clause_rx) = mpsc::channel(CLAUSE_QUEUE_CAPACITY);
    let (audio_tx, audio_rx) = mpsc::channel(AUDIO_QUEUE_CAPACITY);
    let task = tokio::spawn(speak_task(synthesizer, clause_rx, audio_tx, cancel.clone()));
    ActiveSpeech {
        // Opaque, per-utterance: the client uses it only to discard audio that
        // belongs to an utterance it has already been told ended.
        utterance_id: ulid::Ulid::new().to_string(),
        run_id,
        cancel,
        task,
        clauses: Some(clause_tx),
        audio: audio_rx,
        segmenter: ClauseSegmenter::new(),
        announced: false,
    }
}

/// Await the next chunk of the in-flight utterance, or park forever when nothing
/// is being spoken (so the socket's `select!` has a branch it can always poll).
async fn next_speech_chunk(speech: &mut Option<ActiveSpeech>) -> Option<SpeechChunk> {
    match speech {
        Some(active) => active.audio.recv().await,
        None => std::future::pending().await,
    }
}

/// Stop the in-flight utterance **now** (barge-in, socket close, shutdown).
///
/// Taking the [`ActiveSpeech`] out of the socket loop drops its receiver, so the
/// loop structurally cannot emit another audio frame for that utterance no
/// matter what the synthesis task does next; cancelling the token then aborts
/// the synthesis stream at the adapter. The task is awaited only briefly — it is
/// already detached from the audio path — so barge-in is not gated on a slow
/// TTS service winding down.
async fn cancel_speech(
    socket: &mut WebSocket,
    hub: &WsHub,
    speech: &mut Option<ActiveSpeech>,
) -> Result<(), ()> {
    let Some(mut active) = speech.take() else {
        return Ok(());
    };
    active.cancel.cancel();
    drop(active.clauses.take());
    if tokio::time::timeout(SPEECH_CANCEL_GRACE, &mut active.task)
        .await
        .is_err()
    {
        // Cancellation is signalled and the audio path is severed; leaving the
        // task to unwind on its own is bounded by the adapter's own cancellation
        // handling, and the socket must not stall waiting for it.
        active.task.abort();
    }
    if active.announced {
        send_speak_control(
            socket,
            hub,
            &VoiceControlDto::SpeakStop {
                utterance_id: active.utterance_id,
                reason: VoiceSpeakEndDto::Cancelled,
            },
        )
        .await?;
    }
    Ok(())
}

/// Feed one broadcast envelope into the in-flight utterance: text deltas for the
/// spoken run become clauses; its terminal event closes the clause queue.
///
/// Reading the run's text off the socket's own subscription is deliberate — the
/// response is already on this stream, so no second sink, no second copy of the
/// answer, and no coupling from the run engine to the voice channel.
fn feed_speech(speech: &mut Option<ActiveSpeech>, envelope: &EventEnvelope) -> Result<(), ()> {
    let Some(active) = speech.as_mut() else {
        return Ok(());
    };
    if envelope.payload["runId"].as_str() != Some(active.run_id.as_str()) {
        return Ok(());
    }
    match envelope.event_type.as_str() {
        "text.delta" => {
            let Some(text) = envelope.payload["text"].as_str() else {
                return Ok(());
            };
            let clauses = active.segmenter.push(text);
            let Some(sender) = active.clauses.as_ref() else {
                return Ok(());
            };
            for clause in clauses {
                // Never block the socket loop on the synthesizer: this task also
                // carries every other client event. A full queue means synthesis
                // has fallen hopelessly behind, which is a failure to report,
                // not a wait to absorb or a clause to silently drop.
                if sender.try_send(clause).is_err() {
                    return Err(());
                }
            }
        }
        // Terminal for the spoken response, either because the run finished or
        // because it parked in degraded mode: in both cases no further text is
        // coming, so what has been buffered is spoken and the queue closes
        // rather than the utterance hanging open until the socket dies.
        "run.completed" | "run.queued" | "degraded.queued" => {
            if let Some(sender) = active.clauses.as_ref()
                && let Some(tail) = active.segmenter.flush()
                && sender.try_send(tail).is_err()
            {
                return Err(());
            }
            // Closing the queue is what tells the synthesis task the response is
            // complete; it drains what is queued, then reports `Completed`.
            drop(active.clauses.take());
        }
        _ => {}
    }
    Ok(())
}

/// Send a `voice.speak.*` control frame. Server→client text frames are always
/// envelopes (docs/05 §3), so the `VoiceControlDto` tag rides the envelope
/// `type` exactly like a transient event's.
async fn send_speak_control(
    socket: &mut WebSocket,
    hub: &WsHub,
    control: &VoiceControlDto,
) -> Result<(), ()> {
    let (event_type, payload) =
        split_tagged(serde_json::to_value(control).expect("voice control serializes"));
    let envelope = EventEnvelope {
        v: CONTRACT_VERSION,
        seq: hub.high_water(),
        channel: Channel::Voice,
        event_type,
        occurred_at: now_rfc3339(),
        trace_id: None,
        resource_version: None,
        payload,
    };
    send_envelope(socket, &envelope).await
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
    // The device this socket authenticated as (inserted by `require_device`,
    // which every `/ws/v1` upgrade passes through). Carried into the socket task
    // because a voice turn started here must acquire **exactly** the
    // authorization context a typed message from the same device would — a run
    // spawned without an attributable device is deliberately given no policy
    // context at all (CF-15 fail-closed, `runs::RunEngine::spawn`), and a voice
    // transcript must not be the one input that quietly lands in that state.
    Extension(device): Extension<crate::auth::DeviceContext>,
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
    ws.on_upgrade(move |socket| handle_socket(socket, state, since, device))
}

async fn handle_socket(
    mut socket: WebSocket,
    state: WsState,
    since: Option<i64>,
    device: crate::auth::DeviceContext,
) {
    // Subscribe BEFORE replaying so no live event slips through the gap. Any
    // overlap between replay and live is deduped by the client on `seq` (the
    // outbox id is unique and monotonic).
    let mut rx = state.hub.subscribe();
    let mut voice_stream: Option<ActiveVoiceStream> = None;
    let mut speech: Option<ActiveSpeech> = None;
    // Settled transcripts travel task → loop, because only the loop holds the
    // authenticated device identity a run must be attributed to.
    let (finals_tx, mut finals_rx) = mpsc::channel::<VoiceTurn>(4);

    if let Some(since) = since
        && replay_since(&mut socket, &state, since).await.is_err()
    {
        return; // client gone (or replay failed → it can REST-resync)
    }

    macro_rules! shut_down {
        () => {{
            stop_voice_stream(&mut voice_stream).await;
            let _ = cancel_speech(&mut socket, &state.hub, &mut speech).await;
            return;
        }};
    }

    loop {
        tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => {
                stop_voice_stream(&mut voice_stream).await;
                let _ = cancel_speech(&mut socket, &state.hub, &mut speech).await;
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            // A settled transcript. It becomes a run through `RunApi::start_turn`
            // — the same use case `POST /sessions/{id}/messages` calls — so it
            // gets M4's deterministic-grammar-first routing, the policy context
            // of *this* authenticated device, and no shortcut of any kind
            // (invariant #1). Awaited inline rather than spawned so the run is
            // durably created before the loop resumes: the deltas it will emit
            // are already buffered on `rx`, so binding speech to it here cannot
            // miss the start of the answer.
            //
            // Polled BEFORE the inbound and fan-out arms, deliberately.
            // `finals` is BOUNDED, and `biased` means an always-ready arm
            // starves the ones after it: with this arm last, a client that
            // pipelines frames — or a busy fan-out — kept the loop permanently
            // occupied and the queue permanently full, so the transcription
            // tasks feeding it had nowhere to put their results. Work the user
            // has already committed to (an utterance that is *finished*) drains
            // ahead of work that is only just arriving.
            Some(turn) = finals_rx.recv() => {
                if start_voice_turn(&mut socket, &state, &device, &mut speech, turn).await.is_err() {
                    shut_down!();
                }
            }
            received = rx.recv() => match received {
                Ok(envelope) => {
                    if send_envelope(&mut socket, &envelope).await.is_err() {
                        shut_down!();
                    }
                    // The spoken response is assembled from the run's own text
                    // deltas as they pass through this socket (F5.2).
                    if feed_speech(&mut speech, &envelope).is_err() {
                        tracing::warn!("speech clause queue overflowed; cancelling the utterance");
                        state.hub.broadcast_voice_error(
                            speech.as_ref().map(|s| s.utterance_id.as_str()).unwrap_or_default(),
                            VoiceErrorCodeDto::TtsFailed,
                        );
                        if cancel_speech(&mut socket, &state.hub, &mut speech).await.is_err() {
                            shut_down!();
                        }
                    }
                }
                // Too far behind: close so the client reconnects and resyncs
                // (persisted events are recovered via `?since=` / REST).
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    stop_voice_stream(&mut voice_stream).await;
                    let _ = cancel_speech(&mut socket, &state.hub, &mut speech).await;
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => shut_down!(),
            },
            // Inbound voice frames are the one exception to REST-only commands;
            // run control remains on the audited REST surface.
            //
            // Polled BEFORE the outbound speech arms, deliberately: `biased`
            // means an always-ready arm starves the ones after it, and a
            // faster-than-realtime synthesizer keeps the audio channel
            // permanently ready. With the order reversed, the very frame that
            // triggers barge-in could never be read while audio was flowing —
            // the one case this feature exists to handle.
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Close(_))) | None => shut_down!(),
                Some(Ok(Message::Text(text))) => {
                    let Ok(control) = serde_json::from_str::<VoiceControlDto>(&text) else {
                        continue;
                    };
                    match control {
                        VoiceControlDto::StreamStart {
                            stream_id,
                            session_id,
                            sample_rate_hz,
                            sample_width_bytes,
                            channels,
                        } => {
                            // Validated BEFORE the frame is allowed to do
                            // anything at all: a malformed `voice.stream.start`
                            // is not a barge-in, so it must not cancel the
                            // answer currently being spoken either.
                            //
                            // No `voice.error` is emitted for a rejected id: that
                            // event carries the very `streamId` under suspicion,
                            // and broadcasting it to every connected socket is
                            // the amplification this check exists to prevent. It
                            // is logged by length only, and the frame is dropped
                            // like any other unparseable one above — the browser
                            // already fails closed on the absence of a
                            // transcript.
                            if !stream_id_is_acceptable(&stream_id) {
                                tracing::warn!(
                                    stream_id_len = stream_id.len(),
                                    "rejected voice.stream.start: unacceptable streamId"
                                );
                                continue;
                            }
                            // The per-stream format is client-controlled and is
                            // handed straight to the speech service; the
                            // `[voice].audio` config constrains only what the
                            // daemon itself is set up for, so it is checked here.
                            let Some(format) = accepted_audio_format(
                                sample_rate_hz,
                                sample_width_bytes,
                                channels,
                            ) else {
                                tracing::warn!(
                                    %stream_id,
                                    sample_rate_hz,
                                    sample_width_bytes,
                                    channels,
                                    "rejected voice.stream.start: unsupported capture format"
                                );
                                continue;
                            };
                            // BARGE-IN (docs/02 §9: TTS "stops immediately on
                            // barge-in"). The user speaking again supersedes the
                            // answer being spoken, so synthesis is cancelled
                            // here — before any audio of the new turn is even
                            // read — through the existing cancellation token
                            // (invariant #4), not a new mechanism.
                            if cancel_speech(&mut socket, &state.hub, &mut speech).await.is_err() {
                                shut_down!();
                            }
                            stop_voice_stream(&mut voice_stream).await;
                            // An unparseable session id confers nothing: the
                            // turn is transcribed and displayed, but no run is
                            // started against a session that does not exist.
                            let session_id = session_id.and_then(|id| id.parse::<SessionId>().ok());
                            if let Some(transcriber) = &state.transcriber {
                                let cancel = state.shutdown.child_token();
                                voice_stream = Some(start_voice_stream(
                                    Arc::clone(transcriber),
                                    Arc::clone(&state.hub),
                                    stream_id,
                                    session_id,
                                    format,
                                    cancel,
                                    finals_tx.clone(),
                                ));
                            }
                        }
                        VoiceControlDto::StreamStop { stream_id } => {
                            if voice_stream
                                .as_ref()
                                .is_some_and(|active| active.stream_id == stream_id)
                            {
                                stop_voice_stream(&mut voice_stream).await;
                            }
                        }
                        // Speak frames are daemon→client only; a client that
                        // sends one is ignored rather than obeyed — nothing on
                        // the inbound path may cause the daemon to speak.
                        VoiceControlDto::SpeakStart { .. } | VoiceControlDto::SpeakStop { .. } => {}
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    if let Some(tx) = voice_stream.as_mut().and_then(|active| active.audio_tx.as_ref()) {
                        let _ = tx.send(bytes.to_vec()).await;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => shut_down!(),
            },
            chunk = next_speech_chunk(&mut speech) => {
                if forward_speech_chunk(&mut socket, &state, &mut speech, chunk).await.is_err() {
                    shut_down!();
                }
            }
        }
    }
}

/// Turn a settled transcript into a run, then bind spoken output to it.
async fn start_voice_turn(
    socket: &mut WebSocket,
    state: &WsState,
    device: &crate::auth::DeviceContext,
    speech: &mut Option<ActiveSpeech>,
    turn: VoiceTurn,
) -> Result<(), ()> {
    // This turn supersedes whatever was still being spoken, so the previous
    // utterance is stopped **through `cancel_speech`** before it can be
    // replaced. Dropping an `ActiveSpeech` neither cancels its token nor aborts
    // its task, so a bare overwrite left the old synthesis pulling PCM from the
    // speech service — holding that connection open — and left the client
    // without the `voice.speak.stop` its playback bookkeeping is waiting for.
    // Barge-in does not cover this: it fires at `voice.stream.start`, which is
    // strictly before the previous stream's final is dequeued here.
    cancel_speech(socket, &state.hub, speech).await?;

    let stream_id = turn.stream_id;
    let Some(runs) = state.runs.as_ref() else {
        return Ok(()); // no run surface mounted; transcript display only
    };
    let Some(session_id) = turn.session_id else {
        tracing::debug!(%stream_id, "voice transcript has no session; not starting a run");
        return Ok(());
    };

    let ack = match runs.start_turn(&session_id, device, turn.text).await {
        Ok(ack) => ack,
        Err(error) => {
            tracing::warn!(?error, %stream_id, "voice transcript could not start a run");
            return Ok(());
        }
    };

    if let Some(synthesizer) = state.synthesizer.as_ref() {
        // The utterance's token is a child of the socket's shutdown token, so
        // shutdown, socket loss and barge-in all reach it (invariant #4).
        *speech = Some(begin_speech(
            Arc::clone(synthesizer),
            ack.run_id,
            state.shutdown.child_token(),
        ));
    }
    Ok(())
}

/// Relay one item from the synthesis task to the client.
async fn forward_speech_chunk(
    socket: &mut WebSocket,
    state: &WsState,
    speech: &mut Option<ActiveSpeech>,
    chunk: Option<SpeechChunk>,
) -> Result<(), ()> {
    let Some(active) = speech.as_mut() else {
        return Ok(());
    };
    match chunk {
        Some(SpeechChunk::Started(format)) => {
            active.announced = true;
            let control = VoiceControlDto::SpeakStart {
                utterance_id: active.utterance_id.clone(),
                run_id: Some(active.run_id.as_str().to_owned()),
                sample_rate_hz: format.sample_rate_hz,
                sample_width_bytes: format.sample_width_bytes,
                channels: format.channels,
            };
            send_speak_control(socket, &state.hub, &control).await
        }
        Some(SpeechChunk::Audio(bytes)) => {
            for frame in bytes.chunks(MAX_OUTBOUND_AUDIO_FRAME_BYTES) {
                socket
                    .send(Message::Binary(frame.to_vec().into()))
                    .await
                    .map_err(|_| ())?;
            }
            Ok(())
        }
        // Terminal for this utterance, either way: report it, then forget it.
        Some(SpeechChunk::Ended(reason, failure)) => {
            let utterance_id = active.utterance_id.clone();
            let announced = active.announced;
            *speech = None;
            if let Some(code) = failure {
                state.hub.broadcast_voice_error(&utterance_id, code);
            }
            if announced {
                let control = VoiceControlDto::SpeakStop {
                    utterance_id,
                    reason,
                };
                return send_speak_control(socket, &state.hub, &control).await;
            }
            Ok(())
        }
        // The task ended without a terminal chunk (it only does that when the
        // socket loop is gone); nothing left to speak.
        None => {
            *speech = None;
            Ok(())
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
        let (finals_tx, mut finals_rx) = mpsc::channel(4);
        let mut active = Some(start_voice_stream(
            Arc::new(FakeTranscriber),
            Arc::clone(&hub),
            "stream-1".to_owned(),
            None,
            AudioFormat {
                sample_rate_hz: 16_000,
                sample_width_bytes: 2,
                channels: 1,
            },
            CancellationToken::new(),
            finals_tx,
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
        // The settled transcript is also handed to the socket loop, which is
        // the only place holding the device identity a run may be attributed to.
        assert_eq!(
            finals_rx.recv().await.map(|turn| turn.text).as_deref(),
            Some("hello Jarvis")
        );
    }

    struct BrokenTranscriber;

    #[async_trait]
    impl SpeechTranscriber for BrokenTranscriber {
        fn id(&self) -> &str {
            "broken-stt"
        }

        async fn transcribe(
            &self,
            _audio: BoxStream<'static, Vec<u8>>,
            _format: AudioFormat,
            _cancel: CancellationToken,
        ) -> Result<BoxStream<'static, TranscriptEvent>, VoiceError> {
            Ok(Box::pin(futures_util::stream::iter([
                TranscriptEvent::Error(VoiceError::Unavailable("connect failed".to_owned())),
            ])))
        }
    }

    /// A dead STT service must be distinguishable from silence: without a
    /// `voice.error` event the browser sees only the absence of a transcript.
    #[tokio::test]
    async fn a_broken_stt_service_surfaces_voice_error_rather_than_silence() {
        let hub = WsHub::new();
        let mut rx = hub.subscribe();
        let (finals_tx, mut finals_rx) = mpsc::channel(4);
        let mut active = Some(start_voice_stream(
            Arc::new(BrokenTranscriber),
            Arc::clone(&hub),
            "stream-err".to_owned(),
            None,
            AudioFormat {
                sample_rate_hz: 16_000,
                sample_width_bytes: 2,
                channels: 1,
            },
            CancellationToken::new(),
            finals_tx,
        ));
        stop_voice_stream(&mut active).await;

        let event = rx.recv().await.unwrap();
        assert_eq!(event.channel, Channel::Voice);
        assert_eq!(event.event_type, "voice.error");
        assert_eq!(
            event.payload,
            json!({ "streamId": "stream-err", "code": "voice.stt_unavailable" })
        );
        // No transcript is invented from a failed recognition, so no run starts.
        assert!(finals_rx.recv().await.is_none());
    }

    /// The settled turn is handed over on a **bounded** queue, and the socket
    /// loop is by construction not draining it while it is inside this very
    /// teardown. A handover with nowhere to go must therefore not make the
    /// teardown unbounded — it did, before this test: the loop waited 5 s,
    /// cancelled a token a blocked `send` could not observe, and then awaited
    /// the task forever, wedging the whole connection (no inbound frames, no
    /// outbound events, and the `state.shutdown` branch never polled again).
    ///
    /// The handover must therefore not *wait* at all: the assertion is that the
    /// teardown finishes well inside the settle grace, which fails both for the
    /// original unbounded await (it never returns) and for a merely-bounded
    /// blocking `send` (it would burn the whole grace on every such frame — a
    /// stall a client can trigger at will).
    #[tokio::test]
    async fn a_blocked_transcript_handover_cannot_wedge_the_capture_teardown() {
        let started = std::time::Instant::now();
        let hub = WsHub::new();
        // Capacity 1, already full, and nothing will ever read it: exactly the
        // state a pipelined burst of `voice.stream.start` frames produces.
        let (finals_tx, _finals_rx) = mpsc::channel::<VoiceTurn>(1);
        finals_tx
            .send(VoiceTurn {
                stream_id: "already-queued".to_owned(),
                session_id: None,
                text: "already queued".to_owned(),
            })
            .await
            .unwrap();

        let cancel = CancellationToken::new();
        let mut active = Some(start_voice_stream(
            Arc::new(FakeTranscriber),
            Arc::clone(&hub),
            "stream-wedge".to_owned(),
            None,
            AudioFormat {
                sample_rate_hz: 16_000,
                sample_width_bytes: 2,
                channels: 1,
            },
            cancel.clone(),
            finals_tx,
        ));

        tokio::time::timeout(
            VOICE_STREAM_SETTLE_GRACE + VOICE_STREAM_CANCEL_GRACE + Duration::from_secs(5),
            stop_voice_stream(&mut active),
        )
        .await
        .expect("stopping a capture stream must be bounded even when its handover is blocked");
        assert!(active.is_none());
        assert!(
            started.elapsed() < VOICE_STREAM_SETTLE_GRACE,
            "a blocked handover must not cost the settle grace; took {:?}",
            started.elapsed()
        );
        // The stream settled on its own, so nothing had to be cancelled.
        assert!(!cancel.is_cancelled());
    }

    /// The id is echoed into events the hub broadcasts to **every** connected
    /// socket, so it is bounded at the boundary rather than trusted to be sane.
    #[test]
    fn an_unbounded_or_control_laden_stream_id_is_not_acceptable() {
        assert!(stream_id_is_acceptable("s1"));
        assert!(stream_id_is_acceptable(&"x".repeat(MAX_STREAM_ID_CHARS)));
        assert!(!stream_id_is_acceptable(""));
        assert!(!stream_id_is_acceptable(
            &"x".repeat(MAX_STREAM_ID_CHARS + 1)
        ));
        // Bounded in CHARACTERS, not bytes: a multi-byte id of acceptable
        // length is fine, and a long one is still rejected.
        assert!(stream_id_is_acceptable("é"));
        assert!(!stream_id_is_acceptable(
            &"é".repeat(MAX_STREAM_ID_CHARS + 1)
        ));
        assert!(!stream_id_is_acceptable("has\nnewline"));
        assert!(!stream_id_is_acceptable("has\u{7}bell"));
    }

    /// The per-stream format is client-controlled and is handed straight to the
    /// speech service; only the format the daemon is configured for is accepted.
    #[test]
    fn only_the_configured_capture_format_is_accepted() {
        assert_eq!(
            accepted_audio_format(16_000, 2, 1),
            Some(AudioFormat {
                sample_rate_hz: 16_000,
                sample_width_bytes: 2,
                channels: 1,
            })
        );
        assert!(accepted_audio_format(48_000, 2, 2).is_some());
        // Not s16le.
        assert!(accepted_audio_format(16_000, 4, 1).is_none());
        assert!(accepted_audio_format(16_000, 0, 1).is_none());
        // Nonsense channel counts and rates never reach the speech service.
        assert!(accepted_audio_format(16_000, 2, 0).is_none());
        assert!(accepted_audio_format(16_000, 2, 64).is_none());
        assert!(accepted_audio_format(0, 2, 1).is_none());
        assert!(accepted_audio_format(u32::MAX, 2, 1).is_none());
    }

    #[test]
    fn seq_clamps_a_nonpositive_id() {
        assert_eq!(seq_of(7), 7);
        assert_eq!(seq_of(0), 0);
        assert_eq!(seq_of(-1), 0);
    }
}
