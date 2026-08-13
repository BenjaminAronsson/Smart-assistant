//! The node's half of the voice channel (F8.2, docs/05 §1, FR-13).
//!
//! Inbound: `voice.speak.start` … binary PCM … `voice.speak.stop`, arriving
//! inside an [`EventEnvelope`] on the `voice` channel like every other
//! server-authored text frame. Outbound: `voice.stream.start` … binary PCM …
//! `voice.stream.stop`, sent as bare JSON.
//!
//! Deliberately free of I/O and of `cpal`: it holds an [`AudioOutput`] and
//! decides what to do with frames. That is what lets the interesting claims —
//! a mismatched format is refused, a cancelled utterance silences immediately,
//! a malformed frame does not corrupt the stream — be tested without a sound
//! card.

use anyhow::Result;
use jarvis_contracts::envelope::{Channel, EventEnvelope};
use jarvis_contracts::voice::{VoiceControlDto, VoiceSpeakEndDto};

use crate::audio::{self, AudioOutput};

/// Ceiling for one inbound audio frame. The daemon chunks its own output well
/// below this; anything larger is a bug or an attack, not speech.
pub const MAX_AUDIO_FRAME_BYTES: usize = 16 * 1024;

/// What the caller should do after handing a frame in.
#[derive(Debug, PartialEq, Eq)]
pub enum Reaction {
    /// Nothing further; the frame was handled (or harmlessly ignored).
    Handled,
    /// The daemon started speaking. A node with a microphone next to its
    /// speaker needs to know (F8.4 turns this into ducking and AEC).
    SpeechStarted,
    /// The daemon stopped speaking, for the given reason.
    SpeechStopped(VoiceSpeakEndDto),
    /// A timer went off in this room (F8.5).
    Rang,
}

/// Playback state for one node.
pub struct NodeVoice<O: AudioOutput> {
    output: O,
    /// The utterance currently being played, if any. Frames arriving outside an
    /// utterance are dropped: audio with no `speak.start` has no agreed format,
    /// and guessing is how you get a burst of noise at 3 a.m.
    speaking: Option<String>,
}

impl<O: AudioOutput> NodeVoice<O> {
    pub fn new(output: O) -> Self {
        Self {
            output,
            speaking: None,
        }
    }

    /// Whether the node is currently playing spoken output.
    pub fn is_speaking(&self) -> bool {
        self.speaking.is_some()
    }

    /// Handles a server text frame. Non-voice channels are ignored.
    pub fn on_envelope(&mut self, envelope: &EventEnvelope) -> Result<Reaction> {
        if envelope.channel != Channel::Voice {
            return Ok(Reaction::Handled);
        }
        // A timer going off in this room (F8.5). The daemon has already decided
        // this node is the room — the fan-out only delivers an addressed alert
        // to its target — so there is nothing to check here beyond the tag.
        if envelope.event_type == "timer.fired" {
            self.ring_alert()?;
            return Ok(Reaction::Rang);
        }
        // The hub puts the tag on the envelope and the variant's fields in the
        // payload, so merge them back before decoding (same shape as
        // `client::decode_directive`).
        let mut value = envelope.payload.clone();
        let Some(object) = value.as_object_mut() else {
            return Ok(Reaction::Handled);
        };
        object.insert(
            "type".to_owned(),
            serde_json::Value::String(envelope.event_type.clone()),
        );
        // An unknown voice frame is not fatal: forward compatibility, same as
        // display directives.
        let Ok(control) = serde_json::from_value::<VoiceControlDto>(value) else {
            return Ok(Reaction::Handled);
        };
        self.on_control(control)
    }

    pub fn on_control(&mut self, control: VoiceControlDto) -> Result<Reaction> {
        match control {
            VoiceControlDto::SpeakStart {
                utterance_id,
                sample_rate_hz,
                sample_width_bytes,
                channels,
                ..
            } => {
                // Format negotiation is a *check*, not a conversion. docs/05 §1
                // fixes one v1 format; a daemon offering another has drifted
                // from the contract, and playing it at the wrong rate would be
                // a chipmunk, not an error message.
                if sample_rate_hz != audio::SAMPLE_RATE_HZ
                    || sample_width_bytes != audio::SAMPLE_WIDTH_BYTES
                    || channels != audio::CHANNELS
                {
                    self.speaking = None;
                    anyhow::bail!(
                        "refusing spoken audio in {sample_rate_hz} Hz / {sample_width_bytes}-byte \
                         / {channels}-channel format; this node speaks only {} Hz / {} / {}",
                        audio::SAMPLE_RATE_HZ,
                        audio::SAMPLE_WIDTH_BYTES,
                        audio::CHANNELS
                    );
                }
                // A new utterance supersedes any previous one; whatever is
                // still queued belongs to a turn that is over.
                self.output.flush();
                self.speaking = Some(utterance_id);
                Ok(Reaction::SpeechStarted)
            }
            VoiceControlDto::SpeakStop {
                utterance_id,
                reason,
            } => {
                if self.speaking.as_deref() == Some(utterance_id.as_str()) {
                    self.speaking = None;
                    // Completed audio is allowed to drain; cancelled and failed
                    // are silenced immediately. Barge-in that finishes the
                    // sentence is not barge-in.
                    if reason != VoiceSpeakEndDto::Completed {
                        self.output.flush();
                    }
                }
                Ok(Reaction::SpeechStopped(reason))
            }
            // Inbound stream frames are client→daemon; a daemon sending one is
            // ignored rather than obeyed (mirrors the daemon's own stance).
            VoiceControlDto::StreamStart { .. } | VoiceControlDto::StreamStop { .. } => {
                Ok(Reaction::Handled)
            }
        }
    }

