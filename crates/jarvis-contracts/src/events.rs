//! Typed WebSocket event payloads (docs/05 §3). The union is split into two
//! Rust types so the persistence classification is carried by the type system,
//! not by convention:
//!
//! * [`DomainEvent`] — persisted to the outbox and **replayable** on resync via
//!   `since` (run state, message creation, provider health, checkpoints).
//! * [`TransientEvent`] — direct broadcast, **never replayed** (token deltas).
//!
//! A `DomainEvent` can always be reconstructed into the timeline snapshot
//! (`crate::timeline::TimelineItem`); a `TransientEvent` never can — that is the
//! resync contract (NFR-13). The WS hub wraps either in a
//! [`crate::envelope::EventEnvelope`] and fills the envelope fields; payload
//! authors never touch `seq`/`occurredAt`/etc.

use crate::approvals::{ApprovalCardDto, ApprovalResolutionDto};
use crate::messages::MessageDto;
use crate::providers::ProviderDto;
use crate::runs::{RunOutcome, RunStateDto};
use jarvis_domain::ids::{ApprovalId, RunId, SessionId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Persisted, replayable events (docs/05 §3 "persisted event categories").
/// Every variant must be representable in the timeline snapshot — a client that
/// missed it while disconnected recovers it via `GET /sessions/{id}/timeline`.
///
/// The `type` discriminator is **dotted-namespaced** (`run.started`,
/// `message.created`), matching the envelope example in docs/05 §3
/// (`run.tool.completed`) and the error-code scheme — clients route on this
/// string, so it is load-bearing and every tag is spelled explicitly below.
///
/// This union is intentionally **strict**: there is no `Unknown` catch-all
/// (unlike [`crate::content::ContentBlock`], which is open-world because blocks
/// originate from external providers). Every `DomainEvent` is authored by
/// jarvisd itself, and the web shell is served by the same binary — so producer
/// and consumer share one contract version and can never skew. A tag the reader
/// does not recognize is therefore a genuine bug we want surfaced as a decode
/// error, not silently dropped from a resync page. New variants are added
/// additively within a version (docs/05 §3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
// camelCase fields (the wire convention everywhere else in this crate); the
// dotted variant tags are set per-variant since `rename_all` cannot produce
// namespaced names.
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum DomainEvent {
    #[serde(rename = "run.started")]
    RunStarted {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        #[schemars(with = "crate::schema::UlidString")]
        session_id: SessionId,
    },
    #[serde(rename = "run.state_changed")]
    RunStateChanged {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        state: RunStateDto,
    },
    /// Degraded mode: the run is parked awaiting provider recovery (FR-12) —
    /// a visible waiting state, replayed so a reconnecting client still sees it.
    #[serde(rename = "run.queued")]
    RunQueued {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        reason: String,
    },
    #[serde(rename = "run.completed")]
    RunCompleted {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        outcome: RunOutcome,
    },
    #[serde(rename = "message.created")]
    MessageCreated { message: MessageDto },
    #[serde(rename = "provider.health_changed")]
    ProviderHealthChanged { provider: ProviderDto },
    /// Recovery checkpoint (NFR-05/13); replayed so resync reflects the last
    /// safe boundary a restart would resume from.
    #[serde(rename = "run.checkpoint_saved")]
    CheckpointSaved {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        state: RunStateDto,
    },
    /// An R2/R3 tool proposal is parked awaiting a human decision (F2.5,
    /// docs/06 §3). Replayed so a client reconnecting while a run sits at
    /// `WaitingApproval` still sees the pending card and can act on it.
    #[serde(rename = "approval.requested")]
    ApprovalRequested { card: ApprovalCardDto },
    /// A timer, alarm or reminder went off (F3b.7, FR-33, ADR-023).
    ///
    /// Persisted rather than transient, deliberately: a timer going off is a
    /// *fact about a moment*, and a client that was disconnected when the
    /// kitchen timer rang must still learn about it on resync — unlike
    /// `media.state`, which is a current-value readout with no history worth
    /// replaying. It is also not run-scoped (a timer fires with no run in
    /// flight), which is why it carries no `RunId`.
    ///
    /// This feature is the event's only producer: the timer module fires it, and
    /// nothing else may. `missed` is the honest notice ADR-023 requires — the
    /// timer came due while jarvisd was not running, and the human is told so
    /// rather than shown something that looks like it just rang.
    #[serde(rename = "timer.fired")]
    TimerFired {
        timer: crate::timers::TimerDto,
        missed: bool,
    },
    /// A pending approval was answered (F2.5). Replayed so the resolved outcome
    /// survives reconnect and the client can retire the card it was showing.
    #[serde(rename = "approval.resolved")]
    ApprovalResolved {
        #[schemars(with = "crate::schema::UlidString")]
        approval_id: ApprovalId,
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        outcome: ApprovalResolutionDto,
    },
}

impl DomainEvent {
    /// The envelope `type` discriminator for this event (docs/05 §3). The hub
    /// copies this onto the [`crate::envelope::EventEnvelope`]. Must stay in
    /// lockstep with the `#[serde(rename)]` tags above.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::RunStarted { .. } => "run.started",
            Self::RunStateChanged { .. } => "run.state_changed",
            Self::RunQueued { .. } => "run.queued",
            Self::RunCompleted { .. } => "run.completed",
            Self::MessageCreated { .. } => "message.created",
            Self::ProviderHealthChanged { .. } => "provider.health_changed",
            Self::CheckpointSaved { .. } => "run.checkpoint_saved",
            Self::ApprovalRequested { .. } => "approval.requested",
            Self::ApprovalResolved { .. } => "approval.resolved",
            Self::TimerFired { .. } => "timer.fired",
        }
    }
}

