//! Voice-channel control frames (docs/05 §1, FR-13).
//!
//! Audio itself is carried as bounded binary WebSocket messages. These small
//! JSON frames negotiate and close one push-to-talk stream; they are kept in
//! the contract crate so the browser and daemon cannot drift on field names.

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
}