    /// Handles one binary PCM frame from the daemon.
    pub fn on_audio_frame(&mut self, frame: &[u8]) -> Result<Reaction> {
        audio::validate_pcm_frame(frame, MAX_AUDIO_FRAME_BYTES)?;
        if self.speaking.is_none() {
            anyhow::bail!("audio frame outside any utterance; dropped");
        }
        self.output.play(frame)?;
        Ok(Reaction::Handled)
    }

    /// Rings a timer in this room (F8.5).
    ///
    /// Independent of the speech path on purpose: ADR-023 requires an alarm to
    /// sound even with voice services down, so this does not open an utterance
    /// and is not suppressed by one.
    pub fn ring_alert(&mut self) -> Result<()> {
        for frame in alert_tone().chunks(audio::FRAME_BYTES) {
            self.output.play(frame)?;
        }
        Ok(())
    }

    /// Stops playback now — shutdown, revocation, or a socket that died
    /// mid-sentence.
    pub fn silence(&mut self) {
        self.speaking = None;
        self.output.flush();
    }
}

/// A short two-tone alert, synthesised locally (F8.5).
///
/// The daemon sends only the *instruction* to ring — never the audio. The tone
/// is fixed and deterministic, so a node builds its own: that keeps a timer
/// alert a small text frame instead of a per-socket binary stream, and it means
/// a room still rings when nothing about the voice pipeline is working.
pub fn alert_tone() -> Vec<u8> {
    const BEEP_MS: usize = 180;
    const GAP_MS: usize = 120;
    let samples_for = |ms: usize| ms * audio::SAMPLE_RATE_HZ as usize / 1000;

    let mut pcm = Vec::new();
    for (index, hz) in [880.0_f32, 1174.0_f32].into_iter().enumerate() {
        if index > 0 {
            pcm.extend(std::iter::repeat_n(0_i16, samples_for(GAP_MS)));
        }
        for sample in 0..samples_for(BEEP_MS) {
            let t = sample as f32 / audio::SAMPLE_RATE_HZ as f32;
            // Fade the edges so the tone does not click.
            let progress = sample as f32 / samples_for(BEEP_MS) as f32;
            let envelope = (progress * std::f32::consts::PI).sin();
            let value = (t * hz * std::f32::consts::TAU).sin() * envelope * 0.6;
            pcm.push((value * 32767.0) as i16);
        }
    }
    pcm.into_iter().flat_map(i16::to_le_bytes).collect()
}

/// The control frame that opens a capture stream, in the one legal format.
pub fn stream_start(stream_id: &str, session_id: Option<&str>) -> VoiceControlDto {
    VoiceControlDto::StreamStart {
        stream_id: stream_id.to_owned(),
        session_id: session_id.map(str::to_owned),
        sample_rate_hz: audio::SAMPLE_RATE_HZ,
        sample_width_bytes: audio::SAMPLE_WIDTH_BYTES,
        channels: audio::CHANNELS,
    }
}

