//! The voice pipeline boundary (F5.1, FR-13, ADR-007, docs/02 §9).
//!
//! VAD, STT, and TTS are three independently swappable, Wyoming-compatible
//! out-of-process services (ADR-007) — the orchestrator (F5.2+) talks only to
//! these ports, never to a concrete Wyoming client. Mirrors the shape of
//! [`crate::model::ModelProvider`] and [`crate::ports::EmbeddingProvider`]:
//! `async_trait`, a `CancellationToken` the implementation must honor promptly,
//! and `BoxStream` for streamed output. No adapter/provider type crosses this
//! boundary (arch-test enforces the crate purity rule, CLAUDE.md invariant 3).
//!
//! This slice defines the ports only; a Wyoming TCP client (`jarvis-adapters`)
//! and daemon wiring land in F5.2.

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use tokio_util::sync::CancellationToken;

/// PCM framing shared by every leg of the pipeline (Wyoming `audio-start`/
/// `audio-chunk` fields). Raw audio bytes themselves travel as plain
/// `Vec<u8>` stream items; the format is negotiated once per stream, not
/// per-chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate_hz: u32,
    pub sample_width_bytes: u16,
    pub channels: u16,
}

/// Voice-activity boundary events (Wyoming `voice-started`/`voice-stopped`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VadEvent {
    VoiceStarted,
    VoiceStopped,
    /// The detection failed mid-stream. Mirrors [`crate::model::ModelEvent::Error`]:
    /// a stream that simply *ends* means the turn finished normally, so a
    /// transport/protocol failure must be its own observable event — otherwise
    /// "the service died" is indistinguishable from "no speech happened".
    Error(VoiceError),
}

/// Gates audio and detects end-of-turn (docs/02 §9 "VAD (Silero)").
#[async_trait]
pub trait VoiceActivityDetector: Send + Sync {
    /// The service instance this detector talks to. Opaque to callers.
    fn id(&self) -> &str;

    /// Stream PCM audio in, get voice boundary events out. `cancel` must abort
    /// in-flight work promptly and leave no orphaned connection (invariant 4).
    async fn detect(
        &self,
        audio: BoxStream<'static, Vec<u8>>,
        format: AudioFormat,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, VadEvent>, VoiceError>;
}

/// A speech-to-text result (docs/02 §9 "STT"). `Partial` is an incremental,
/// possibly-revised hypothesis; `Final` is the turn's settled transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEvent {
    Partial(String),
    Final(String),
    /// Transcription failed mid-stream (see [`VadEvent::Error`]). Without this,
    /// a dropped STT connection reaches the orchestrator as an empty transcript
    /// stream — i.e. as "the user said nothing", which is a dishonest failure
    /// state for a voice assistant and would silently swallow a dead service.
    Error(VoiceError),
}

/// Produces partial and final transcripts from streamed audio.
#[async_trait]
pub trait SpeechTranscriber: Send + Sync {
    fn id(&self) -> &str;

    async fn transcribe(
        &self,
        audio: BoxStream<'static, Vec<u8>>,
        format: AudioFormat,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, TranscriptEvent>, VoiceError>;
}

/// Synthesizes speech from text (docs/02 §9 "TTS (Piper)"). Starts from
/// complete clauses and stops on barge-in in a later slice; here it is simply
/// text in, framed PCM out.
#[async_trait]
pub trait SpeechSynthesizer: Send + Sync {
    fn id(&self) -> &str;

    /// Returns the output audio format plus a stream of raw PCM chunks.
    ///
    /// Chunks are `Result` for the same reason [`VadEvent::Error`] exists: a
    /// playback path must be able to tell "the utterance finished" from "the
    /// synthesizer died halfway through", rather than truncating the spoken
    /// response and reporting success.
    async fn synthesize(
        &self,
        text: &str,
        cancel: CancellationToken,
    ) -> Result<(AudioFormat, BoxStream<'static, Result<Vec<u8>, VoiceError>>), VoiceError>;
}

/// A voice-service-side failure (mirrors [`crate::model::ModelError`]'s
/// neutral shape — no raw adapter/transport text crosses this boundary,
/// docs/06 §5). Stable reason-code classification (mirroring
/// `health::REASON_CODES`) is F5.2's job once there is a health tracker to
/// feed; this slice only needs the coarse cases.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VoiceError {
    /// The voice service could not be reached or is unhealthy.
    #[error("voice service unavailable")]
    Unavailable(String),
    /// The request was cancelled before it produced a terminal result.
    #[error("voice request was cancelled")]
    Cancelled,
    /// The service's response did not follow the wire protocol.
    #[error("malformed voice service response")]
    Malformed(String),
}
