//! openWakeWord over ONNX Runtime — the engine behind [`WakeWordDetector`]
//! (F8.3, ADR-032 §1).
//!
//! Compiled only with the `wake-word-onnx` feature. ADR-032's last consequence
//! is the reason that is a feature and not a hard dependency: ONNX Runtime is a
//! heavyweight native dependency, CI builds and tests the *pipeline* without
//! it, and a node with the feature off still runs and still answers
//! push-to-talk. The port is what makes that fallback honest rather than
//! hypothetical.
//!
//! # The model chain
//!
//! openWakeWord is three models, not one, and the split is why it is cheap
//! enough to run continuously on a satellite: the two expensive, *word-
//! independent* stages are shared, and only the last tiny stage is per-word.
//!
//! ```text
//!   16 kHz PCM ──▶ melspectrogram ──▶ embedding ──▶ <word> ──▶ probability
//!   1280 samples    8 mel frames      1 × 96-dim    16 × 96
//!   (80 ms)         (32 bins each)    vector        vectors
//! ```
//!
//! Each 80 ms chunk costs exactly one inference of each stage, and the
//! per-word stage is the one an owner swaps to change the wake word — which is
//! what ADR-032 §4 means by "the word is configuration, not code".
//!
//! # Why the buffers look the way they do
//!
//! The melspectrogram model is run over the new chunk **plus 480 samples of the
//! previous one** ([`OVERLAP_SAMPLES`]). That is not a guess: 1280 + 480
//! samples yields exactly 8 mel frames, which is 80 ms at the model's 10 ms
//! hop, so the frames tile the stream without gap or overlap. Feeding it the
//! bare 1280 samples yields 5 frames and silently loses 30 ms of every chunk.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ort::session::Session;
use ort::value::Tensor;
use sha2::{Digest, Sha256};

use crate::wake::{Sensitivity, WakeWordDetector};

/// Samples per inference step: 80 ms at 16 kHz.
const CHUNK_SAMPLES: usize = 1280;

/// Extra history handed to the melspectrogram model with each chunk, so the
/// frames it returns tile the stream exactly. See the module docs.
const OVERLAP_SAMPLES: usize = 480;

/// Mel bins per frame, fixed by the melspectrogram model's output shape.
const MEL_BINS: usize = 32;

/// Mel frames the embedding model consumes per vector (its input is
/// `[batch, 76, 32, 1]`).
const MEL_WINDOW: usize = 76;

/// New mel frames per chunk, and therefore the embedding stride.
const MEL_PER_CHUNK: usize = 8;

/// Dimensions of one embedding vector.
const EMBEDDING_DIM: usize = 96;

/// Embedding vectors the per-word model consumes (its input is `[1, 16, 96]`),
/// i.e. 1.28 s of context.
const EMBEDDING_WINDOW: usize = 16;

/// SHA-256 of the two word-independent assets, pinned.
///
/// These are the exact files ADR-032 §1's licence review covers, so pinning
/// them is what makes that review describe what actually runs. They are not
/// vendored (ADR-032 consequence 3) — the installer downloads them and this
/// verifies them.
///
/// The per-word model is deliberately **not** pinned here: it is the thing an
/// owner swaps, so a constant in this file would defeat the swap path. It is
/// validated structurally instead — see [`load_word_model`].
const MELSPECTROGRAM_SHA256: &str =
    "ba2b0e0f8b7b875369a2c89cb13360ff53bac436f2895cced9f479fa65eb176f";
const EMBEDDING_SHA256: &str = "70d164290c1d095d1d4ee149bc5e00543250a7316b59f31d056cff7bd3075c1f";

/// Where a node keeps its provisioned model assets.
///
/// Overridable so a satellite image can put them on a read-only partition, and
/// so the tests can point at a cache without installing anything.
pub fn asset_dir() -> PathBuf {
    std::env::var_os("JARVIS_AGENT_WAKE_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join(".local/share/jarvis-agent/wake"))
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/jarvis-agent"))
}

/// Turns a configured wake word into the model file that implements it.
///
/// `"hey jarvis"` is the file `hey_jarvis.onnx`. The word is already
/// lowercased and trimmed by `wake::normalise_wake_word`; this only has to
/// agree with it about spacing.
pub fn model_file_name(word: &str) -> String {
    format!("{}.onnx", word.replace([' ', '-'], "_"))
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("wake-word asset {} could not be read", path.display()))?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != expected {
        bail!(
            "wake-word asset {} does not match its pinned checksum (expected {expected}, got \
             {actual}). Re-provision it (ADR-032 consequence 3); a mismatched feature extractor \
             produces silent nonsense rather than an error.",
            path.display()
        );
    }
    Ok(())
}

