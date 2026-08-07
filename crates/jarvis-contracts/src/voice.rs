//! Voice-channel control frames (docs/05 §1, FR-13).
//!
//! Audio itself is carried as bounded binary WebSocket messages. These small
//! JSON frames negotiate and close one push-to-talk stream; they are kept in
//! the contract crate so the browser and daemon cannot drift on field names.
//!
//! One enum covers **both directions** of the voice channel (F5.2), because the
//! bracket-the-binary pattern is the same either way:
//!
//! * client → daemon: `voice.stream.start` … PCM frames … `voice.stream.stop`,
//!   sent as bare JSON text frames (there is no envelope on the inbound path;
//!   the daemon is the only reader).
//! * daemon → client: `voice.speak.start` … PCM frames … `voice.speak.stop`,
//!   delivered **inside an [`crate::envelope::EventEnvelope`]** on the `voice`
//!   channel, like every other server-authored text frame. Server→client text
//!   frames are envelopes without exception (docs/05 §3), so the payload here is
//!   the variant's fields and the `voice.speak.*` tag rides the envelope `type`.
//!
//! Additive evolution only: variants are appended, never renamed or reordered.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// JSON control frames that bracket a voice PCM stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum VoiceControlDto {
    /// Sent before the first binary PCM frame.
    #[serde(rename = "voice.stream.start")]
    StreamStart {
        stream_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        sample_rate_hz: u32,
        sample_width_bytes: u16,
        channels: u16,
    },
    /// Sent after the final binary PCM frame.
    #[serde(rename = "voice.stream.stop")]
    StreamStop { stream_id: String },
    /// Daemon → client: spoken output for one turn begins; the binary frames
    /// that follow are PCM in this format, until the matching
    /// [`VoiceControlDto::SpeakStop`]. `utteranceId` scopes the audio so a
    /// barge-in-cancelled utterance can never be confused with the next one.
    #[serde(rename = "voice.speak.start")]
    SpeakStart {
        utterance_id: String,
        /// The run whose response is being spoken, when there is one.
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        sample_rate_hz: u32,
        sample_width_bytes: u16,
        channels: u16,
    },
    /// Daemon → client: no further audio belongs to `utteranceId`. `reason`
    /// distinguishes a finished utterance from an interrupted or failed one —
    /// silence alone would be indistinguishable between the three.
    #[serde(rename = "voice.speak.stop")]
    SpeakStop {
        utterance_id: String,
        reason: VoiceSpeakEndDto,
    },
}

/// Why spoken output for one utterance ended (F5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VoiceSpeakEndDto {
    /// The whole response was synthesized and sent.
    Completed,
    /// Barge-in (or shutdown) cancelled synthesis mid-utterance; the client
    /// must stop playback of what it has already buffered.
    Cancelled,
    /// The synthesizer failed; playback stops and the failure is surfaced by a
    /// `voice.error` transient event rather than as truncated audio.
    Failed,
}

/// Stable machine codes for a voice-pipeline failure (`voice.error`). Free-form
/// service text is deliberately **not** on the wire: the client maps the code to
/// its own message, so no adapter/transport string can reach the UI (docs/06 §5).
/// Additive only — codes are never renamed or reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum VoiceErrorCodeDto {
    /// The speech-to-text service could not be reached.
    #[serde(rename = "voice.stt_unavailable")]
    SttUnavailable,
    /// The speech-to-text service failed or broke the protocol mid-stream.
    #[serde(rename = "voice.stt_failed")]
    SttFailed,
    /// The text-to-speech service could not be reached.
    #[serde(rename = "voice.tts_unavailable")]
    TtsUnavailable,
    /// The text-to-speech service failed or broke the protocol mid-utterance.
    #[serde(rename = "voice.tts_failed")]
    TtsFailed,
}
