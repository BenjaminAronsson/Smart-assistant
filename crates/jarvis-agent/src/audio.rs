//! Node audio: capture and playback (F8.2, FR-13, docs/05 §1).
//!
//! This is what makes M7's `voice-node` / `room-node` classes describe
//! something that can exist. Until now the only thing in the tree that could
//! open a microphone was a browser tab.
//!
//! The format is not negotiable and not guessed: **PCM 16-bit little-endian,
//! 16 kHz, mono**, which is what docs/05 §1 fixes for v1 and what the Wyoming
//! services on the other end expect. A device that cannot produce it is
//! resampled to it here rather than being allowed to put a surprise sample rate
//! on the wire.
//!
//! Everything hardware-facing sits behind [`AudioInput`] / [`AudioOutput`] so
//! the session logic in [`crate::node_voice`] is testable without a sound card
//! — CI has no audio device, and a test that skipped itself there would be no
//! evidence at all.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

/// The one wire format (docs/05 §1).
pub const SAMPLE_RATE_HZ: u32 = 16_000;
pub const CHANNELS: u16 = 1;
pub const SAMPLE_WIDTH_BYTES: u16 = 2;

/// 20 ms at 16 kHz mono 16-bit = 320 samples = 640 bytes. Inside the 20–40 ms
/// band docs/05 §1 specifies, and small enough that muting takes effect within
/// one frame.
pub const FRAME_BYTES: usize = 640;

/// Whether this node's microphone is live, and who says so.
///
/// A satellite whose mute state you cannot see is not one people accept in a
/// kitchen, so this is deliberately a first-class object rather than a flag
/// buried in the capture loop: it is readable for display, and the capture
/// callback consults it **at the source**. Muting does not stop the daemon from
/// being sent audio; it stops audio from being *captured*, which is the only
/// version of "off" that means anything to somebody standing in the room.
#[derive(Clone, Default)]
pub struct Mute(Arc<AtomicBool>);

impl Mute {
    pub fn new(muted: bool) -> Self {
        Self(Arc::new(AtomicBool::new(muted)))
    }

    pub fn is_muted(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set(&self, muted: bool) {
        let previous = self.0.swap(muted, Ordering::Relaxed);
        if previous != muted {
            // Visible, per the feature's "honoured **and visible**".
            tracing::info!(muted, "microphone mute state changed");
        }
    }
}

/// A microphone. Frames arrive on the channel as raw little-endian PCM.
pub trait AudioInput: Send + Sync {
    /// Begins capture, sending [`FRAME_BYTES`]-sized frames until the returned
    /// handle is dropped. Frames are dropped at the source while muted.
    fn start(
        &self,
        frames: tokio::sync::mpsc::Sender<Vec<u8>>,
        mute: Mute,
    ) -> Result<CaptureHandle>;

    /// What the owner would call this device.
    fn describe(&self) -> String;
}

/// A speaker.
pub trait AudioOutput: Send + Sync {
    /// Queues one PCM frame for playback.
    fn play(&self, frame: &[u8]) -> Result<()>;

    /// Drops anything still queued — barge-in, and the cancelled/failed ends of
    /// an utterance. Silence must be immediate, not "after the buffer drains".
    fn flush(&self);

    fn describe(&self) -> String;
}

/// Dropping this stops the capture stream.
///
/// It holds a channel, not the stream. `cpal::Stream` is `!Send` on several
/// hosts (ALSA among them), so the stream is created, played and dropped on one
/// dedicated thread and never crosses another — which is what lets this crate
/// stay free of `unsafe` despite CLAUDE.md permitting it here. Dropping the
/// sender closes the channel, the thread wakes, and the stream is dropped where
/// it was made.
pub struct CaptureHandle {
    _shutdown: std::sync::mpsc::Sender<()>,
}

impl CaptureHandle {
    pub fn new(shutdown: std::sync::mpsc::Sender<()>) -> Self {
        Self {
            _shutdown: shutdown,
        }
    }
}

/// Which devices this node should use. `None` means the host default.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioConfig {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// Start muted. A node that boots hot in a bedroom is a bad default to not
    /// have a switch for.
    #[serde(default)]
    pub start_muted: bool,
}

