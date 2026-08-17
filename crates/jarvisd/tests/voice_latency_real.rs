//! The **real** NFR-04 measurement (D-M5-3, open since M5).
//!
//! `voice_latency.rs` measures the daemon's own share of the budget with the
//! speech engines replaced by fixtures that answer instantly. That is the right
//! thing to run on every machine and in CI, and it is deliberately *not* the
//! NFR-04 number: the figures docs/01 §4.1 budgets are dominated by the models
//! themselves.
//!
//! This harness is the other half. It drives the same production pipeline
//! against **real Wyoming services** — faster-whisper and Piper, started by
//! `infra/compose/voice.yml` — with **real recorded speech**, and reports the
//! two figures NFR-04 actually names:
//!
//! * **final transcript < 0.8 s after end of speech**
//! * **first audio < 1.2 s after the response text begins**
//!
//! # Why it skips by default
//!
//! It needs two containers holding a speech model resident, which is exactly
//! what docs/01 §4.1 says an 8 GB laptop running the test suite should not also
//! be doing. So it runs only when asked:
//!
//! ```text
//! docker compose -f infra/compose/dev.yml -f infra/compose/voice.yml up -d
//! JARVIS_NFR04_REAL=1 cargo test -p jarvisd --release --test voice_latency_real -- --nocapture
//! ```
//!
//! # What a pass here does and does not mean
//!
//! It measures the machine it runs on. NFR-04 is specified **on reference
//! hardware** (the 8 GB profile), so a pass on a developer workstation is
//! evidence that the pipeline is in the right order of magnitude and that
//! nothing in it is pathologically slow — not that the budget holds on the
//! target. The gate report must say which machine produced the number.

use std::sync::Arc;
use std::time::{Duration, Instant};

use jarvis_adapters::wyoming::WyomingClient;
use jarvis_application::testing::FakeModel;
use sqlx::PgPool;

mod voice_fixture;
use voice_fixture::{Harness, Received, SESSION, VoiceSocket, VoiceWiring};

/// NFR-04 (docs/01 §4.1), in full rather than a share of it.
const TRANSCRIPT_BUDGET: Duration = Duration::from_millis(800);
const FIRST_AUDIO_BUDGET: Duration = Duration::from_millis(1200);

/// Fewer samples than the fixture harness: each one runs a real speech model.
const SAMPLES: usize = 5;

/// Generous, because a cold model can take seconds on its first utterance —
/// the point is to fail rather than hang.
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(30);

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// Real recorded speech, as 20 ms frames of 16 kHz mono s16le.
///
/// Silence would be worse than useless here: an STT model given silence returns
/// almost immediately, so a "measurement" over it would report the pipeline's
/// latency with the expensive part skipped. This uses the same openWakeWord
/// recordings the engine tests use — real human speech at exactly the wire
/// format the node captures.
fn speech_frames() -> Option<Vec<Vec<u8>>> {
    let path = std::env::var("JARVIS_NFR04_SPEECH_WAV")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".cache/jarvis-wake-assets/hey_jane.wav")
        });
    let bytes = std::fs::read(&path).ok()?;

    let mut cursor = 12;
    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into().ok()?) as usize;
        if id == b"data" {
            return Some(
                bytes[cursor + 8..(cursor + 8 + size).min(bytes.len())]
                    .chunks(640)
                    .map(<[u8]>::to_vec)
                    .collect(),
            );
        }
        cursor += 8 + size + (size & 1);
    }
    None
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx]
}

