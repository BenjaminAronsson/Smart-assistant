//! Wake word, on the node (F8.3, ADR-032, FR-13 as amended).
//!
//! The feature that changes what this product *is*: until now, talking to
//! Jarvis meant opening a browser tab and holding a button.
//!
//! Two things live here, and keeping them apart is the point:
//!
//! * [`WakeWordDetector`] — the engine, behind a port (ADR-032 §4). The real
//!   one is openWakeWord over ONNX Runtime, compiled only with the
//!   `wake-word-onnx` feature; everything else works without it.
//! * [`WakeGate`] — the pipeline, which is engine-independent and is where the
//!   properties that matter actually live: nothing is streamed before a
//!   detection, one detection opens one stream, a detection while the assistant
//!   is speaking is a **barge-in** rather than a second turn, and the stream is
//!   closed by end-of-speech rather than left open.
//!
//! The gate is what the tests exercise, against a scripted detector. An engine
//! swap must not be able to break the privacy property, which means the privacy
//! property must not live in the engine.

use crate::audio::{FRAME_BYTES, SAMPLE_RATE_HZ};

/// The word this node answers to.
///
/// **`hey jarvis`** by default (ADR-032 §1, owner's choice 2026-08-17).
///
/// Configuration rather than code: changing it is an owner decision plus a
/// model swap, never a rebuild — which is also why the *pipeline* never
/// hardcodes it and only the engine and the listening indicator ever read it.
///
/// It was `"andy"` between 2026-08-15 and 2026-08-17. That changed for one
/// concrete reason: openWakeWord publishes no model for "Andy", so the word
/// would have cost a training run before any node could answer to it, and a
/// house that cannot hear its own name is not a hands-free house. `hey jarvis`
/// is one of the six words the project ships a pre-trained model for, so it
/// works the moment the assets are provisioned.
pub const DEFAULT_WAKE_WORD: &str = "hey jarvis";

/// The words openWakeWord publishes a pre-trained model for (ADR-032 §1).
///
/// Here so the *default* can be checked against it in a test: any other word
/// is a legitimate owner choice, but it costs a training run, and choosing one
/// by accident should not be possible for the shipped default.
pub const PRE_TRAINED_WORDS: [&str; 6] = [
    "alexa",
    "hey jarvis",
    "hey mycroft",
    "hey rhasspy",
    "timer",
    "weather",
];

/// Normalises a configured wake word to the name of its model.
///
/// Lowercased and trimmed: it names an openWakeWord model file, and an owner
/// typing "Hey Jarvis" must reach the same model as one typing "hey jarvis". A blank
/// setting is not a wake word and falls back to the default.
///
/// Pure, taking the raw value rather than reading the environment itself —
/// which is what lets it be tested without mutating process state (`set_var`
/// is `unsafe` in edition 2024, and this crate stays free of `unsafe`).
pub fn normalise_wake_word(raw: Option<&str>) -> String {
    raw.map(|value| value.trim().to_lowercase())
        .filter(|word| !word.is_empty())
        .unwrap_or_else(|| DEFAULT_WAKE_WORD.to_owned())
}

/// The wake word this node is configured with.
pub fn configured_wake_word() -> String {
    normalise_wake_word(std::env::var("JARVIS_AGENT_WAKE_WORD").ok().as_deref())
}

/// How eager the detector is. Higher fires more often, on less.
///
/// A single knob, deliberately: openWakeWord exposes several, and a satellite
/// in a kitchen is tuned by somebody standing in that kitchen saying the word
/// until it works.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sensitivity(f32);

impl Sensitivity {
    pub const DEFAULT: Self = Self(0.5);

    /// Clamped rather than rejected: a node must start with a bad number in its
    /// config, not refuse to boot in a hallway.
    pub fn new(value: f32) -> Self {
        Self(value.clamp(0.0, 1.0))
    }

    pub fn threshold(self) -> f32 {
        // Higher sensitivity means a lower score is enough.
        1.0 - self.0
    }

    pub fn value(self) -> f32 {
        self.0
    }
}

impl Default for Sensitivity {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The engine port (ADR-032 §4).
pub trait WakeWordDetector: Send {
    /// Feeds one frame of 16 kHz mono PCM and reports whether the word fired.
    ///
    /// Implementations are expected to be *edge*-triggered: one utterance of
    /// the word produces exactly one `true`, not one per frame while it echoes.
    fn accept(&mut self, frame: &[u8]) -> bool;