/// Disposable, never-replayed events (docs/05 §3 "not persisted"). A durable
/// snapshot (`DomainEvent`) always follows the work these describe. Dotted tags
/// and strict (no-catch-all) decoding for the same reasons as [`DomainEvent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum TransientEvent {
    /// One incremental chunk of streamed model output (FR-01).
    #[serde(rename = "text.delta")]
    TextDelta {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        text: String,
    },
    /// This run's spoken answer must stay in the house (ADR-033 §4, S3).
    ///
    /// Emitted the moment private content enters the run — a tool declared
    /// `SpeechSensitivity::Sensitive` returned, or an agenda was assembled — and
    /// always ahead of the text that quotes it, so a node picks the local voice
    /// for the sentence rather than one sentence later.
    ///
    /// Transient, for the same reason as `text.delta`: it describes an utterance
    /// in flight. There is no utterance to label after a reconnect, and a
    /// replayed escalation would attach to whatever is being spoken *then*.
    ///
    /// Carries only the run id. Deliberately not the tool that triggered it:
    /// this event reaches a room satellite (it is on the hub's spoken-run
    /// allowlist), and "the owner just read their mail" is more than a device
    /// needs in order to choose a synthesizer. One bit, one direction — there is
    /// no de-escalation event, because a label that could be walked back would
    /// not be a routing constraint.
    #[serde(rename = "run.speech_sensitive")]
    SpeechSensitive {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
    },
    /// A run entered degraded-mode queueing. The next durable run snapshot is
    /// authoritative after reconnect; this live notice supplies the position
    /// while the provider is unavailable (FR-12, angular-shell §5).
    #[serde(rename = "degraded.queued")]
    DegradedQueued {
        #[schemars(with = "crate::schema::UlidString")]
        run_id: RunId,
        reason: String,
        position: usize,
    },
    /// Current local playback state, feeding the media bar (FR-22, docs/02
    /// §11a — "a `media.state` transient WS event").
    ///
    /// Transient is the correct classification, not a shortcut: this is a
    /// *current-value readout* of whatever is playing right now, not a fact
    /// about the run timeline. A client that missed one is not missing history
    /// — it holds a stale value that the next change corrects, and a client
    /// that just connected reads `GET /api/v1/media/state` instead of replaying.
    /// It is also not run-scoped: media state exists with no run in flight,
    /// which is why this variant carries no `RunId`.
    #[serde(rename = "media.state")]
    MediaState { state: crate::media::MediaStateDto },
    /// One canvas instruction for the HUD (F3b.6, FR-27/FR-24, ADR-017): what
    /// this turn does to the materialization canvas, and the cards that belong
    /// on it. The **first producer** of [`crate::cards::HudCardDto`] on the wire.
    ///
    /// Transient, for the same reason as `media.state` and for one more:
    ///
    /// * The canvas is a *current-value* surface, not history. Panels shelve,
    ///   are dismissible, and expire silently on a TTL (docs/12 §4), so a
    ///   client that missed an instruction is not missing a fact about the past
    ///   — and the payload carries the live card set rather than a delta, so
    ///   the next instruction re-converges it. The durable record of a thread
    ///   is the Research Notes artifact (FR-08), which has its own read surface.
    /// * A `DomainEvent` is published from the outbox in the same transaction
    ///   as the domain change it describes (invariant #6). A deep-dive turn
    ///   commits no row, so there is no transaction to ride and nothing
    ///   honest to replay from.
    ///
    /// Not run-scoped: a canvas update can come from a list command with no run
    /// and no session at all, which is why the session id lives (optional)
    /// inside the payload rather than beside it.
    #[serde(rename = "hud.canvas")]
    HudCanvas {
        canvas: crate::deepdive::HudCanvasDto,
    },
    /// Partial or final speech recognition text on the voice channel. It is
    /// disposable: the committed user message is the durable transcript.
    #[serde(rename = "voice.transcript")]
    VoiceTranscript {
        stream_id: String,
        text: String,
        #[serde(rename = "final")]
        is_final: bool,
    },
    /// A voice-pipeline leg failed (F5.2). Transient for the same reason as
    /// `voice.transcript`: it describes a live capture/playback attempt, not a
    /// fact about the run timeline, and a client that was disconnected has
    /// nothing to recover.
    ///
    /// It exists because a stream that simply *ends* means the service finished
    /// normally (`jarvis_application::voice`) — without this event a dead STT
    /// service is indistinguishable from a user who said nothing, and a dead TTS
    /// service is indistinguishable from a silent response. Only a stable
    /// [`crate::voice::VoiceErrorCodeDto`] crosses the wire; no service text.
    #[serde(rename = "voice.error")]
    VoiceError {
        /// The capture stream, or the utterance, the failure belongs to.
        stream_id: String,
        code: crate::voice::VoiceErrorCodeDto,
    },
}

impl TransientEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::TextDelta { .. } => "text.delta",
            Self::SpeechSensitive { .. } => "run.speech_sensitive",
            Self::DegradedQueued { .. } => "degraded.queued",
            Self::MediaState { .. } => "media.state",
            Self::HudCanvas { .. } => "hud.canvas",
            Self::VoiceTranscript { .. } => "voice.transcript",
            Self::VoiceError { .. } => "voice.error",
        }
    }
}