fn report(label: &str, samples: &mut [Duration], budget: Duration) -> bool {
    samples.sort_unstable();
    let median = percentile(samples, 0.5);
    let worst = *samples.last().expect("at least one sample");
    let within = worst <= budget;
    println!(
        "  {label:<22} median {:>7.1} ms   p95 {:>7.1} ms   worst {:>7.1} ms   budget {:>6} ms   {}",
        median.as_secs_f64() * 1000.0,
        percentile(samples, 0.95).as_secs_f64() * 1000.0,
        worst.as_secs_f64() * 1000.0,
        budget.as_millis(),
        if within { "PASS" } else { "OVER" }
    );
    within
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn the_real_voice_round_trip_is_measured_against_nfr_04(pool: PgPool) {
    if std::env::var_os("JARVIS_NFR04_REAL").is_none() {
        eprintln!(
            "SKIP: the real NFR-04 measurement needs live Wyoming services. Start them with\n  \
             docker compose -f infra/compose/dev.yml -f infra/compose/voice.yml up -d\n\
             then re-run with JARVIS_NFR04_REAL=1 (see this file's header)."
        );
        return;
    }
    let Some(frames) = speech_frames() else {
        eprintln!(
            "SKIP: no speech recording. Set JARVIS_NFR04_SPEECH_WAV, or run \
             infra/install/fetch-wake-assets.sh, which provides one."
        );
        return;
    };

    let stt = env_or("JARVIS_NFR04_STT", "127.0.0.1:10300");
    let tts = env_or("JARVIS_NFR04_TTS", "127.0.0.1:10200");
    println!(
        "\nNFR-04, real services: stt={stt} tts={tts} speech={} frames",
        frames.len()
    );

    let harness = Harness::start(
        pool,
        FakeModel::streaming(["the deterministic route answers this"]),
        VoiceWiring {
            transcriber: Some(Arc::new(WyomingClient::new("stt", &stt))),
            synthesizer: Some(Arc::new(WyomingClient::new("tts", &tts))),
        },
    )
    .await;

    let mut transcript = Vec::new();
    let mut first_audio = Vec::new();

    // Sample 0 is discarded: the first utterance pays for the model warming up,
    // and reporting it as a latency figure would describe a state the house is
    // in exactly once.
    for sample in 0..=SAMPLES {
        let mut socket = harness.connect().await;
        let stream_id = format!("nfr04-{sample}");
        socket
            .send_control(VoiceSocket::start_stream(&stream_id, Some(SESSION)))
            .await;
        for frame in &frames {
            socket.send_pcm(frame.clone()).await;
        }

        // "End of speech" — the instant NFR-04 measures the transcript from.
        let released = Instant::now();
        socket
            .send_control(VoiceSocket::stop_stream(&stream_id))
            .await;

        let timed = socket
            .collect_timed(SAMPLE_TIMEOUT, |received| {
                matches!(received, Received::Audio(_))
            })
            .await;

        let mut transcript_at = None;
        let mut first_delta_at = None;
        let mut first_audio_at = None;
        for (at, item) in &timed {
            if transcript_at.is_none()
                && !matches!(item, Received::Event { event_type, payload }
                    if event_type == "voice.transcript" && payload["final"] == true)
            {
                continue;
            }
            match item {
                Received::Event {
                    event_type,
                    payload,
                } if event_type == "voice.transcript"
                    && payload["final"] == true
                    && transcript_at.is_none() =>
                {
                    transcript_at = Some(at.saturating_duration_since(released));
                }
                Received::Event { event_type, .. }
                    if event_type == "text.delta" && first_delta_at.is_none() =>
                {
                    first_delta_at = Some(*at);
                }
                Received::Audio(_) if first_audio_at.is_none() => {
                    first_audio_at = Some(*at);
                }
                _ => {}
            }
        }

        let t = transcript_at.expect("no final transcript within the sample timeout");
        let delta = first_delta_at.expect("no response text within the sample timeout");
        let audio = first_audio_at.expect("no audio within the sample timeout");
        if sample == 0 {
            println!(
                "  (warm-up sample discarded: transcript {:.0} ms)",
                t.as_secs_f64() * 1000.0
            );
            continue;
        }
        transcript.push(t);
        first_audio.push(audio.saturating_duration_since(delta));
    }

    println!("NFR-04 on THIS machine (not the 8 GB reference profile):");
    let transcript_ok = report("final transcript", &mut transcript, TRANSCRIPT_BUDGET);
    let audio_ok = report("first audio", &mut first_audio, FIRST_AUDIO_BUDGET);
    println!();

    assert!(
        transcript_ok && audio_ok,
        "NFR-04 exceeded on this machine — see the figures above"
    );
}
