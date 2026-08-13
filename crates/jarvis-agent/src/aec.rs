//! Echo cancellation and on-device barge-in (F8.4, FR-13).
//!
//! A satellite with a speaker beside its microphone hears itself. Server-side
//! barge-in cannot fix that, because the interruption *is* the assistant's own
//! voice: the daemon has no way to tell "the owner interrupted" from "the
//! microphone picked up the sentence we are currently speaking".
//!
//! Two mechanisms, and the second is the one that must never fail:
//!
//! * [`EchoCanceller`] — an NLMS adaptive filter. It knows what was sent to the
//!   speaker, so it can estimate what the microphone will hear of it and
//!   subtract that estimate. When it converges, the owner can interrupt by
//!   voice while the assistant is talking.
//! * [`HalfDuplex`] — the floor. While the node is speaking (plus a short
//!   tail), wake detection is suppressed outright. It costs barge-in-by-voice
//!   and it *guarantees* the node cannot trigger itself, which is the failure
//!   that turns a kitchen speaker into an infinite loop.
//!
//! Pure Rust and no new dependency, deliberately. `speexdsp` and
//! `webrtc-audio-processing` are better cancellers and both are C libraries
//! with build systems; NLMS is a hundred lines, is testable against a synthetic
//! echo, and — critically — the *correctness* of the feature rests on
//! [`HalfDuplex`], which cannot fail to converge.

use crate::audio::SAMPLE_RATE_HZ;

/// Adaptive filter length in samples: 128 ms at 16 kHz.
///
/// Long enough for the acoustic path of a small room (a speaker and a
/// microphone on the same device, plus one or two reflections), short enough
/// that the filter converges in seconds rather than minutes.
const FILTER_TAPS: usize = 2048;

/// NLMS step size. Below 1.0 for stability; 0.3 converges in a few seconds of
/// speech without the filter chasing noise.
const STEP_SIZE: f32 = 0.3;

/// Guards the divide in the NLMS update when the reference is silent.
const REGULARISATION: f32 = 1e-6;

/// Cancels the node's own playback out of what its microphone hears.
///
/// The reference signal is exactly the PCM this node sent to its speaker, which
/// is why this lives on the node: nothing upstream knows what a given room
/// actually played, or when.
pub struct EchoCanceller {
    /// Adaptive filter coefficients.
    weights: Vec<f32>,
    /// Most recent reference samples, newest last.
    reference: std::collections::VecDeque<f32>,
    enabled: bool,
}

impl Default for EchoCanceller {
    fn default() -> Self {
        Self::new()
    }
}

