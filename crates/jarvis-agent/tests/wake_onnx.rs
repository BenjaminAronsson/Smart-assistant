//! F8.3's named acceptances, against the real openWakeWord engine.
//!
//! These are the tests the M8a gate report recorded as **unsatisfied**: *"a
//! recorded clip fires once and only once"* and *"silence and household noise
//! do not"*. They run real ONNX inference over real recordings — a scripted
//! detector cannot stand in here, because the claim under test *is* the engine.
//!
//! # Why these tests skip rather than fail without assets
//!
//! ADR-032 consequence 3 forbids vendoring the model assets, so they are not in
//! this repository and CI does not have them. `scripts/fetch-wake-assets.sh`
//! downloads them with pinned checksums; without them these tests skip loudly
//! rather than failing, which keeps "the assets are provisioned, not vendored"
//! from turning into "the engine is never tested".
//!
//! The recordings are openWakeWord's own test clips (16 kHz mono PCM — the same
//! format the node captures), so a positive here is the engine agreeing with
//! its author's fixtures, not with ours.

#![cfg(feature = "wake-word-onnx")]

use std::path::{Path, PathBuf};

use jarvis_agent::audio::FRAME_BYTES;
use jarvis_agent::wake::{Sensitivity, WakeWordDetector};
use jarvis_agent::wake_onnx::OnnxWakeWord;

/// Where `scripts/fetch-wake-assets.sh` puts the models and clips.
fn assets() -> Option<PathBuf> {
    let dir = std::env::var_os("JARVIS_WAKE_TEST_ASSETS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".cache/jarvis-wake-assets")
        });
    dir.join("melspectrogram.onnx").exists().then_some(dir)
}

macro_rules! require_assets {
    () => {
        match assets() {
            Some(dir) => dir,
            None => {
                eprintln!(
                    "SKIP: wake-word assets absent; run scripts/fetch-wake-assets.sh \
                     (ADR-032 consequence 3 — provisioned, never vendored)"
                );
                return;
            }
        }
    };
}

/// Decodes a 16-bit PCM WAV by walking its RIFF chunks.
///
/// Deliberately not a crate: the node's own wire format is exactly this, and a
/// test helper that accepts more than the format under test can hide a
/// mismatch between the two.
fn wav_frames(path: &Path) -> Vec<Vec<u8>> {
    let bytes = std::fs::read(path).expect("test clip readable");
    let mut cursor = 12; // past "RIFF" + size + "WAVE"
    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes(
            bytes[cursor + 4..cursor + 8]
                .try_into()
                .expect("chunk header"),
        ) as usize;
        if id == b"data" {
            return bytes[cursor + 8..cursor + 8 + size]
                .chunks(FRAME_BYTES)
                .map(<[u8]>::to_vec)
                .collect();
        }
        cursor += 8 + size + (size & 1);
    }
    panic!("{} has no data chunk", path.display());
}

/// One second of digital silence, as the node would deliver it.
fn silence(seconds: usize) -> Vec<Vec<u8>> {
    let frames_per_second = 16_000 * 2 / FRAME_BYTES;
    vec![vec![0u8; FRAME_BYTES]; frames_per_second * seconds]
}

/// Feeds frames one at a time, the way `NodeAudio` does, and counts firings.
fn detections(engine: &mut OnnxWakeWord, frames: &[Vec<u8>]) -> usize {
    frames.iter().filter(|frame| engine.accept(frame)).count()
}

fn load(dir: &Path, word: &str) -> OnnxWakeWord {
    OnnxWakeWord::load_from(dir, word, Sensitivity::DEFAULT)
        .unwrap_or_else(|error| panic!("engine for {word:?} should load: {error}"))
}

/// `Result::expect_err` needs `T: Debug`, and a loaded engine holds ONNX
/// sessions that are not. The refusal message is what these tests are about
/// anyway.
fn refusal(dir: &Path, word: &str) -> String {
    match OnnxWakeWord::load_from(dir, word, Sensitivity::DEFAULT) {
        Ok(_) => panic!(
            "loading {word:?} from {} should have been refused",
            dir.display()
        ),
        Err(error) => format!("{error}"),
    }
}

/// A room is quiet, someone says the word, the node hears it — **once**.
///
/// The "and only once" half is the part that needs asserting: the per-word
/// model scores every 80 ms chunk, and a detection stays above threshold for
/// several of them, so an engine without an edge trigger reports a handful of
/// detections for one utterance and the node opens a handful of streams.
#[test]
fn a_recorded_clip_fires_once_and_only_once() {
    let dir = require_assets!();
    let mut engine = load(&dir, "alexa");

    let mut frames = silence(2);
    frames.extend(wav_frames(&dir.join("alexa_test.wav")));
    frames.extend(silence(1));

    assert_eq!(
        detections(&mut engine, &frames),
        1,
        "one utterance of the word must produce exactly one detection"
    );
}

/// The same, for a second word and a second recording — so a pass cannot be an
/// accident of one model file.
#[test]
fn a_second_word_and_recording_also_fires_exactly_once() {
    let dir = require_assets!();
    let mut engine = load(&dir, "hey mycroft");

    let mut frames = silence(2);
    frames.extend(wav_frames(&dir.join("hey_mycroft_test.wav")));
    frames.extend(silence(1));

    assert_eq!(detections(&mut engine, &frames), 1);
}

/// Silence does not fire. The satellite spends almost all of its life here, so
/// a false accept on silence is the failure that would actually be lived with.
#[test]
fn silence_does_not_fire() {
    let dir = require_assets!();
    let mut engine = load(&dir, "alexa");

    assert_eq!(detections(&mut engine, &silence(10)), 0);
}

