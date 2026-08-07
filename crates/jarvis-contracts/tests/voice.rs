//! Voice-channel control framing (docs/05 §1).

use jarvis_contracts::voice::VoiceControlDto;
use serde_json::json;

#[test]
fn control_frames_round_trip_with_documented_tags_and_casing() {
    let start = VoiceControlDto::StreamStart {
        stream_id: "stream-1".into(),
        session_id: None,
        sample_rate_hz: 16_000,
        sample_width_bytes: 2,
        channels: 1,
    };
    assert_eq!(
        serde_json::to_value(&start).unwrap(),
        json!({
            "type": "voice.stream.start",
            "streamId": "stream-1",
            "sampleRateHz": 16000,
            "sampleWidthBytes": 2,
            "channels": 1,
        })
    );
    let back: VoiceControlDto =
        serde_json::from_value(serde_json::to_value(start).unwrap()).unwrap();
    assert_eq!(
        back,
        VoiceControlDto::StreamStart {
            stream_id: "stream-1".into(),
            session_id: None,
            sample_rate_hz: 16_000,
            sample_width_bytes: 2,
            channels: 1,
        }
    );

    let stop: VoiceControlDto = serde_json::from_value(json!({
        "type": "voice.stream.stop",
        "streamId": "stream-1",
    }))
    .unwrap();
    assert_eq!(
        stop,
        VoiceControlDto::StreamStop {
            stream_id: "stream-1".into()
        }
    );
}