pub fn stream_stop(stream_id: &str) -> VoiceControlDto {
    VoiceControlDto::StreamStop {
        stream_id: stream_id.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Records what reached the speaker, so a test can assert silence.
    #[derive(Clone, Default)]
    struct FakeOutput {
        played: Arc<Mutex<Vec<Vec<u8>>>>,
        flushes: Arc<Mutex<usize>>,
    }

    impl AudioOutput for FakeOutput {
        fn play(&self, frame: &[u8]) -> Result<()> {
            self.played.lock().expect("lock").push(frame.to_vec());
            Ok(())
        }
        fn flush(&self) {
            *self.flushes.lock().expect("lock") += 1;
        }
        fn describe(&self) -> String {
            "fake".into()
        }
    }

    fn speak_start(utterance: &str) -> VoiceControlDto {
        VoiceControlDto::SpeakStart {
            utterance_id: utterance.to_owned(),
            run_id: None,
            sample_rate_hz: audio::SAMPLE_RATE_HZ,
            sample_width_bytes: audio::SAMPLE_WIDTH_BYTES,
            channels: audio::CHANNELS,
        }
    }

    #[test]
    fn an_utterance_plays_its_frames() {
        let output = FakeOutput::default();
        let mut voice = NodeVoice::new(output.clone());

        assert_eq!(
            voice.on_control(speak_start("u1")).expect("start"),
            Reaction::SpeechStarted
        );
        assert!(voice.is_speaking());
        voice.on_audio_frame(&[1, 2, 3, 4]).expect("frame");
        voice
            .on_control(VoiceControlDto::SpeakStop {
                utterance_id: "u1".into(),
                reason: VoiceSpeakEndDto::Completed,
            })
            .expect("stop");

        assert_eq!(output.played.lock().expect("lock").len(), 1);
        assert!(!voice.is_speaking());
    }

    /// The format is fixed by docs/05 §1. A daemon offering another one is a
    /// contract drift, and playing it would be audible nonsense.
    #[test]
    fn a_format_that_is_not_the_agreed_one_is_refused() {
        let output = FakeOutput::default();
        let mut voice = NodeVoice::new(output.clone());

        let error = voice
            .on_control(VoiceControlDto::SpeakStart {
                utterance_id: "u1".into(),
                run_id: None,
                sample_rate_hz: 44_100,
                sample_width_bytes: 2,
                channels: 2,
            })
            .expect_err("must refuse");
        assert!(
            error.to_string().contains("refusing spoken audio"),
            "{error}"
        );

        assert!(!voice.is_speaking());
        // And nothing plays afterwards, because no utterance was opened.
        assert!(voice.on_audio_frame(&[1, 2]).is_err());
        assert!(output.played.lock().expect("lock").is_empty());
    }

    /// Barge-in: a cancelled utterance is silent immediately, not once the
    /// buffer drains.
    #[test]
    fn a_cancelled_utterance_flushes_but_a_completed_one_drains() {
        let output = FakeOutput::default();
        let mut voice = NodeVoice::new(output.clone());

        voice.on_control(speak_start("u1")).expect("start");
        let flushes_after_start = *output.flushes.lock().expect("lock");
        voice
            .on_control(VoiceControlDto::SpeakStop {
                utterance_id: "u1".into(),
                reason: VoiceSpeakEndDto::Cancelled,
            })
            .expect("stop");
        assert_eq!(
            *output.flushes.lock().expect("lock"),
            flushes_after_start + 1,
            "cancellation must flush"
        );

        voice.on_control(speak_start("u2")).expect("start");
        let before = *output.flushes.lock().expect("lock");
        voice
            .on_control(VoiceControlDto::SpeakStop {
                utterance_id: "u2".into(),
                reason: VoiceSpeakEndDto::Completed,
            })
            .expect("stop");
        assert_eq!(
            *output.flushes.lock().expect("lock"),
            before,
            "a completed utterance must be allowed to finish"
        );
    }

    #[test]
    fn frames_outside_an_utterance_and_malformed_frames_are_dropped() {
        let output = FakeOutput::default();
        let mut voice = NodeVoice::new(output.clone());

        // No `speak.start` yet.
        assert!(voice.on_audio_frame(&[1, 2]).is_err());

        voice.on_control(speak_start("u1")).expect("start");
        // Odd length: not a whole number of samples.
        assert!(voice.on_audio_frame(&[1, 2, 3]).is_err());
        // Absurd size.
        assert!(
            voice
                .on_audio_frame(&vec![0_u8; MAX_AUDIO_FRAME_BYTES + 2])
                .is_err()
        );
        assert!(voice.on_audio_frame(&[]).is_err());

        assert!(output.played.lock().expect("lock").is_empty());
    }

    /// A stop for a different utterance must not silence the current one — the
    /// `utteranceId` scoping is what keeps a cancelled turn from muting its
    /// successor.
    #[test]
    fn a_stop_for_another_utterance_does_not_stop_this_one() {
        let output = FakeOutput::default();
        let mut voice = NodeVoice::new(output.clone());

        voice.on_control(speak_start("u2")).expect("start");
        voice
            .on_control(VoiceControlDto::SpeakStop {
                utterance_id: "u1".into(),
                reason: VoiceSpeakEndDto::Cancelled,
            })
            .expect("stop");

        assert!(voice.is_speaking(), "u2 must still be speaking");
        voice.on_audio_frame(&[1, 2, 3, 4]).expect("still playing");
        assert_eq!(output.played.lock().expect("lock").len(), 1);
    }

    #[test]
    fn a_voice_envelope_from_the_hub_decodes_and_drives_playback() {
        let output = FakeOutput::default();
        let mut voice = NodeVoice::new(output.clone());

        let envelope: EventEnvelope = serde_json::from_value(serde_json::json!({
            "v": 1,
            "seq": 4,
            "channel": "voice",
            "type": "voice.speak.start",
            "occurredAt": "2026-08-13T00:00:00Z",
            "payload": {
                "utteranceId": "u1",
                "sampleRateHz": audio::SAMPLE_RATE_HZ,
                "sampleWidthBytes": audio::SAMPLE_WIDTH_BYTES,
                "channels": audio::CHANNELS,
            }
        }))
        .expect("envelope");

        assert_eq!(
            voice.on_envelope(&envelope).expect("handled"),
            Reaction::SpeechStarted
        );
        assert!(voice.is_speaking());
    }

    /// F8.5: the alert arrives as an instruction, and this node makes the
    /// sound itself. No audio crossed the wire.
    #[test]
    fn a_timer_alert_addressed_here_rings_locally() {
        let output = FakeOutput::default();
        let mut voice = NodeVoice::new(output.clone());

        let envelope: EventEnvelope = serde_json::from_value(serde_json::json!({
            "v": 1, "seq": 9, "channel": "voice",
            "type": "timer.fired", "occurredAt": "2026-08-13T00:00:00Z",
            "payload": {
                "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "name": "pasta timer",
                "targetDeviceId": "01ARZ3NDEKTSV4RRFFQ69G5FB2"
            }
        }))
        .expect("envelope");

        assert_eq!(
            voice.on_envelope(&envelope).expect("handled"),
            Reaction::Rang
        );
        assert!(
            !output.played.lock().expect("lock").is_empty(),
            "the node must synthesise and play the tone itself"
        );
        assert!(
            !voice.is_speaking(),
            "an alert is not an utterance: it must not open one"
        );
    }

    /// ADR-023: an alarm must sound even with voice services down. Ringing does
    /// not depend on — and is not suppressed by — an utterance being in flight.
    #[test]
    fn a_timer_alert_rings_even_while_the_assistant_is_speaking() {
        let output = FakeOutput::default();
        let mut voice = NodeVoice::new(output.clone());
        voice.on_control(speak_start("u1")).expect("start");
        let before = output.played.lock().expect("lock").len();

        voice.ring_alert().expect("rings");

        assert!(output.played.lock().expect("lock").len() > before);
        assert!(voice.is_speaking(), "the utterance is untouched");
    }

    #[test]
    fn the_alert_tone_is_audible_and_bounded() {
        let tone = alert_tone();
        // Whole 16-bit samples, and a sane length: two ~180 ms beeps and a gap.
        assert_eq!(tone.len() % 2, 0);
        let seconds = tone.len() as f32 / 2.0 / audio::SAMPLE_RATE_HZ as f32;
        assert!(
            (0.3..1.0).contains(&seconds),
            "{seconds}s is not a timer beep"
        );
        // Actually audible: something well above silence.
        let peak = tone
            .chunks_exact(2)
            .map(|s| i16::from_le_bytes([s[0], s[1]]).abs())
            .max()
            .expect("samples");
        assert!(peak > 8_000, "the tone must be audible, peak was {peak}");
    }

    #[test]
    fn a_non_voice_envelope_is_ignored() {
        let output = FakeOutput::default();
        let mut voice = NodeVoice::new(output);
        let envelope: EventEnvelope = serde_json::from_value(serde_json::json!({
            "v": 1, "seq": 1, "channel": "display",
            "type": "display.place_surface", "occurredAt": "2026-08-13T00:00:00Z",
            "payload": {"surface": "artifact_canvas", "appId": "x", "monitor": "DP-1"}
        }))
        .expect("envelope");
        assert_eq!(
            voice.on_envelope(&envelope).expect("handled"),
            Reaction::Handled
        );
        assert!(!voice.is_speaking());
    }

    #[test]
    fn the_outbound_control_frames_carry_the_one_legal_format() {
        let VoiceControlDto::StreamStart {
            sample_rate_hz,
            sample_width_bytes,
            channels,
            stream_id,
            ..
        } = stream_start("s1", Some("sess"))
        else {
            panic!("expected a stream start");
        };
        assert_eq!(stream_id, "s1");
        assert_eq!(sample_rate_hz, audio::SAMPLE_RATE_HZ);
        assert_eq!(sample_width_bytes, audio::SAMPLE_WIDTH_BYTES);
        assert_eq!(channels, audio::CHANNELS);
    }
}