/// Household noise does not fire.
///
/// `hey_jane.wav` is a person saying a phrase with the same rhythm and opening
/// syllable as a wake word — a far harder negative than white noise, which is
/// why it is the one used. Tested against every model provisioned, so a word
/// the node is *not* configured for cannot answer for it either.
#[test]
fn household_speech_that_is_not_the_word_does_not_fire() {
    let dir = require_assets!();
    let near_miss = wav_frames(&dir.join("hey_jane.wav"));

    for word in ["alexa", "hey mycroft"] {
        let mut engine = load(&dir, word);
        let mut frames = silence(1);
        frames.extend(near_miss.clone());
        frames.extend(silence(1));

        assert_eq!(
            detections(&mut engine, &frames),
            0,
            "{word:?} must not fire on nearby household speech"
        );
    }
}

/// A node configured for one word does not answer to another. This is the
/// property that makes two satellites with different words in one house work,
/// and it is also the strongest available check that the per-word stage is
/// really the thing deciding.
#[test]
fn a_node_does_not_answer_to_a_different_word() {
    let dir = require_assets!();
    let mut engine = load(&dir, "hey mycroft");

    let mut frames = silence(1);
    frames.extend(wav_frames(&dir.join("alexa_test.wav")));
    frames.extend(silence(1));

    assert_eq!(detections(&mut engine, &frames), 0);
}

/// Measures the false-accept rate over a household-noise corpus.
///
/// ADR-032 consequence 2: *"False accepts are a budget to measure, not a claim
/// to assert."* This is the harness that makes the measurement reproducible;
/// the corpus is deliberately **not** in this repository, because a rate
/// measured over audio chosen by the same person who tuned the threshold is not
/// a measurement of anything.
///
/// Point `JARVIS_WAKE_NOISE_CORPUS` at a directory of 16 kHz mono WAV files
/// recorded in the room the node will live in, and this reports accepts per
/// hour. It fails only if the corpus fires more than the budget allows, so it
/// is a measurement in CI's sense as well as the gate's.
#[test]
fn the_false_accept_rate_over_a_noise_corpus_is_within_budget() {
    let dir = require_assets!();
    let Some(corpus) = std::env::var_os("JARVIS_WAKE_NOISE_CORPUS").map(PathBuf::from) else {
        eprintln!(
            "SKIP: no household-noise corpus. Set JARVIS_WAKE_NOISE_CORPUS to a directory of \
             16 kHz mono WAV recordings to produce the ADR-032 consequence 2 measurement."
        );
        return;
    };

    /// One accept per hour of ordinary household audio. Chosen as the point at
    /// which a satellite stops being something people leave switched on.
    const BUDGET_PER_HOUR: f64 = 1.0;

    let word = std::env::var("JARVIS_AGENT_WAKE_WORD").unwrap_or_else(|_| "alexa".to_owned());
    let mut engine = load(&dir, &word);

    let mut frames_seen = 0usize;
    let mut accepts = 0usize;
    let mut clips = 0usize;
    for entry in std::fs::read_dir(&corpus).expect("corpus directory readable") {
        let path = entry.expect("corpus entry").path();
        if path.extension().is_some_and(|e| e == "wav") {
            let frames = wav_frames(&path);
            frames_seen += frames.len();
            accepts += detections(&mut engine, &frames);
            clips += 1;
        }
    }
    assert!(
        clips > 0,
        "corpus {} contains no WAV files",
        corpus.display()
    );

    let hours = (frames_seen * FRAME_BYTES / 2) as f64 / 16_000.0 / 3_600.0;
    let rate = accepts as f64 / hours.max(f64::MIN_POSITIVE);
    println!(
        "false-accept measurement (ADR-032 consequence 2): word={word:?} clips={clips} \
         hours={hours:.3} accepts={accepts} rate={rate:.3}/hour"
    );

    assert!(
        rate <= BUDGET_PER_HOUR,
        "false-accept rate {rate:.3}/hour exceeds the {BUDGET_PER_HOUR}/hour budget"
    );
}

/// A tampered feature extractor is refused rather than used.
///
/// This is the failure mode the pinned checksum exists for: a corrupted or
/// substituted melspectrogram model loads perfectly happily and then produces
/// values the embedding stage was never trained on, so the node silently stops
/// hearing its name with nothing in the log to say why.
#[test]
fn a_tampered_asset_is_refused_by_checksum() {
    let dir = require_assets!();
    let scratch = tempfile::tempdir().expect("tempdir");
    for name in ["melspectrogram.onnx", "embedding_model.onnx", "alexa.onnx"] {
        let source = if name == "alexa.onnx" {
            dir.join("alexa_v0.1.onnx")
        } else {
            dir.join(name)
        };
        std::fs::copy(&source, scratch.path().join(name)).expect("stage asset");
    }
    // Flip the tail of the feature extractor: still a loadable file.
    let path = scratch.path().join("melspectrogram.onnx");
    let mut bytes = std::fs::read(&path).expect("read staged asset");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&path, &bytes).expect("tamper");

    let message = refusal(scratch.path(), "alexa");
    assert!(
        message.contains("pinned checksum"),
        "the refusal must name the reason, got: {message}"
    );
}

/// A wake word with no model says so, in terms the owner can act on.
///
/// The owner's configured word is `"Andy"` (ADR-032 §1) and openWakeWord ships
/// no model for it, so this is the exact message the house will show until one
/// is trained or the word is changed. It must name both options.
#[test]
fn a_word_with_no_model_names_both_ways_out() {
    let dir = require_assets!();

    let message = refusal(&dir, "andy");

    assert!(message.contains("andy"), "name the word: {message}");
    assert!(
        message.contains("trained"),
        "say that a model must be trained: {message}"
    );
    assert!(
        message.contains("JARVIS_AGENT_WAKE_WORD"),
        "say how to choose a word that has one: {message}"
    );
}