impl EchoCanceller {
    pub fn new() -> Self {
        Self {
            weights: vec![0.0; FILTER_TAPS],
            reference: std::collections::VecDeque::from(vec![0.0; FILTER_TAPS]),
            enabled: true,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Records what was sent to the speaker. Must be called with every played
    /// frame, whether or not the microphone is currently being processed —
    /// otherwise the filter's idea of "when" drifts from the room's.
    pub fn observe_playback(&mut self, frame: &[u8]) {
        for sample in frame.chunks_exact(2) {
            self.reference
                .push_back(f32::from(i16::from_le_bytes([sample[0], sample[1]])) / 32768.0);
        }
        while self.reference.len() > FILTER_TAPS * 4 {
            self.reference.pop_front();
        }
    }

    /// Subtracts the estimated echo from one microphone frame.
    ///
    /// Returns the residual — what is left after the node's own voice is
    /// removed, which is the owner if they are speaking and near-silence if
    /// they are not.
    pub fn process(&mut self, mic: &[u8]) -> Vec<u8> {
        if !self.enabled || self.reference.len() < FILTER_TAPS {
            return mic.to_vec();
        }
        let samples: Vec<f32> = mic
            .chunks_exact(2)
            .map(|s| f32::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0)
            .collect();

        let mut out = Vec::with_capacity(mic.len());
        for (index, sample) in samples.iter().enumerate() {
            // The slice of reference history aligned with this sample.
            let end = self
                .reference
                .len()
                .saturating_sub(samples.len().saturating_sub(index));
            if end < FILTER_TAPS {
                out.extend_from_slice(&to_i16(*sample).to_le_bytes());
                continue;
            }
            let start = end - FILTER_TAPS;

            let mut estimate = 0.0_f32;
            let mut energy = 0.0_f32;
            for tap in 0..FILTER_TAPS {
                let reference = self.reference[start + tap];
                estimate += self.weights[tap] * reference;
                energy += reference * reference;
            }

            let residual = sample - estimate;
            // NLMS update, normalised by reference energy so the step is
            // independent of how loud the assistant happens to be speaking.
            let scale = STEP_SIZE * residual / (energy + REGULARISATION);
            for tap in 0..FILTER_TAPS {
                self.weights[tap] += scale * self.reference[start + tap];
            }
            out.extend_from_slice(&to_i16(residual).to_le_bytes());
        }
        out
    }
}

/// The inverse of the `/ 32768.0` used on the way in.
///
/// Both directions must use the same scale, or a sample that passes through
/// untouched still comes out one LSB different — which would mean the canceller
/// quietly degrades every frame it does not need to change.
fn to_i16(sample: f32) -> i16 {
    (sample * 32768.0).clamp(-32768.0, 32767.0) as i16
}

/// Mean energy of a PCM frame, normalised to 0.0–1.0.
pub fn frame_energy(frame: &[u8]) -> f32 {
    let mut total = 0.0_f32;
    let mut count = 0.0_f32;
    for sample in frame.chunks_exact(2) {
        let value = f32::from(i16::from_le_bytes([sample[0], sample[1]])) / 32768.0;
        total += value * value;
        count += 1.0;
    }
    if count == 0.0 {
        0.0
    } else {
        (total / count).sqrt()
    }
}

/// Frames of continued suppression after playback stops.
///
/// The room keeps ringing for a moment, and a node that unmutes its detector
/// the instant the last sample is queued will hear its own tail.
const TAIL_FRAMES: usize = 15; // 300 ms

/// The guarantee that does not depend on an adaptive filter converging.
///
/// While the node is speaking, and for a short tail afterwards, wake detection
/// is suppressed. With [`EchoCanceller`] working this is belt-and-braces; with
/// it absent or diverged, it is the whole defence, and the node degrades to
/// push-to-talk during playback rather than looping on its own voice.
pub struct HalfDuplex {
    tail: usize,
    /// How much louder than the residual echo floor the room has to be before
    /// a voice is believed. 0.0 disables the check.
    duck_threshold: f32,
}

impl Default for HalfDuplex {
    fn default() -> Self {
        Self {
            tail: 0,
            duck_threshold: 0.02,
        }
    }
}

impl HalfDuplex {
    /// Whether the detector may run on this frame.
    ///
    /// `speaking` is whether playback is currently active; `residual_energy` is
    /// the energy left after echo cancellation.
    pub fn allows_detection(&mut self, speaking: bool, residual_energy: f32) -> bool {
        if speaking {
            self.tail = TAIL_FRAMES;
            // With a converged canceller, a genuine interruption still stands
            // clearly above the residual floor — that is what makes barge-in
            // survive a loud speaker. Without one, the residual is the
            // assistant's own voice at full volume, which is far above the
            // threshold and would self-trigger — so `aec_enabled` gates this
            // path in `should_detect`.
            return residual_energy > self.duck_threshold;
        }
        if self.tail > 0 {
            self.tail -= 1;
            return false;
        }
        true
    }

    /// The whole decision, including the degradation rule.
    ///
    /// Without echo cancellation there is no safe way to listen while speaking,
    /// so the node does not: it waits. That is worse UX and a *correct*
    /// failure, and it is what the feature means by "degrades to push-to-talk
    /// rather than looping".
    pub fn should_detect(
        &mut self,
        speaking: bool,
        residual_energy: f32,
        aec_enabled: bool,
    ) -> bool {
        if speaking && !aec_enabled {
            self.tail = TAIL_FRAMES;
            return false;
        }
        self.allows_detection(speaking, residual_energy)
    }
}

/// Playback gain while the microphone is being listened to (ducking).
///
/// Not silence: the assistant keeps talking until it is actually interrupted,
/// it just stops shouting over the person trying to interrupt it.
pub const DUCKED_GAIN: f32 = 0.35;

/// Applies a gain to a PCM frame, for ducking.
pub fn apply_gain(frame: &[u8], gain: f32) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len());
    for sample in frame.chunks_exact(2) {
        let value = f32::from(i16::from_le_bytes([sample[0], sample[1]])) * gain;
        out.extend_from_slice(&(value.clamp(-32768.0, 32767.0) as i16).to_le_bytes());
    }
    out
}

/// Frames per second, for turning tail lengths into durations in a report.
pub const FRAMES_PER_SECOND: f32 = SAMPLE_RATE_HZ as f32 / 320.0;

#[cfg(test)]
mod tests {
    use super::*;

