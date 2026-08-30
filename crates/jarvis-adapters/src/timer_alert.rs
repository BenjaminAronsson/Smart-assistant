//! The audible half of a timer going off (FR-33, ADR-023) — and the M5 seam for
//! the spoken half.
//!
//! ADR-023's hard requirement: **"an alarm must sound even if voice services are
//! down"**. So the tone here does not touch the TTS pipeline, does not touch
//! Wyoming, does not touch the model, and does not read a file from disk:
//!
//! * the waveform is **synthesized in memory** ([`alert_wav`]) — no bundled
//!   asset to go missing, no path to mis-resolve, nothing resident;
//! * it is handed to a **configured playback command** on stdin
//!   (`paplay` by default, `aplay`/`ffplay` work equally), which is the only
//!   audio dependency on the box;
//! * a missing or failing player is [`AlertError::Unavailable`], never a panic
//!   and never a failed fire — the timer still rings visually and is still
//!   recorded.
//!
//! **Trust:** the program name and its arguments come from `[timers]` in the
//! owner's config file (Z1) and from nowhere else. No timer name, no reminder
//! note, and no model output is ever interpolated into the command line — the
//! only thing that flows from a timer into this adapter is *that it fired*. The
//! WAV goes to the child's **stdin**, so there is no argument to inject into
//! even if that changed.
//!
//! **M5 boundary.** [`SilentAnnouncer`] is the whole of the not-yet-built voice
//! hop: it answers [`AnnouncementOutcome::Unavailable`] for every line. When M5
//! lands the Wyoming TTS adapter, it implements
//! [`jarvis_application::ports::Announcer`] and replaces this one binding in
//! `jarvisd::main`. Nothing else in the timers feature changes, and in
//! particular the tone above stays exactly where it is — an alarm must not
//! become dependent on voice by being upgraded.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jarvis_application::ports::{AlertError, AlertPlayer, AnnouncementOutcome, Announcer};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Sample rate of the generated tone. 22.05 kHz is ample for a two-tone chime
/// and halves the bytes we push through a pipe on an 8 GB target (docs/09 §5).
pub const SAMPLE_RATE: u32 = 22_050;

/// How long the whole alert lasts. Long enough to be noticed across a room,
/// short enough that it never becomes the thing you are waiting to end.
const TONE_MILLIS: u32 = 250;
const TONE_REPEATS: u32 = 3;
const GAP_MILLIS: u32 = 120;

/// The two pitches of the chime (a rising major third — distinctly "attention"
/// rather than "error", and audible over speech).
const PITCH_LOW_HZ: f32 = 880.0;
const PITCH_HIGH_HZ: f32 = 1_108.7;

/// Peak amplitude, ~50% of full scale. Deliberately not maximal: this plays at
/// whatever the system volume happens to be, and a clipped square-edged blast at
/// 3 a.m. is a hearing-protection problem, not a louder alarm.
const AMPLITUDE: f32 = 0.5;

/// Hard ceiling on how long a playback child may live. A wedged player must not
/// hold a scheduler task open (invariant 4); it is killed and reported.
const PLAYBACK_DEADLINE: Duration = Duration::from_secs(10);

/// Synthesize the alert as a 16-bit mono PCM WAV.
///
/// Pure and deterministic — the same bytes every time — so it is testable
/// without an audio device, which is the only way this is testable at all in CI.
/// Each burst is amplitude-ramped at both ends: an abrupt start/stop on a sine
/// is a click, and a click is what a cheap alarm sounds like.
pub fn alert_wav() -> Vec<u8> {
    let mut samples: Vec<i16> = Vec::new();
    for repeat in 0..TONE_REPEATS {
        let pitch = if repeat % 2 == 0 {
            PITCH_LOW_HZ
        } else {
            PITCH_HIGH_HZ
        };
        push_tone(&mut samples, pitch, TONE_MILLIS);
        if repeat + 1 < TONE_REPEATS {
            push_silence(&mut samples, GAP_MILLIS);
        }
    }
    wav_container(&samples)
}

fn sample_count(millis: u32) -> u32 {
    (SAMPLE_RATE / 1_000) * millis
}

