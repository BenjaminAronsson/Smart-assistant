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
// S3: the label lives in the domain, beside `DataEgress` (see its doc there).
// Imported rather than re-exported — a re-export would give one routing rule
// two names inside one crate, which is the divergence ADR-034 spent M9 undoing.
use futures_core::stream::BoxStream;
use jarvis_domain::policy::SpeechSensitivity;
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
        sensitivity: SpeechSensitivity,
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
    ///
    /// The payload is an **adapter-authored** reason, not transport text: short,
    /// stable strings the adapter chose, carrying no address and no secret. It
    /// is surfaced because a message that says only "unavailable" cannot be
    /// acted on — the first live voice turn produced exactly that and the log
    /// could not say what had gone wrong.
    #[error("voice service unavailable: {0}")]
    Unavailable(String),
    /// The request was cancelled before it produced a terminal result.
    #[error("voice request was cancelled")]
    Cancelled,
    /// The service's response did not follow the wire protocol. Payload as
    /// above: adapter-authored, safe to log, and the only thing that makes this
    /// diagnosable.
    #[error("malformed voice service response: {0}")]
    Malformed(String),
}

/// Longest run of response text this will hold back waiting for a clause
/// terminator. A model that answers in one long comma-free sentence must not
/// delay the *first* audio past the NFR-04 "first audio < 1.2 s" budget, and the
/// buffer must not grow with the response length either — so past this point the
/// segmenter breaks at the last word boundary instead of waiting.
const MAX_PENDING_CLAUSE_BYTES: usize = 160;

/// Splits streamed response text into speakable clauses (docs/02 §9: TTS "starts
/// from complete clauses"). Pure text logic with no I/O, so it lives beside the
/// ports rather than in the daemon, and its boundary rules are unit-testable
/// without a synthesizer.
///
/// A clause ends at `.`/`!`/`?`/`;`/`:`/newline **that is already followed by
/// whitespace in the buffer**. Requiring the following whitespace to have
/// arrived is what keeps `34.5`, `16:9` and `v1.2` intact: a terminator at the
/// very end of the buffer so far is not yet known to be a terminator, so the
/// segmenter waits for the next delta instead of guessing.
#[derive(Debug, Default)]
pub struct ClauseSegmenter {
    pending: String,
}

impl ClauseSegmenter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one streamed delta and return every clause it completed, in order.
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.pending.push_str(delta);
        let mut clauses = Vec::new();
        while let Some(split) = self.next_split() {
            let rest = self.pending.split_off(split);
            let clause = std::mem::replace(&mut self.pending, rest);
            let clause = clause.trim();
            if !clause.is_empty() {
                clauses.push(clause.to_owned());
            }
        }
        clauses
    }

    /// The tail after the last complete clause — the final partial sentence,
    /// spoken once the response is known to be finished.
    pub fn flush(&mut self) -> Option<String> {
        let tail = std::mem::take(&mut self.pending);
        let tail = tail.trim();
        (!tail.is_empty()).then(|| tail.to_owned())
    }

    /// Byte index just past the end of the next complete clause, if any.
    /// Terminators are ASCII, so `index + 1` is always a char boundary.
    fn next_split(&self) -> Option<usize> {
        let bytes = self.pending.as_bytes();
        for (index, byte) in bytes.iter().enumerate() {
            if !matches!(byte, b'.' | b'!' | b'?' | b';' | b':' | b'\n') || index + 1 == bytes.len()
            {
                // A terminator at the very end of what has arrived so far is
                // not yet known to be one ("34." may become "34.5"), so it is
                // left for the next delta to decide.
                continue;
            }
            if bytes[index + 1].is_ascii_whitespace() {
                return Some(index + 1);
            }
        }
        // No usable terminator, but the buffer has grown past the hold-back
        // bound: break at the last word boundary so speech starts (and memory
        // stays bounded) rather than waiting for a sentence that may never end.
        if self.pending.len() > MAX_PENDING_CLAUSE_BYTES {
            return self
                .pending
                .char_indices()
                .filter(|(_, c)| c.is_whitespace())
                .map(|(at, c)| at + c.len_utf8())
                .next_back();
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::ClauseSegmenter;

    #[test]
    fn a_sentence_is_emitted_once_its_terminator_is_followed_by_space() {
        let mut segmenter = ClauseSegmenter::new();
        assert!(segmenter.push("Hello there").is_empty());
        // The '.' alone is not yet a boundary — the next character decides.
        assert!(segmenter.push(".").is_empty());
        assert_eq!(segmenter.push(" And more"), vec!["Hello there."]);
        assert_eq!(segmenter.flush().as_deref(), Some("And more"));
    }

    #[test]
    fn a_decimal_point_is_not_a_clause_boundary() {
        let mut segmenter = ClauseSegmenter::new();
        // The M4 deterministic math answer: splitting at "34." would speak a
        // wrong number, which is worse than speaking late.
        assert!(segmenter.push("15% of 230 = 34.5").is_empty());
        assert_eq!(segmenter.flush().as_deref(), Some("15% of 230 = 34.5"));
    }

    #[test]
    fn several_clauses_in_one_delta_come_out_in_order() {
        let mut segmenter = ClauseSegmenter::new();
        assert_eq!(
            segmenter.push("One. Two! Three? "),
            vec!["One.", "Two!", "Three?"]
        );
        assert_eq!(segmenter.flush(), None);
    }

    #[test]
    fn an_unterminated_run_breaks_at_a_word_boundary_rather_than_growing() {
        let mut segmenter = ClauseSegmenter::new();
        let long = "word ".repeat(60); // 300 chars, no terminator anywhere
        let clauses = segmenter.push(&long);
        assert!(
            !clauses.is_empty(),
            "speech must start before the sentence ends"
        );
        let spoken: String = clauses.join(" ");
        assert!(spoken.split_whitespace().all(|w| w == "word"));
        // Nothing was lost or duplicated across the break.
        let tail = segmenter.flush().unwrap_or_default();
        let total = spoken.split_whitespace().count() + tail.split_whitespace().count();
        assert_eq!(total, 60);
    }

    #[test]
    fn flush_on_an_empty_segmenter_speaks_nothing() {
        assert_eq!(ClauseSegmenter::new().flush(), None);
        assert_eq!(ClauseSegmenter::new().push("   "), Vec::<String>::new());
    }
}