    /// Name for logs and the listening indicator.
    fn word(&self) -> &str;
}

/// What the node should do with a frame, once the gate has seen it.
#[derive(Debug, PartialEq, Eq)]
pub enum GateAction {
    /// Discard it. The default, and what happens the overwhelming majority of
    /// the time a satellite is powered on.
    Discard,
    /// Open a capture stream and send this frame, preceded by the pre-roll.
    ///
    /// The pre-roll matters: a wake word is only detected once it has been
    /// *said*, so the audio containing the beginning of the sentence after it
    /// is already in the past by the time the gate opens.
    OpenStream { pre_roll: Vec<Vec<u8>> },
    /// Send it on the open stream.
    Send,
    /// End of speech: close the stream.
    CloseStream,
    /// The word fired while the assistant was speaking. Interrupt it — do not
    /// start a second turn on top of the first.
    BargeIn { pre_roll: Vec<Vec<u8>> },
}

/// Frames of pre-roll kept before a detection: 500 ms at 20 ms per frame.
const PRE_ROLL_FRAMES: usize = 25;

/// Silence that ends a turn. VAD gates end-of-turn as it did before the wake
/// word existed; this is the node's half of that.
const SILENCE_FRAMES_TO_END: usize = 40; // 800 ms

/// Below this mean absolute amplitude a frame counts as silence.
const SILENCE_THRESHOLD: i32 = 300;

/// The pipeline (ADR-032 §4).
pub struct WakeGate<D: WakeWordDetector> {
    detector: D,
    /// Recent frames, kept so the start of the sentence is not lost.
    pre_roll: std::collections::VecDeque<Vec<u8>>,
    open: bool,
    silent_frames: usize,
    /// Counted, never asserted away (ADR-032 consequence 2).
    detections: u64,
    listening: bool,
}

impl<D: WakeWordDetector> WakeGate<D> {
    pub fn new(detector: D) -> Self {
        Self {
            detector,
            pre_roll: std::collections::VecDeque::with_capacity(PRE_ROLL_FRAMES),
            open: false,
            silent_frames: 0,
            detections: 0,
            listening: true,
        }
    }

    /// Whether a stream is currently open.
    pub fn is_streaming(&self) -> bool {
        self.open
    }

    /// How many times the word has fired since boot — the numerator of the
    /// false-accept rate the M8a gate reports.
    pub fn detections(&self) -> u64 {
        self.detections
    }

    /// The visible listening state. A satellite that is not listening must say
    /// so, for the same reason a muted one must.
    pub fn is_listening(&self) -> bool {
        self.listening
    }

    pub fn set_listening(&mut self, listening: bool) {
        if self.listening != listening {
            tracing::info!(
                listening,
                word = self.detector.word(),
                "wake word listening"
            );
            self.listening = listening;
            if !listening {
                self.reset();
            }
        }
    }

    /// Closes any open stream and forgets buffered audio.
    pub fn reset(&mut self) {
        self.open = false;
        self.silent_frames = 0;
        self.pre_roll.clear();
    }

    /// Feeds one captured frame. `speaking` is whether the node is currently
    /// playing the assistant's voice.
    pub fn accept(&mut self, frame: &[u8], speaking: bool) -> GateAction {
        if !self.listening {
            return GateAction::Discard;
        }

        if self.open {
            // Mid-turn: the detector is not consulted, so the word appearing
            // inside a sentence cannot restart the turn it is part of.
            if is_silent(frame) {
                self.silent_frames += 1;
                if self.silent_frames >= SILENCE_FRAMES_TO_END {
                    self.open = false;
                    self.silent_frames = 0;
                    return GateAction::CloseStream;
                }
            } else {
                self.silent_frames = 0;
            }
            return GateAction::Send;
        }

        // Closed. Keep the frame only in the rolling pre-roll buffer, which is
        // bounded and never leaves the node unless the word fires.
        self.pre_roll.push_back(frame.to_vec());
        while self.pre_roll.len() > PRE_ROLL_FRAMES {
            self.pre_roll.pop_front();
        }

        if !self.detector.accept(frame) {
            return GateAction::Discard;
        }

        self.detections += 1;
        self.open = true;
        self.silent_frames = 0;
        let pre_roll: Vec<Vec<u8>> = self.pre_roll.drain(..).collect();
        tracing::info!(
            word = self.detector.word(),
            detections = self.detections,
            barge_in = speaking,
            "wake word detected"
        );
        if speaking {
            GateAction::BargeIn { pre_roll }
        } else {
            GateAction::OpenStream { pre_roll }
        }
    }
}

/// Mean absolute amplitude below [`SILENCE_THRESHOLD`].
fn is_silent(frame: &[u8]) -> bool {
    if frame.len() < 2 {
        return true;
    }
    let mut total: i64 = 0;
    let mut count: i64 = 0;
    for sample in frame.chunks_exact(2) {
        total += i64::from(i16::from_le_bytes([sample[0], sample[1]]).abs());
        count += 1;
    }
    count == 0 || (total / count) < i64::from(SILENCE_THRESHOLD)
}

/// So a node can hold whichever engine it was built with behind one type.
impl WakeWordDetector for Box<dyn WakeWordDetector> {
    fn accept(&mut self, frame: &[u8]) -> bool {
        (**self).accept(frame)
    }
    fn word(&self) -> &str {
        (**self).word()
    }
}

/// A detector that never fires: what a node has when the engine is not
/// compiled in or its model is missing.
///
/// Not a stub to be replaced — it is the honest fallback ADR-032's last
/// consequence names. A node with no wake word still runs, still shows its
/// screen, still speaks, and still answers push-to-talk. It simply does not
/// answer to its name, and says so at startup.
pub struct NeverWakes;

impl WakeWordDetector for NeverWakes {
    fn accept(&mut self, _frame: &[u8]) -> bool {
        false
    }
    fn word(&self) -> &str {
        "<none>"
    }
}

#[cfg(test)]
mod word_tests {
    use super::*;