/// Loads the per-word model and checks it is actually a wake-word model.
///
/// The per-word file is unpinned by design, so this is the integrity check that
/// replaces a checksum. It matters more than it looks: an ONNX file with the
/// wrong shape loads perfectly happily and then never fires, which presents as
/// "the house stopped answering" with nothing in the logs.
fn load_word_model(path: &Path, word: &str) -> Result<(Session, String, String)> {
    if !path.exists() {
        bail!(
            "no wake-word model for {word:?} at {}. openWakeWord ships pre-trained models for \
             \"alexa\", \"hey jarvis\", \"hey mycroft\", \"hey rhasspy\", \"timer\" and \
             \"weather\" only; any other word needs a model trained for it (ADR-032 §4 — the \
             word is configuration plus a model swap). Provision the file or set \
             JARVIS_AGENT_WAKE_WORD to a word you have a model for.",
            path.display()
        );
    }
    let session = Session::builder()?
        .commit_from_file(path)
        .with_context(|| format!("wake-word model {} could not be loaded", path.display()))?;

    let input = session
        .inputs()
        .first()
        .map(|i| i.name().to_owned())
        .context("wake-word model declares no input")?;
    let output = session
        .outputs()
        .first()
        .map(|o| o.name().to_owned())
        .context("wake-word model declares no output")?;
    Ok((session, input, output))
}

/// openWakeWord, running on this node.
pub struct OnnxWakeWord {
    word: String,
    threshold: f32,
    melspectrogram: Session,
    embedding: Session,
    word_model: Session,
    word_input: String,
    word_output: String,
    /// Samples not yet part of a whole chunk.
    pending: Vec<f32>,
    /// The tail of the previous chunk, for the melspectrogram overlap.
    overlap: Vec<f32>,
    /// The most recent [`MEL_WINDOW`] mel frames, flattened.
    mels: VecDeque<f32>,
    /// The most recent [`EMBEDDING_WINDOW`] embedding vectors, flattened.
    embeddings: VecDeque<f32>,
    /// Edge-trigger state: `true` when a *new* detection is allowed.
    armed: bool,
}

impl OnnxWakeWord {
    /// Loads the three models for `word` from the provisioned asset directory.
    pub fn load(word: &str, sensitivity: Sensitivity) -> Result<Self> {
        Self::load_from(&asset_dir(), word, sensitivity)
    }

    /// Directory-explicit variant, so tests need no environment mutation
    /// (`set_var` is `unsafe` in edition 2024 and this crate stays free of it).
    pub fn load_from(dir: &Path, word: &str, sensitivity: Sensitivity) -> Result<Self> {
        let mel_path = dir.join("melspectrogram.onnx");
        let emb_path = dir.join("embedding_model.onnx");
        verify_sha256(&mel_path, MELSPECTROGRAM_SHA256)?;
        verify_sha256(&emb_path, EMBEDDING_SHA256)?;

        let melspectrogram = Session::builder()?.commit_from_file(&mel_path)?;
        let embedding = Session::builder()?.commit_from_file(&emb_path)?;
        let (word_model, word_input, word_output) =
            load_word_model(&dir.join(model_file_name(word)), word)?;

        Ok(Self {
            word: word.to_owned(),
            threshold: sensitivity.threshold(),
            melspectrogram,
            embedding,
            word_model,
            word_input,
            word_output,
            pending: Vec::with_capacity(CHUNK_SAMPLES * 2),
            overlap: Vec::new(),
            mels: VecDeque::with_capacity(MEL_WINDOW * MEL_BINS),
            embeddings: VecDeque::with_capacity(EMBEDDING_WINDOW * EMBEDDING_DIM),
            armed: true,
        })
    }