impl AudioConfig {
    /// Read from the environment, which is where a systemd unit puts it
    /// (F8.9 gives this a real config file).
    pub fn from_env() -> Self {
        Self {
            input_device: std::env::var("JARVIS_AGENT_AUDIO_INPUT").ok(),
            output_device: std::env::var("JARVIS_AGENT_AUDIO_OUTPUT").ok(),
            start_muted: std::env::var("JARVIS_AGENT_START_MUTED")
                .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true")),
        }
    }
}

/// Converts an interleaved `f32` buffer at `source_rate` into mono 16-bit PCM
/// at [`SAMPLE_RATE_HZ`].
///
/// Cheap nearest-neighbour resampling and channel averaging. It is not a good
/// resampler and does not need to be: the target is speech recognition at
/// 16 kHz, the common case is a device that already runs at 16 or 48 kHz, and
/// the alternative — refusing to open any device whose native rate is not
/// 16 kHz — would make the feature depend on the microphone somebody happened
/// to buy.
pub fn to_pcm16(samples: &[f32], source_rate: u32, source_channels: u16) -> Vec<u8> {
    if samples.is_empty() || source_channels == 0 || source_rate == 0 {
        return Vec::new();
    }
    let channels = usize::from(source_channels);
    let frames = samples.len() / channels;
    let target_frames = ((frames as u64 * u64::from(SAMPLE_RATE_HZ)) / u64::from(source_rate))
        .try_into()
        .unwrap_or(usize::MAX);

    let mut out = Vec::with_capacity(target_frames * 2);
    for index in 0..target_frames {
        let source_index = index * frames / target_frames.max(1);
        let start = source_index * channels;
        let mixed: f32 = samples[start..(start + channels).min(samples.len())]
            .iter()
            .sum::<f32>()
            / channels as f32;
        // Clamp before casting: an out-of-range f32 to i16 cast saturates on
        // some targets and is UB-adjacent nonsense on others.
        let scaled = (mixed.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        out.extend_from_slice(&scaled.to_le_bytes());
    }
    out
}

/// Turns a stream of device callbacks into whole wire frames, honouring mute.
///
/// This exists as its own type so that "mute stops frames **at the source**"
/// is a claim a test can check. Inside a `cpal` callback it would only be
/// checkable with a microphone attached, which on CI means not checked at all.
pub struct FrameAccumulator {
    pending: Vec<u8>,
    mute: Mute,
}

impl FrameAccumulator {
    pub fn new(mute: Mute) -> Self {
        Self {
            pending: Vec::with_capacity(FRAME_BYTES * 2),
            mute,
        }
    }

    /// Feeds one device buffer in and returns any whole frames it completed.
    ///
    /// While muted this returns nothing **and discards what was already
    /// buffered** — otherwise unmuting would flush out audio recorded while the
    /// microphone was supposed to be off, which is precisely the thing a mute
    /// switch promises cannot happen.
    pub fn push(
        &mut self,
        samples: &[f32],
        source_rate: u32,
        source_channels: u16,
    ) -> Vec<Vec<u8>> {
        if self.mute.is_muted() {
            self.pending.clear();
            return Vec::new();
        }
        self.pending
            .extend_from_slice(&to_pcm16(samples, source_rate, source_channels));
        let mut frames = Vec::new();
        while self.pending.len() >= FRAME_BYTES {
            frames.push(self.pending.drain(..FRAME_BYTES).collect());
        }
        frames
    }
}

/// Rejects a binary audio frame that cannot be PCM in the agreed format.
///
/// The daemon is trusted, but "trusted" is not "infallible", and a frame with
/// an odd byte count would be silently shifted by one byte for the rest of the
/// utterance — audible as a burst of noise rather than as an error.
pub fn validate_pcm_frame(frame: &[u8], max_bytes: usize) -> Result<()> {
    if frame.is_empty() {
        anyhow::bail!("empty audio frame");
    }
    if frame.len() > max_bytes {
        anyhow::bail!(
            "audio frame of {} bytes exceeds the {max_bytes}-byte ceiling",
            frame.len()
        );
    }
    if !frame.len().is_multiple_of(usize::from(SAMPLE_WIDTH_BYTES)) {
        anyhow::bail!(
            "audio frame of {} bytes is not a whole number of 16-bit samples",
            frame.len()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// cpal-backed implementations
// ---------------------------------------------------------------------------

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Opens the host's input device, or the one named in config.
///
/// Only the device *name* is kept. The `cpal::Device` itself is resolved again
/// on the capture thread, so nothing `!Send` is stored here.
pub struct CpalInput {
    name: String,
    requested: Option<String>,
}

/// Finds an input device by name, or the host default.
fn input_device(requested: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    match requested {
        Some(name) => host
            .input_devices()
            .context("enumerating input devices")?
            .find(|device| device.name().is_ok_and(|actual| actual == name))
            .with_context(|| format!("no input device named {name:?}")),
        None => host
            .default_input_device()
            .context("this host has no default input device"),
    }
}

impl CpalInput {
    pub fn open(requested: Option<&str>) -> Result<Self> {
        let device = input_device(requested)?;
        // Probe the configuration now so "there is no usable microphone" is
        // reported at startup, not at the first wake word.
        device
            .default_input_config()
            .context("the input device reported no usable configuration")?;
        Ok(Self {
            name: device
                .name()
                .unwrap_or_else(|_| "<unnamed input>".to_owned()),
            requested: requested.map(str::to_owned),
        })
    }
}

impl AudioInput for CpalInput {
    fn start(
        &self,
        frames: tokio::sync::mpsc::Sender<Vec<u8>>,
        mute: Mute,
    ) -> Result<CaptureHandle> {
        let (shutdown, wait) = std::sync::mpsc::channel::<()>();
        let (ready, started) = std::sync::mpsc::channel::<Result<()>>();
        let requested = self.requested.clone();

        // The stream is built, played and dropped entirely on this thread.
        std::thread::Builder::new()
            .name("jarvis-audio-capture".to_owned())
            .spawn(move || {
                let built = (|| -> Result<cpal::Stream> {
                    let device = input_device(requested.as_deref())?;
                    let supported = device
                        .default_input_config()
                        .context("the input device reported no usable configuration")?;
                    let source_rate = supported.sample_rate().0;
                    let source_channels = supported.channels();
                    // The mute check lives in `FrameAccumulator`, which is
                    // where a test can reach it.
                    let mut accumulator = FrameAccumulator::new(mute);

                    let stream = device
                        .build_input_stream(
                            &supported.config(),
                            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                                for frame in accumulator.push(data, source_rate, source_channels) {
                                    // A full channel means the socket is
                                    // behind. Dropping audio beats growing an
                                    // unbounded queue on a satellite
                                    // (low-power rule 3) and beats blocking a
                                    // realtime callback, which would glitch
                                    // the whole stream.
                                    if frames.try_send(frame).is_err() {
                                        tracing::debug!("audio frame dropped: sender is behind");
                                    }
                                }
                            },
                            |error| tracing::warn!(%error, "audio capture error"),
                            None,
                        )
                        .context("opening the input stream")?;
                    stream.play().context("starting capture")?;
                    Ok(stream)
                })();

                match built {
                    Ok(stream) => {
                        let _ = ready.send(Ok(()));
                        // Park until the handle is dropped, then drop the
                        // stream here, on the thread that created it.
                        let _ = wait.recv();
                        drop(stream);
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                    }
                }
            })
            .context("spawning the capture thread")?;

        started
            .recv()
            .context("the capture thread stopped before reporting")??;
        Ok(CaptureHandle::new(shutdown))
    }

    fn describe(&self) -> String {
        self.name.clone()
    }
}

/// Playback through the host's output device.
///
/// Like [`CpalInput`], the `!Send` stream lives on its own thread; this struct
/// holds only the channel and the flush flag.
pub struct CpalOutput {
    name: String,
    queue: std::sync::mpsc::Sender<Vec<u8>>,
    /// Set to drop everything queued; cleared by the next callback.
    flushing: Arc<AtomicBool>,
    _shutdown: std::sync::mpsc::Sender<()>,
}

fn output_device(requested: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    match requested {
        Some(name) => host
            .output_devices()
            .context("enumerating output devices")?
            .find(|device| device.name().is_ok_and(|actual| actual == name))
            .with_context(|| format!("no output device named {name:?}")),
        None => host
            .default_output_device()
            .context("this host has no default output device"),
    }
}

impl CpalOutput {
    pub fn open(requested: Option<&str>) -> Result<Self> {
        let name = output_device(requested)?
            .name()
            .unwrap_or_else(|_| "<unnamed output>".to_owned());

        let (queue, receiver) = std::sync::mpsc::channel::<Vec<u8>>();
        let (shutdown, wait) = std::sync::mpsc::channel::<()>();
        let (ready, started) = std::sync::mpsc::channel::<Result<()>>();
        let flushing = Arc::new(AtomicBool::new(false));
        let flush_flag = flushing.clone();
        let requested = requested.map(str::to_owned);

        std::thread::Builder::new()
            .name("jarvis-audio-playback".to_owned())
            .spawn(move || {
                let built = (|| -> Result<cpal::Stream> {
                    let device = output_device(requested.as_deref())?;
                    let supported = device
                        .default_output_config()
                        .context("the output device reported no usable configuration")?;
                    let target_rate = supported.sample_rate().0;
                    let target_channels = supported.channels();
                    let mut buffered: std::collections::VecDeque<i16> =
                        std::collections::VecDeque::new();

                    let stream = device
                        .build_output_stream(
                            &supported.config(),
                            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                                // Barge-in must be silent *now*, not after the
                                // buffer drains.
                                if flush_flag.swap(false, Ordering::Relaxed) {
                                    buffered.clear();
                                }
                                while let Ok(frame) = receiver.try_recv() {
                                    for sample in frame.chunks_exact(2) {
                                        buffered
                                            .push_back(i16::from_le_bytes([sample[0], sample[1]]));
                                    }
                                }
                                // Nearest-neighbour up-sample from 16 kHz to
                                // the device rate, duplicated across channels.
                                // Same trade as capture.
                                let step = f64::from(SAMPLE_RATE_HZ) / f64::from(target_rate);
                                let mut position = 0.0_f64;
                                for chunk in out.chunks_mut(usize::from(target_channels)) {
                                    let sample =
                                        buffered.get(position as usize).map_or(0.0, |value| {
                                            f32::from(*value) / f32::from(i16::MAX)
                                        });
                                    for slot in chunk.iter_mut() {
                                        *slot = sample;
                                    }
                                    position += step;
                                }
                                for _ in 0..(position as usize).min(buffered.len()) {
                                    buffered.pop_front();
                                }
                            },
                            |error| tracing::warn!(%error, "audio playback error"),
                            None,
                        )
                        .context("opening the output stream")?;
                    stream.play().context("starting playback")?;
                    Ok(stream)
                })();

                match built {
                    Ok(stream) => {
                        let _ = ready.send(Ok(()));
                        let _ = wait.recv();
                        drop(stream);
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                    }
                }
            })
            .context("spawning the playback thread")?;

        started
            .recv()
            .context("the playback thread stopped before reporting")??;

        Ok(Self {
            name,
            queue,
            flushing,
            _shutdown: shutdown,
        })
    }
}

impl AudioOutput for CpalOutput {
    fn play(&self, frame: &[u8]) -> Result<()> {
        self.queue
            .send(frame.to_vec())
            .map_err(|_| anyhow::anyhow!("the playback stream has stopped"))
    }

    fn flush(&self) {
        self.flushing.store(true, Ordering::Relaxed);
    }

    fn describe(&self) -> String {
        self.name.clone()
    }
}

/// An output that discards everything: a node with no speaker, and the type a
/// display-only agent names when it has no audio at all.
pub struct NoOutput;

impl AudioOutput for NoOutput {
    fn play(&self, _frame: &[u8]) -> Result<()> {
        Ok(())
    }
    fn flush(&self) {}
    fn describe(&self) -> String {
        "<no audio output>".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mute_is_readable_and_flips() {
        let mute = Mute::new(false);
        assert!(!mute.is_muted());
        let shared = mute.clone();
        shared.set(true);
        // A clone is the same switch, not a copy of its position — the capture
        // callback holds one and the UI would hold another.
        assert!(mute.is_muted());
    }

    #[test]
    fn resampling_48k_stereo_to_16k_mono_produces_the_right_frame_length() {
        // 480 stereo frames at 48 kHz = 10 ms → 160 mono samples at 16 kHz.
        let samples = vec![0.5_f32; 480 * 2];
        let pcm = to_pcm16(&samples, 48_000, 2);
        assert_eq!(pcm.len(), 160 * 2, "expected 160 16-bit samples");
    }

    #[test]
    fn a_device_already_at_16k_mono_passes_through_sample_for_sample() {
        let samples = vec![0.0_f32, 1.0, -1.0, 0.5];
        let pcm = to_pcm16(&samples, SAMPLE_RATE_HZ, 1);
        assert_eq!(pcm.len(), 8);
        // Full scale clamps to i16::MAX / -i16::MAX rather than wrapping.
        let decoded: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(decoded[0], 0);
        assert_eq!(decoded[1], i16::MAX);
        assert_eq!(decoded[2], -i16::MAX);
    }

    #[test]
    fn resampling_survives_degenerate_input_without_panicking() {
        assert!(to_pcm16(&[], 16_000, 1).is_empty());
        assert!(to_pcm16(&[0.1], 0, 1).is_empty());
        assert!(to_pcm16(&[0.1], 16_000, 0).is_empty());
    }

    #[test]
    fn an_odd_length_frame_is_rejected_rather_than_shifted() {
        assert!(validate_pcm_frame(&[0, 1, 2], 4096).is_err());
        assert!(validate_pcm_frame(&[0, 1, 2, 3], 4096).is_ok());
    }

    #[test]
    fn empty_and_oversized_frames_are_rejected() {
        assert!(validate_pcm_frame(&[], 4096).is_err());
        assert!(validate_pcm_frame(&vec![0_u8; 5000], 4096).is_err());
    }

    /// The feature's own wording: mute must stop frames **at the source**, not
    /// at the server. Nothing is produced to send in the first place.
    #[test]
    fn muting_stops_frames_being_produced_at_all() {
        let mute = Mute::new(false);
        let mut accumulator = FrameAccumulator::new(mute.clone());

        // One 20 ms buffer at 16 kHz mono = exactly one wire frame.
        let buffer = vec![0.25_f32; 320];
        assert_eq!(accumulator.push(&buffer, SAMPLE_RATE_HZ, 1).len(), 1);

        mute.set(true);
        for _ in 0..10 {
            assert!(
                accumulator.push(&buffer, SAMPLE_RATE_HZ, 1).is_empty(),
                "a muted microphone must produce no frames"
            );
        }

        mute.set(false);
        assert_eq!(accumulator.push(&buffer, SAMPLE_RATE_HZ, 1).len(), 1);
    }

    /// Unmuting must not flush out audio captured while muted.
    #[test]
    fn audio_buffered_before_a_mute_is_discarded_not_released_afterwards() {
        let mute = Mute::new(false);
        let mut accumulator = FrameAccumulator::new(mute.clone());

        // Half a frame in: not enough to emit, so it sits in the buffer.
        assert!(
            accumulator
                .push(&vec![0.5_f32; 160], SAMPLE_RATE_HZ, 1)
                .is_empty()
        );
        mute.set(true);
        assert!(
            accumulator
                .push(&vec![0.5_f32; 160], SAMPLE_RATE_HZ, 1)
                .is_empty()
        );
        mute.set(false);

        // The next half-frame must NOT combine with the pre-mute half to emit
        // a frame containing audio from the muted period.
        assert!(
            accumulator
                .push(&vec![0.5_f32; 160], SAMPLE_RATE_HZ, 1)
                .is_empty(),
            "audio from before the mute must have been discarded"
        );
    }

    #[test]
    fn audio_config_defaults_to_the_host_devices_and_unmuted() {
        let config = AudioConfig::default();
        assert!(config.input_device.is_none());
        assert!(config.output_device.is_none());
        assert!(!config.start_muted);
    }
}