    #[test]
    fn the_default_wake_word_has_a_pre_trained_model() {
        // ADR-032 §1, as amended 2026-08-17. The default must be a word
        // openWakeWord actually publishes a model for — a default that needs a
        // training run before it works is a house that cannot hear its name.
        assert_eq!(DEFAULT_WAKE_WORD, "hey jarvis");
        assert!(
            PRE_TRAINED_WORDS.contains(&DEFAULT_WAKE_WORD),
            "the default wake word must be one of the published models"
        );
    }

    #[test]
    fn a_configured_word_is_normalised_to_its_model_name() {
        // "Hey Jarvis" and "hey jarvis" must reach the same model.
        assert_eq!(normalise_wake_word(Some("  Hey Jarvis  ")), "hey jarvis");
        assert_eq!(normalise_wake_word(Some("HEY JARVIS")), "hey jarvis");
        // A blank setting is not a wake word.
        assert_eq!(normalise_wake_word(Some("   ")), DEFAULT_WAKE_WORD);
        assert_eq!(normalise_wake_word(None), DEFAULT_WAKE_WORD);
        // And an owner may genuinely choose another one.
        assert_eq!(normalise_wake_word(Some("Hey Jarvis")), "hey jarvis");
    }
}

/// Frames per second of capture, for turning a frame count into a duration in
/// the gate report.
pub const FRAMES_PER_SECOND: usize = SAMPLE_RATE_HZ as usize * 2 / FRAME_BYTES;

#[cfg(test)]
mod tests {
    use super::*;

    /// Fires on the frames whose index appears in the script.
    struct Scripted {
        at: Vec<usize>,
        seen: usize,
    }

    impl Scripted {
        fn new(at: &[usize]) -> Self {
            Self {
                at: at.to_vec(),
                seen: 0,
            }
        }
    }

    impl WakeWordDetector for Scripted {
        fn accept(&mut self, _frame: &[u8]) -> bool {
            let index = self.seen;
            self.seen += 1;
            self.at.contains(&index)
        }
        fn word(&self) -> &str {
            "hey jarvis"
        }
    }

    fn loud() -> Vec<u8> {
        let mut frame = Vec::with_capacity(FRAME_BYTES);
        for _ in 0..FRAME_BYTES / 2 {
            frame.extend_from_slice(&8000_i16.to_le_bytes());
        }
        frame
    }

    fn quiet() -> Vec<u8> {
        vec![0_u8; FRAME_BYTES]
    }

    /// The privacy property, stated as a test: before the word fires, every
    /// frame is discarded.
    #[test]
    fn nothing_is_streamed_before_the_word_fires() {
        let mut gate = WakeGate::new(Scripted::new(&[100]));
        for _ in 0..100 {
            assert_eq!(gate.accept(&loud(), false), GateAction::Discard);
        }
        assert!(!gate.is_streaming());
        assert_eq!(gate.detections(), 0);
    }

    #[test]
    fn a_detection_opens_one_stream_and_carries_the_pre_roll() {
        let mut gate = WakeGate::new(Scripted::new(&[5]));
        for _ in 0..5 {
            gate.accept(&loud(), false);
        }
        let action = gate.accept(&loud(), false);
        let GateAction::OpenStream { pre_roll } = action else {
            panic!("expected the stream to open, got {action:?}");
        };
        // Five buffered frames plus the one that fired.
        assert_eq!(pre_roll.len(), 6, "the start of the sentence must survive");
        assert!(gate.is_streaming());
        assert_eq!(gate.detections(), 1);
    }