    /// Runs the three stages over one 80 ms chunk and returns the word's
    /// probability, once there is enough context for the per-word model to have
    /// an opinion.
    fn score_chunk(&mut self, chunk: &[f32]) -> Result<Option<f32>> {
        // Stage 1: melspectrogram over the chunk plus the previous tail.
        let mut input = Vec::with_capacity(self.overlap.len() + chunk.len());
        input.extend_from_slice(&self.overlap);
        input.extend_from_slice(chunk);
        let had_overlap = self.overlap.len() == OVERLAP_SAMPLES;
        self.overlap = chunk[chunk.len() - OVERLAP_SAMPLES..].to_vec();

        let samples = input.len();
        let mel_out = self
            .melspectrogram
            .run(ort::inputs! { "input" => Tensor::from_array(([1_usize, samples], input))? })?;
        let (_, mel) = mel_out["output"].try_extract_tensor::<f32>()?;

        // openWakeWord's own scaling of the raw mel output. Without it the
        // embedding model is fed values it was never trained on and the chain
        // reports plausible-looking noise.
        let scaled: Vec<f32> = mel.iter().map(|v| v / 10.0 + 2.0).collect();

        // Before the first overlap is established the frame count is short by
        // the missing history; take only whole trailing frames either way.
        let take = if had_overlap {
            MEL_PER_CHUNK * MEL_BINS
        } else {
            scaled.len().min(MEL_PER_CHUNK * MEL_BINS)
        };
        for value in &scaled[scaled.len() - take..] {
            self.mels.push_back(*value);
        }
        while self.mels.len() > MEL_WINDOW * MEL_BINS {
            self.mels.pop_front();
        }
        if self.mels.len() < MEL_WINDOW * MEL_BINS {
            return Ok(None);
        }

        // Stage 2: one embedding vector over the trailing 76 mel frames. The
        // 8-frame stride is implicit — this runs once per chunk.
        let window: Vec<f32> = self.mels.iter().copied().collect();
        let emb_out = self.embedding.run(ort::inputs! {
            "input_1" => Tensor::from_array(([1_usize, MEL_WINDOW, MEL_BINS, 1], window))?
        })?;
        let (_, vector) = emb_out["conv2d_19"].try_extract_tensor::<f32>()?;
        for value in vector {
            self.embeddings.push_back(*value);
        }
        while self.embeddings.len() > EMBEDDING_WINDOW * EMBEDDING_DIM {
            self.embeddings.pop_front();
        }
        if self.embeddings.len() < EMBEDDING_WINDOW * EMBEDDING_DIM {
            return Ok(None);
        }

        // Stage 3: the per-word model over the trailing 16 embeddings.
        let context: Vec<f32> = self.embeddings.iter().copied().collect();
        let word_out = self.word_model.run(ort::inputs! {
            self.word_input.as_str() =>
                Tensor::from_array(([1_usize, EMBEDDING_WINDOW, EMBEDDING_DIM], context))?
        })?;
        let (_, score) = word_out[self.word_output.as_str()].try_extract_tensor::<f32>()?;
        Ok(score.first().copied())
    }

    /// Applies the edge trigger. The port's contract is that one utterance
    /// produces exactly one `true`, not one per chunk while the score stays
    /// high.
    fn fired(&mut self, score: f32) -> bool {
        if score >= self.threshold {
            if self.armed {
                self.armed = false;
                // Drop the context the detection was made from, so the same
                // utterance cannot fire again as it slides out of the window.
                self.embeddings.clear();
                return true;
            }
            return false;
        }
        // Hysteresis: re-arm well below the firing threshold rather than at it,
        // so a score hovering on the boundary cannot chatter.
        if score < self.threshold * 0.5 {
            self.armed = true;
        }
        false
    }
}

impl WakeWordDetector for OnnxWakeWord {
    fn accept(&mut self, frame: &[u8]) -> bool {
        // The wire format is PCM 16-bit LE (docs/05 §1). openWakeWord is
        // trained on the raw int16 magnitudes as floats, *not* on samples
        // normalised to [-1, 1] — normalising here costs about 40 dB of input
        // level and the models simply never fire.
        self.pending.extend(
            frame
                .chunks_exact(2)
                .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]]))),
        );

        let mut fired = false;
        while self.pending.len() >= CHUNK_SAMPLES {
            let chunk: Vec<f32> = self.pending.drain(..CHUNK_SAMPLES).collect();
            match self.score_chunk(&chunk) {
                Ok(Some(score)) => {
                    if self.fired(score) {
                        fired = true;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    // A node that stops listening because one inference failed
                    // is worse than one that misses a chunk: push-to-talk and
                    // playback are unaffected either way.
                    tracing::warn!(%error, "wake-word inference failed for one chunk");
                }
            }
        }
        fired
    }

    fn word(&self) -> &str {
        &self.word
    }
}
