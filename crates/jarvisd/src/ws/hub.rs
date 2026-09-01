use super::replay::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use axum::extract::ws::CloseCode;
use jarvis_application::ports::{IdentityStore, RepositoryError};
use jarvis_application::voice::{SpeechSynthesizer, SpeechTranscriber};
use jarvis_contracts::CONTRACT_VERSION;
use jarvis_contracts::cards::{AgendaEventDto, HudCardDto};
use jarvis_contracts::deepdive::{CanvasActionDto, HudCanvasDto};
use jarvis_contracts::display::{DisplayDirective, SurfaceDto};
use jarvis_contracts::envelope::{Channel, EventEnvelope};
use jarvis_contracts::events::TransientEvent;
use jarvis_contracts::voice::VoiceErrorCodeDto;
use jarvis_domain::display::Surface;
use jarvis_domain::identity::{ClassScope, DeviceClass};
use jarvis_domain::ids::RunId;
use jarvis_infra::dispatcher::OutboxRecord;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Bounded fan-out buffer. A client that falls this far behind is disconnected
/// (`broadcast::Lagged`) and resyncs via REST — never unbounded buffering
/// (low-power / DoS guard). Generous for a single owner's devices.
const CHANNEL_CAPACITY: usize = 1024;

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
    pub(crate) tx: broadcast::Sender<Arc<EventEnvelope>>,
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
    pub(crate) fn domain_envelope(&self, record: &OutboxRecord) -> EventEnvelope {
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
    pub(crate) fn broadcast_domain(&self, record: &OutboxRecord) {
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
    pub(crate) fn broadcast_display(
        &self,
        placement: &jarvis_domain::display::SurfacePlacement,
        target: Option<&str>,
    ) -> bool {
        let directive = DisplayDirective::PlaceSurface {
            surface: surface_dto(placement.surface),
            app_id: placement.surface.app_id().to_owned(),
            monitor: placement.monitor.as_str().to_owned(),
            target_device_id: target.map(ToOwned::to_owned),
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
    pub(crate) fn broadcast_voice_transcript(&self, stream_id: &str, text: String, is_final: bool) {
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
    pub(crate) fn broadcast_voice_error(&self, stream_id: &str, code: VoiceErrorCodeDto) {
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

    /// Ring a timer in the room it was set in (F8.5, FR-33).
    ///
    /// Voice channel and **addressed**, so exactly the node that set the timer
    /// hears it — [`delivers_to`] enforces that. Only the *instruction* travels:
    /// the alert tone is fixed and deterministic, so the node synthesises it
    /// locally rather than having audio pushed at it. That keeps this a small
    /// text frame instead of a per-socket binary stream, and it means a room
    /// still rings when nothing about the voice pipeline is working.
    ///
    /// Returns whether any socket was subscribed — the caller uses that to
    /// decide whether the alarm was actually delivered, or whether it has to
    /// fall back to the daemon's own speaker (ADR-023: an alarm must sound).
    pub fn ring_timer_at(&self, timer: &jarvis_contracts::timers::TimerDto) -> bool {
        let payload = serde_json::to_value(timer).expect("timer dto serializes");
        let envelope = EventEnvelope {
            v: CONTRACT_VERSION,
            seq: self.high_water.load(Ordering::SeqCst),
            channel: Channel::Voice,
            event_type: "timer.fired".to_owned(),
            occurred_at: now_rfc3339(),
            trace_id: None,
            resource_version: None,
            payload,
        };
        self.tx.send(Arc::new(envelope)).is_ok()
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

    pub(crate) fn broadcast_agenda(
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
    pub(crate) fn broadcast_delta(&self, run_id: &RunId, text: &str) {
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

    /// Broadcast the "keep this run's answer in the house" notice (S3,
    /// ADR-033 §4). Same transient semantics as a text delta, and deliberately
    /// sent on the same channel: it has to arrive interleaved with — and ahead
    /// of — the deltas it labels, which a second transport could not guarantee.
    pub(crate) fn broadcast_speech_sensitive(&self, run_id: &RunId) {
        let event = TransientEvent::SpeechSensitive {
            run_id: run_id.clone(),
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

    /// Broadcast the live degraded-mode queue notice. It shares the transient
    /// sequence semantics of token deltas; a reconnect gets the durable run
    /// snapshot and a fresh provider poll instead (FR-12).
    pub(crate) fn broadcast_queued(&self, run_id: &RunId, reason: &str, position: usize) {
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

/// Which envelopes a given connection is allowed to receive (F7.4, **CF-8**).
///
/// # The hole this closes
///
/// Until now the hub sent **every** envelope to **every** authenticated
/// socket, and `replay_since` replayed every outbox row the same way. That was
/// inert while the only device was the owner's own browser, which is why the
/// M2 gate recorded it as dormant and scheduled it "before M7". F7.1 and F7.2
/// are what end the dormancy: there can now be a second device on that socket,
/// and the highest-value payload on the channel is `approval.requested` — it
/// carries the exact effect, the real arguments (a recipient, a message body)
/// and the approval id, which is a decision oracle.
///
/// # The rule
///
/// * **Session** — the owner's channel: runs, messages, approvals, artifacts,
///   memories. Only a device holding `ui`. A satellite has no business
///   knowing what the owner asked, let alone what was proposed on their
///   behalf.
/// * **Display** — surfaces. Devices that present: `display-agent` holders,
///   and the owner's shell (which renders the same canvases). Per-*node*
///   addressing lands in F7.5; this is the class-level gate beneath it.
/// * **Voice** — capture and speech, restricted to the socket the stream
///   belongs to. This is the M5 carry-forward: `broadcast_voice_transcript`
///   fanned live microphone text to every connected socket, which was
///   defensible when every socket was the owner's and is not once a kitchen
///   satellite is listening. An envelope naming a stream reaches only the
///   connection that owns that stream.
///
/// Deliberately a pure function over (envelope, class, owned stream): it is
/// the security-relevant decision in this file, so it is testable as a table
/// without standing up a hub, and it is applied at **both** delivery sites —
/// a filter that exists only on the live path is the classic form of this bug.
pub(crate) fn delivers_to(
    envelope: &EventEnvelope,
    class: DeviceClass,
    device_id: Option<&str>,
    owned_stream: Option<&str>,
) -> bool {
    match envelope.channel {
        Channel::Session => class.holds(ClassScope::Ui.as_str()),
        Channel::Display => {
            if !class.holds(ClassScope::DisplayAgent.as_str()) {
                return false;
            }
            // An addressed placement reaches exactly its target (F7.5); an
            // unaddressed one is the pre-node behaviour and reaches every
            // presenter.
            match envelope
                .payload
                .get("targetDeviceId")
                .and_then(|v| v.as_str())
            {
                Some(target) => device_id.is_some_and(|id| id == target),
                None => true,
            }
        }
        Channel::Voice => {
            if !class.holds(ClassScope::VoiceCapture.as_str()) {
                return false;
            }
            // Addressed to a *device* (F8.5): a timer must ring in the room it
            // was set in and nowhere else. Checked before the stream rule,
            // because this kind of event belongs to a room rather than to a
            // conversation — there is no capture stream open when a timer goes
            // off in an empty kitchen.
            if let Some(target) = envelope
                .payload
                .get("targetDeviceId")
                .and_then(|v| v.as_str())
            {
                return device_id.is_some_and(|id| id == target);
            }
            match envelope.payload.get("streamId").and_then(|v| v.as_str()) {
                // Addressed to a stream: only its owner hears it.
                Some(stream) => owned_stream == Some(stream),
                // No stream named — a pipeline-wide notice. The owner sees it;
                // a satellite has nothing to do with it.
                None => class.holds(ClassScope::Ui.as_str()),
            }
        }
    }
}

/// Ask the delivery filter a question from an integration test.
///
/// [`delivers_to`] is `pub(crate)` and takes types an integration test cannot
/// easily build; this is a thin string-to-type translation over the **same**
/// function, so golden 12 exercises the production rule rather than a copy of
/// it. Deliberately narrow: it answers, it cannot deliver anything.
#[doc(hidden)]
pub fn delivers_to_for_test(
    channel: &str,
    event_type: &str,
    payload: &serde_json::Value,
    class: &str,
    device_id: Option<&str>,
    owned_stream: Option<&str>,
) -> bool {
    let channel = match channel {
        "voice" => Channel::Voice,
        "display" => Channel::Display,
        _ => Channel::Session,
    };
    let envelope = EventEnvelope {
        v: CONTRACT_VERSION,
        seq: 0,
        channel,
        event_type: event_type.to_owned(),
        occurred_at: now_rfc3339(),
        trace_id: None,
        resource_version: None,
        payload: payload.clone(),
    };
    let class: DeviceClass = class.parse().expect("a real device class");
    delivers_to(&envelope, class, device_id, owned_stream)
}

/// How many ids one socket is remembered as owning. A socket that opens more
/// than a handful of streams is a long-lived UI; the oldest are no longer
/// receiving events anyone is waiting for.
///
/// A voice turn now claims **three** ids — the capture stream, the utterance,
/// and (F8.5) the run — so this is 12 rather than 8 to keep the same four turns
/// of history a satellite could previously rely on. Evicting a run id early
/// would cut off the answer to the question before last, mid-sentence.
const MAX_REMEMBERED_STREAMS: usize = 12;

/// One id this socket owns, **tagged by what kind of id it is**.
///
/// The tag is the security property, not bookkeeping. Capture-stream ids are
/// chosen by the *client*; run ids are minted by the *daemon*. Kept in one
/// untagged list, a socket could declare a `streamId` equal to somebody else's
/// run id and be treated as that run's owner — receiving the full spoken answer
/// to a question it never asked, straight past the Session channel's `ui` rule
/// that F7.4/CF-8 exist to enforce.
///
/// That was not reachable when this was written (run ids are ULIDs and nothing
/// a node may receive discloses another run's id), but it is a confused deputy
/// waiting for one future field to hand it the key. Tagging costs nothing and
/// removes the class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OwnedId {
    /// A capture stream or spoken utterance — **client-supplied**.
    Stream(String),
    /// A run this socket started — **daemon-minted**.
    Run(String),
}

impl OwnedId {
    fn stream(&self) -> Option<&str> {
        match self {
            Self::Stream(id) => Some(id),
            Self::Run(_) => None,
        }
    }

    fn run(&self) -> Option<&str> {
        match self {
            Self::Run(id) => Some(id),
            Self::Stream(_) => None,
        }
    }
}

/// Remember an id this socket owns — a capture stream, a spoken utterance, or
/// a run this socket started.
/// Bounded: the list grows with client behaviour, and the oldest entries are
/// no longer receiving events anyone is waiting for.
pub(crate) fn register_owned_stream(owned: &mut std::collections::VecDeque<OwnedId>, id: OwnedId) {
    if owned.iter().any(|existing| existing == &id) {
        return;
    }
    owned.push_back(id);
    while owned.len() > MAX_REMEMBERED_STREAMS {
        owned.pop_front();
    }
}

/// The Session-channel events a satellite is allowed to hear for a run it
/// started (F8.5) — precisely the ones [`crate::ws::voice::feed_speech`] turns into speech.
///
/// Kept next to [`delivers_to_owner_of`] rather than derived from `feed_speech`
/// so that widening it is a deliberate edit to a security rule with its own
/// test, not a side effect of teaching the speech assembler a new event.
const SPOKEN_RUN_EVENTS: [&str; 5] = [
    "text.delta",
    "run.completed",
    "run.queued",
    "degraded.queued",
    // S3: how the answer may be spoken, sent to the node that will speak it.
    //
    // A deliberate widening, weighed against the rule above rather than added
    // because `feed_speech` learned a new event. It survives that test on the
    // ground the rule cares about — what a satellite gains by hearing it. This
    // payload is a run id and a single bit, carried beside `text.delta`, which
    // is already sending that same socket the entire answer. It reveals
    // strictly less than what it rides along with, and withholding it would
    // not protect the content: it would just mean the node speaks that content
    // in a third-party voice.
    "run.speech_sensitive",
];

/// [`delivers_to`] against every stream this socket owns.
pub(crate) fn delivers_to_owner_of(
    envelope: &EventEnvelope,
    class: DeviceClass,
    device_id: &str,
    owned: &std::collections::VecDeque<OwnedId>,
) -> bool {
    if let Some(stream) = envelope.payload.get("streamId").and_then(|v| v.as_str()) {
        // Matched only against ids registered as *streams*.
        return owned
            .iter()
            .filter_map(OwnedId::stream)
            .any(|s| s == stream)
            && delivers_to(envelope, class, Some(device_id), Some(stream));
    }

    // F8.5, the answer path: the run a satellite started must come back to it.
    //
    // A run's text deltas ride the Session channel, and the Session rule is
    // `ui` — which a `voice-node`/`room-node` deliberately never holds (F7.1;
    // a satellite is not an operator console). So the node that asked the
    // question was the one socket that could not hear the answer, and a node
    // cannot speak what it is not sent.
    //
    // Two keys, not one, and the second is the important one:
    //
    // * **ownership** — the run *this socket started*, so a node still cannot
    //   see another room's conversation or any run it did not begin.
    //   Ownership is per-socket and in memory, so it does not survive a
    //   reconnect, which is why `replay_since` needs no matching change.
    // * **an allowlist of event types** — exactly what [`feed_speech`]
    //   consumes to build the spoken answer, and nothing else. Ownership alone
    //   would be a standing invitation: `approval.requested` is a Session event
    //   about a specific run, and it carries the exact effect, the real
    //   arguments, and an approval id that is a decision oracle. It only fails
    //   to match today because its `runId` happens to sit nested under `card`,
    //   which is an accident of a DTO's shape and not a security boundary.
    if matches!(envelope.channel, Channel::Session)
        && SPOKEN_RUN_EVENTS.contains(&envelope.event_type.as_str())
        && let Some(run) = envelope.payload.get("runId").and_then(|v| v.as_str())
        // Matched only against ids the DAEMON minted. A client-declared
        // `streamId` that happens to equal a run id buys nothing.
        && owned.iter().filter_map(OwnedId::run).any(|r| r == run)
    {
        return true;
    }

    delivers_to(envelope, class, Some(device_id), None)
}

/// Close code sent when a socket is dropped because its device was revoked
/// (F7.1). 1008 "policy violation" — the connection was fine, the
/// authorization behind it stopped being.
pub(crate) const REVOKED_CLOSE_CODE: CloseCode = 1008;

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
    /// Revocations announced by `POST /devices/{id}/revoke` (F7.1). A socket
    /// authorizes once, at upgrade; without this it would keep streaming to a
    /// device the owner just revoked until the client happened to reconnect.
    /// Defaults to a bus nobody publishes on, which is what tests that mount
    /// the socket without the device surface want.
    pub revocations: crate::devices::RevocationBus,
    /// Read back once per socket, immediately after subscribing, to close the
    /// **subscribe-after-authorize race**: a `broadcast` receiver only sees
    /// values sent after `subscribe()`, and authorization happened earlier, in
    /// `require_device`, before the upgrade completed. A revocation landing in
    /// that window would otherwise be lost — and the socket would then hold
    /// its cached authority for its whole lifetime (security-auditor, F7.1).
    /// `None` in deployments that mount no device surface, where nothing can
    /// revoke anything.
    pub identity: Option<Arc<dyn IdentityStore>>,
    /// Who is holding a socket right now (F7.5) — read by the placement route
    /// so "the kitchen screen is not connected" is answerable before a
    /// directive is audited and dispatched.
    pub connected: crate::devices::ConnectedDevices,
    /// Durable record for refused capture attempts (F7.6). A device reaching
    /// for a microphone it was never granted is exactly the docs/06 §5
    /// "remote node impersonation" signal worth keeping.
    pub audit: Option<Arc<dyn jarvis_application::ports::AuditLog>>,
    /// What each node should be showing (F7.7), re-asserted when it connects.
    pub surfaces: crate::devices::SurfaceState,
}