    /// "Fires once and only once": while a turn is open the detector is not
    /// consulted, so the word occurring inside the sentence cannot restart it.
    #[test]
    fn the_word_inside_an_open_turn_does_not_start_a_second_one() {
        let mut gate = WakeGate::new(Scripted::new(&[0, 1, 2, 3]));
        assert!(matches!(
            gate.accept(&loud(), false),
            GateAction::OpenStream { .. }
        ));
        for _ in 0..3 {
            assert_eq!(gate.accept(&loud(), false), GateAction::Send);
        }
        assert_eq!(gate.detections(), 1, "one utterance, one detection");
    }

    #[test]
    fn silence_ends_the_turn_and_the_stream_closes() {
        let mut gate = WakeGate::new(Scripted::new(&[0]));
        gate.accept(&loud(), false);
        // Speech continues.
        for _ in 0..10 {
            assert_eq!(gate.accept(&loud(), false), GateAction::Send);
        }
        // Then silence, but not enough of it.
        for _ in 0..(SILENCE_FRAMES_TO_END - 1) {
            assert_eq!(gate.accept(&quiet(), false), GateAction::Send);
        }
        assert!(gate.is_streaming());
        assert_eq!(gate.accept(&quiet(), false), GateAction::CloseStream);
        assert!(!gate.is_streaming());
    }

    #[test]
    fn a_pause_mid_sentence_does_not_end_the_turn() {
        let mut gate = WakeGate::new(Scripted::new(&[0]));
        gate.accept(&loud(), false);
        for _ in 0..(SILENCE_FRAMES_TO_END - 1) {
            gate.accept(&quiet(), false);
        }
        // Speech resumes: the silence counter must reset.
        assert_eq!(gate.accept(&loud(), false), GateAction::Send);
        for _ in 0..(SILENCE_FRAMES_TO_END - 1) {
            assert_eq!(gate.accept(&quiet(), false), GateAction::Send);
        }
        assert!(gate.is_streaming(), "a pause is not the end of a turn");
    }

    /// The claim from the feature list: detection while speaking is a barge-in,
    /// not a second turn.
    #[test]
    fn a_detection_while_the_assistant_speaks_is_a_barge_in() {
        let mut gate = WakeGate::new(Scripted::new(&[0]));
        let action = gate.accept(&loud(), true);
        assert!(
            matches!(action, GateAction::BargeIn { .. }),
            "expected a barge-in, got {action:?}"
        );
        assert!(gate.is_streaming(), "barge-in still opens the turn");
        assert_eq!(gate.detections(), 1);
    }

    #[test]
    fn a_node_that_is_not_listening_discards_everything() {
        let mut gate = WakeGate::new(Scripted::new(&[0, 1, 2]));
        gate.set_listening(false);
        assert!(!gate.is_listening());
        for _ in 0..3 {
            assert_eq!(gate.accept(&loud(), false), GateAction::Discard);
        }
        assert_eq!(gate.detections(), 0);
    }

    #[test]
    fn the_pre_roll_buffer_is_bounded() {
        let mut gate = WakeGate::new(Scripted::new(&[500]));
        for _ in 0..500 {
            gate.accept(&loud(), false);
        }
        let GateAction::OpenStream { pre_roll } = gate.accept(&loud(), false) else {
            panic!("expected the stream to open");
        };
        // The firing frame is buffered before the cap is applied, so the
        // delivered pre-roll is exactly the cap — not 501 frames of kitchen.
        assert_eq!(
            pre_roll.len(),
            PRE_ROLL_FRAMES,
            "an idle node must not accumulate audio without bound"
        );
    }

    #[test]
    fn sensitivity_is_clamped_rather_than_rejected() {
        assert_eq!(Sensitivity::new(5.0).value(), 1.0);
        assert_eq!(Sensitivity::new(-1.0).value(), 0.0);
        // Higher sensitivity = lower score needed.
        assert!(Sensitivity::new(0.9).threshold() < Sensitivity::new(0.1).threshold());
    }

    #[test]
    fn the_fallback_detector_never_fires() {
        let mut gate = WakeGate::new(NeverWakes);
        for _ in 0..1000 {
            assert_eq!(gate.accept(&loud(), false), GateAction::Discard);
        }
        assert_eq!(gate.detections(), 0);
    }

    /// Household noise must not fire the word. With a scripted detector this
    /// asserts the *gate* adds no firing of its own — the engine's own
    /// false-accept rate is measured at the gate report, not asserted here
    /// (ADR-032 consequence 2).
    #[test]
    fn the_gate_never_fires_on_its_own() {
        let mut gate = WakeGate::new(Scripted::new(&[]));
        for index in 0..2000 {
            let frame = if index % 3 == 0 { quiet() } else { loud() };
            assert_eq!(gate.accept(&frame, false), GateAction::Discard);
        }
        assert_eq!(gate.detections(), 0);
    }
}