fn push_tone(out: &mut Vec<i16>, hz: f32, millis: u32) {
    let total = sample_count(millis);
    // 5 ms of fade at each end kills the click without audibly softening the
    // attack.
    let fade = sample_count(5).max(1);
    for n in 0..total {
        let t = n as f32 / SAMPLE_RATE as f32;
        let envelope = {
            let in_ramp = (n as f32 / fade as f32).min(1.0);
            let out_ramp = ((total - n) as f32 / fade as f32).min(1.0);
            in_ramp.min(out_ramp)
        };
        let value = (t * hz * std::f32::consts::TAU).sin() * AMPLITUDE * envelope;
        // Scale into i16 with headroom so rounding can never wrap to the
        // opposite rail (which would be an audible pop).
        out.push((value * f32::from(i16::MAX - 1)) as i16);
    }
}

fn push_silence(out: &mut Vec<i16>, millis: u32) {
    out.extend(std::iter::repeat_n(0i16, sample_count(millis) as usize));
}

/// Wrap PCM samples in a canonical 44-byte RIFF/WAVE header.
fn wav_container(samples: &[i16]) -> Vec<u8> {
    let data_bytes = (samples.len() * 2) as u32;
    let byte_rate = SAMPLE_RATE * 2; // mono * 16 bit
    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // format = PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // channels = mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Plays the alert by piping a WAV to a configured command's stdin.
///
/// The bytes are synthesized once at construction and shared, so firing a timer
/// allocates nothing and the resident cost is one ~30 KB buffer.
pub struct CommandAlertPlayer {
    program: String,
    args: Vec<String>,
    wav: Arc<Vec<u8>>,
}

impl CommandAlertPlayer {
    /// `program` and `args` come from `[timers]` in the owner's config — never
    /// from a timer, a model, or any other untrusted source.
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            wav: Arc::new(alert_wav()),
        }
    }

    /// The default PulseAudio/PipeWire player. `paplay` with no file argument
    /// reads an audio file from stdin, which is exactly what we hand it.
    pub fn paplay() -> Self {
        Self::new("paplay", Vec::new())
    }
}

#[async_trait]
impl AlertPlayer for CommandAlertPlayer {
    async fn play(
        &self,
        // This player is the daemon's own speaker, so it is the fallback
        // rather than a router: it rings here wherever the timer was set.
        // Routing to the room lives in jarvisd, which is the only thing that
        // knows which nodes are connected.
        _timer: &jarvis_domain::timers::Timer,
        cancel: CancellationToken,
    ) -> Result<(), AlertError> {
        if cancel.is_cancelled() {
            return Err(AlertError::Cancelled);
        }
        let mut player = Command::new(&self.program);
        // The audio player is a host binary, but it is still a child that would
        // otherwise inherit the daemon's database credential (invariant 5).
        crate::host_env::scrub_secrets(&mut player);
        let mut child = player
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Kill the player if this future is dropped — a timer alert must
            // never outlive the process that started it (invariant 4).
            .kill_on_drop(true)
            .spawn()
            // A missing player is the ordinary headless case, not an incident:
            // report it and let the timer fire silently.
            .map_err(|_| AlertError::Unavailable)?;

        if let Some(mut stdin) = child.stdin.take() {
            // A player that exits early (unsupported format) closes the pipe;
            // that is a broken-pipe write error, not a reason to fail loudly.
            let _ = stdin.write_all(&self.wav).await;
            let _ = stdin.shutdown().await;
        }

        let status = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                let _ = child.kill().await;
                return Err(AlertError::Cancelled);
            }
            result = tokio::time::timeout(PLAYBACK_DEADLINE, child.wait()) => match result {
                Ok(status) => status,
                Err(_) => {
                    let _ = child.kill().await;
                    return Err(AlertError::Failed("playback deadline exceeded".to_owned()));
                }
            },
        };

        match status {
            Ok(status) if status.success() => Ok(()),
            // The player ran and refused: no audio sink, wrong format. Same
            // user-visible outcome as no player at all.
            Ok(_) => Err(AlertError::Unavailable),
            Err(e) => Err(AlertError::Failed(format!("playback failed: {e}"))),
        }
    }
}

/// The pre-M5 announcer: there is no voice pipeline, and it says so.
///
/// Deliberately not a no-op that claims success — [`FiredTimer::announced`] is
/// what tells the HUD that the card is the *only* notice the human is getting,
/// and lying here would silently degrade that (jarvis_application::timers).
pub struct SilentAnnouncer;