    /// A tone, as PCM.
    fn tone(samples: usize, frequency: f32, amplitude: f32) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples * 2);
        for index in 0..samples {
            let t = index as f32 / SAMPLE_RATE_HZ as f32;
            let value = (t * frequency * std::f32::consts::TAU).sin() * amplitude;
            out.extend_from_slice(&to_i16(value).to_le_bytes());
        }
        out
    }

    fn silence(samples: usize) -> Vec<u8> {
        vec![0_u8; samples * 2]
    }

    /// The core claim: given what it played, the node can subtract what it
    /// hears of itself. Asserted as an energy reduction against a synthetic
    /// echo, which is the only way to check convergence without a room.
    #[test]
    fn the_canceller_converges_and_removes_a_synthetic_echo() {
        let mut aec = EchoCanceller::new();
        let mut residual_energy = 0.0;
        let mut echo_energy = 0.0;

        // Feed a few seconds of speech-like reference, with the microphone
        // hearing a scaled copy (the acoustic path, simplified to a gain).
        for _ in 0..300 {
            let played = tone(320, 300.0, 0.6);
            aec.observe_playback(&played);
            // What the microphone hears: the same signal, quieter.
            let heard = apply_gain(&played, 0.5);
            echo_energy = frame_energy(&heard);
            residual_energy = frame_energy(&aec.process(&heard));
        }

        assert!(
            residual_energy < echo_energy * 0.3,
            "the canceller must remove most of the echo: {residual_energy} vs {echo_energy}"
        );
    }

    /// The property that keeps a kitchen speaker from talking to itself: with
    /// no canceller, the node simply does not listen while it speaks.
    #[test]
    fn without_cancellation_the_node_does_not_listen_while_speaking() {
        let mut duplex = HalfDuplex::default();
        // The microphone is hearing the assistant at full volume.
        for _ in 0..50 {
            assert!(
                !duplex.should_detect(true, 0.9, false),
                "a node with no AEC must not run its detector while speaking"
            );
        }
    }

    /// …and it keeps not listening for a moment afterwards, because the room
    /// is still ringing.
    #[test]
    fn suppression_continues_for_a_tail_after_playback_stops() {
        let mut duplex = HalfDuplex::default();
        duplex.should_detect(true, 0.9, false);
        for _ in 0..TAIL_FRAMES {
            assert!(
                !duplex.should_detect(false, 0.9, false),
                "tail must suppress"
            );
        }
        assert!(
            duplex.should_detect(false, 0.9, false),
            "and then listening resumes"
        );
    }

    /// With cancellation working, a genuine interruption is heard over the
    /// assistant's own voice — that is what barge-in-by-voice requires.
    #[test]
    fn with_cancellation_a_real_interruption_is_still_heard_while_speaking() {
        let mut duplex = HalfDuplex::default();
        // Residual is near the floor: only the assistant's own echo remains.
        assert!(
            !duplex.should_detect(true, 0.001, true),
            "the assistant's own residual must not count as an interruption"
        );
        // Somebody actually speaks.
        assert!(
            duplex.should_detect(true, 0.4, true),
            "a real voice over the residual floor must be heard"
        );
    }

    #[test]
    fn a_disabled_canceller_passes_audio_through_untouched() {
        let mut aec = EchoCanceller::new();
        aec.set_enabled(false);
        let frame = tone(320, 440.0, 0.5);
        assert_eq!(aec.process(&frame), frame);
        assert!(!aec.is_enabled());
    }

    #[test]
    fn the_canceller_is_a_no_op_before_it_has_a_reference() {
        let mut aec = EchoCanceller::new();
        let frame = tone(320, 440.0, 0.5);
        // No playback observed yet: nothing to subtract, and it must not
        // mangle the owner's voice while it waits.
        assert_eq!(aec.process(&frame), frame);
    }

    #[test]
    fn ducking_lowers_playback_without_silencing_it() {
        let frame = tone(320, 440.0, 0.8);
        let ducked = apply_gain(&frame, DUCKED_GAIN);
        let full = frame_energy(&frame);
        let quiet = frame_energy(&ducked);
        assert!(quiet < full, "ducking must lower the level");
        assert!(quiet > 0.0, "ducking is not muting: it keeps talking");
    }

    #[test]
    fn silence_has_no_energy_and_does_not_divide_by_zero() {
        assert_eq!(frame_energy(&silence(320)), 0.0);
        assert_eq!(frame_energy(&[]), 0.0);
        let mut aec = EchoCanceller::new();
        for _ in 0..10 {
            aec.observe_playback(&silence(320));
            let _ = aec.process(&silence(320));
        }
    }
}