#[async_trait]
impl Announcer for SilentAnnouncer {
    async fn announce(
        &self,
        _text: &str,
        _target: Option<&jarvis_domain::ids::DeviceId>,
        _cancel: CancellationToken,
    ) -> AnnouncementOutcome {
        AnnouncementOutcome::Unavailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_alert_is_a_well_formed_mono_16_bit_wav() {
        let wav = alert_wav();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1, "PCM");
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1, "mono");
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            SAMPLE_RATE
        );
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16, "bits/sample");
        assert_eq!(&wav[36..40], b"data");
        let data_len = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]) as usize;
        assert_eq!(
            data_len,
            wav.len() - 44,
            "the declared data length matches the payload"
        );
        // The chunk size field must agree too, or players truncate.
        assert_eq!(
            u32::from_le_bytes([wav[4], wav[5], wav[6], wav[7]]) as usize,
            wav.len() - 8
        );
    }

    #[test]
    fn the_alert_is_audible_and_bounded() {
        let wav = alert_wav();
        let samples: Vec<i16> = wav[44..]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        let expected = (TONE_REPEATS * sample_count(TONE_MILLIS)
            + (TONE_REPEATS - 1) * sample_count(GAP_MILLIS)) as usize;
        assert_eq!(samples.len(), expected);
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > 8_000, "the tone is actually audible: peak {peak}");
        assert!(
            peak < u16::try_from(i16::MAX).unwrap(),
            "and never clips to the rail: peak {peak}"
        );
        // Ramped ends: the very first and last samples are near silence, so the
        // chime does not start or end with a click.
        assert!(samples[0].unsigned_abs() < 500);
        assert!(samples[samples.len() - 1].unsigned_abs() < 500);
    }

    /// The host player ignores the timer entirely — it is the fallback, not a
    /// router — so any well-formed timer serves.
    fn fixture_timer() -> jarvis_domain::timers::Timer {
        use jarvis_domain::timers::{TimerKind, TimerName};
        jarvis_domain::timers::Timer::schedule(
            "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("timer id"),
            TimerName::new("pasta timer").expect("name"),
            TimerKind::Alarm,
            std::time::SystemTime::now() + std::time::Duration::from_secs(60),
            std::time::SystemTime::now(),
        )
        .expect("schedulable")
    }

    #[tokio::test]
    async fn a_box_with_no_audio_player_reports_unavailable_rather_than_failing() {
        // The headless case: the configured player does not exist. The timer
        // must still fire — this is a report, not an error to propagate.
        let player = CommandAlertPlayer::new("jarvis-no-such-audio-player", Vec::new());
        assert_eq!(
            player
                .play(&fixture_timer(), CancellationToken::new())
                .await,
            Err(AlertError::Unavailable)
        );
    }

    #[tokio::test]
    async fn an_already_cancelled_alert_never_spawns_anything() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        // `true` exists everywhere and would succeed; the cancellation check
        // must come first (invariant 4).
        let player = CommandAlertPlayer::new("true", Vec::new());
        assert_eq!(
            player.play(&fixture_timer(), cancel).await,
            Err(AlertError::Cancelled)
        );
    }

    #[tokio::test]
    async fn a_player_that_accepts_the_wav_is_a_clean_success() {
        // `cat` consumes stdin and exits 0 — a stand-in for a working audio
        // sink that proves the pipe/wait path, with no device needed in CI.
        let player = CommandAlertPlayer::new("cat", Vec::new());
        assert_eq!(
            player
                .play(&fixture_timer(), CancellationToken::new())
                .await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn a_player_that_rejects_the_stream_reads_as_unavailable() {
        let player = CommandAlertPlayer::new("false", Vec::new());
        assert_eq!(
            player
                .play(&fixture_timer(), CancellationToken::new())
                .await,
            Err(AlertError::Unavailable)
        );
    }

    #[tokio::test]
    async fn the_pre_m5_announcer_admits_it_cannot_speak() {
        assert_eq!(
            SilentAnnouncer
                .announce("Reminder — call Mom", None, CancellationToken::new())
                .await,
            AnnouncementOutcome::Unavailable,
            "claiming success here would hide that the card is the only notice"
        );
    }
}
