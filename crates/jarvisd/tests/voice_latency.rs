//! NFR-04 latency harness (`cargo xtask perf --voice`).
//!
//! # What this measures, and what it deliberately does not
//!
//! NFR-04 (docs/01 §4.1) budgets **final transcript < 0.8 s after end of speech**
//! and **first audio < 1.2 s after the response text begins**, on reference
//! hardware. Those end-to-end figures are dominated by the speech models
//! themselves — faster-whisper `base`/`small` int8 and Piper — which run as
//! out-of-process Wyoming services this repository does not ship, pin, or start
//! (ADR-007, docs/02 §12).
//!
//! So this harness measures the part that **is** ours and is reproducible on any
//! machine with no containers running: the daemon-side pipeline overhead, with
//! the speech engines replaced by fixtures that answer immediately. Concretely:
//!
//! * `transcript overhead` — from the client's `voice.stream.stop` to the
//!   `voice.transcript` final event: WS ingest, PCM framing, the Wyoming client's
//!   connect/write/read cycle, and the broadcast back out.
//! * `first-audio overhead` — from the first `text.delta` of the response to the
//!   first binary PCM frame: clause segmentation, the synthesizer connect, and
//!   the audio bracket.
//!
//! **The number printed is NOT the NFR-04 figure.** Add the measured STT/TTS
//! model time on the reference machine to it to get that. What this harness can
//! honestly do — and what it is for — is fail when *our* share of the budget
//! regresses, and give the M5 gate a real, repeatable figure for the overhead
//! the model time is added to. Recording the reference-hardware model-size
//! decision (docs/08 §6) needs a reference machine with real services on it and
//! is explicitly out of this harness's scope; it is a gate-report entry.
//!
//! The run under test takes the M4 deterministic route ("15% of 230"), so no
//! model provider latency is in the figures either.

mod voice_fixture;

use std::sync::Arc;
use std::time::{Duration, Instant};

use jarvis_adapters::wyoming::WyomingClient;
use jarvis_application::testing::FakeModel;
use sqlx::PgPool;
use voice_fixture::{Harness, Received, SESSION, VoiceSocket, VoiceWiring, addr_of};

/// Samples per figure. Enough for a stable p95 without making the harness slow.
const SAMPLES: usize = 12;

/// Our share of the NFR-04 0.8 s transcript budget. Deliberately a small slice
/// of it: the STT model is expected to consume the rest, so daemon overhead
/// above this would be eating the model's budget.
const TRANSCRIPT_OVERHEAD_BUDGET: Duration = Duration::from_millis(150);

/// Our share of the NFR-04 1.2 s first-audio budget (clause segmentation +
/// synthesizer connect + the audio bracket; Piper's own latency is on top).
const FIRST_AUDIO_OVERHEAD_BUDGET: Duration = Duration::from_millis(300);

/// Above this fraction of the budget the figure is reported as a warning: still
/// passing, but no longer leaving the speech models the room they need.
const WARN_FRACTION: f64 = 0.6;

/// Hard ceiling on one sample, so a wedged pipeline fails the harness instead of
/// hanging it.
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(10);

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn voice_pipeline_overhead_is_within_its_share_of_nfr_04(pool: PgPool) {
    let stt_url = voice_fixture::stt_returning("15% of 230").await;
    // Zero-gap TTS: the fixture is not the thing being measured.
    let tts_url = voice_fixture::tts_streaming(2, 2048, Duration::from_millis(0)).await;
    let harness = Harness::start(
        pool,
        FakeModel::streaming(["the deterministic route answers this"]),
        VoiceWiring {
            transcriber: Some(Arc::new(WyomingClient::new("stt", addr_of(&stt_url)))),
            synthesizer: Some(Arc::new(WyomingClient::new("tts", addr_of(&tts_url)))),
        },
    )
    .await;

    let mut transcript = Vec::new();
    let mut first_audio = Vec::new();

    for sample in 0..SAMPLES {
        let mut socket = harness.connect().await;
        let stream_id = format!("perf-{sample}");
        socket
            .send_control(VoiceSocket::start_stream(&stream_id, Some(SESSION)))
            .await;
        // ~20 ms of 16 kHz mono s16le, the frame size the browser emits.
        socket.send_pcm(vec![0u8; 640]).await;

        let released = Instant::now();
        socket
            .send_control(VoiceSocket::stop_stream(&stream_id))
            .await;

        // One pass, stamped per frame as it arrives — timing from when a batch
        // is processed would collapse same-batch frames to a fictitious 0 ms.
        let frames = socket
            .collect_timed(SAMPLE_TIMEOUT, |received| {
                matches!(received, Received::Audio(_))
            })
            .await;

        let mut transcript_at = None;
        let mut first_delta_at = None;
        let mut first_audio_at = None;
        for (at, item) in &frames {
            // Only events after THIS sample's transcript belong to this turn: a
            // previous sample's assistant-message commit can still be in flight
            // when the next socket connects, and must not be timed as this one.
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

        transcript.push(transcript_at.expect("no final transcript within the sample timeout"));
        let delta = first_delta_at.expect("no response text within the sample timeout");
        let audio = first_audio_at.expect("no synthesized audio within the sample timeout");
        first_audio.push(audio.saturating_duration_since(delta));
    }

    println!();
    println!("=== jarvisd voice pipeline overhead (NFR-04 context, docs/01 §4.1) ===");
    println!(
        "measured with FIXTURE Wyoming services — the STT/TTS MODEL TIME IS EXCLUDED, so these"
    );
    println!("are not the NFR-04 figures; they are the daemon-side share the model time adds to.");
    let transcript_ok = report(
        "voice.stream.stop -> final transcript",
        &mut transcript,
        TRANSCRIPT_OVERHEAD_BUDGET,
        "NFR-04 budgets 0.8s end to end, faster-whisper time on top",
    );
    let audio_ok = report(
        "first text.delta   -> first audio frame",
        &mut first_audio,
        FIRST_AUDIO_OVERHEAD_BUDGET,
        "NFR-04 budgets 1.2s end to end, Piper time on top",
    );

    harness.shutdown.cancel();
    assert!(
        transcript_ok && audio_ok,
        "voice pipeline overhead exceeded its share of the NFR-04 budget — see the report above"
    );
}

/// Print one figure the way `xtask perf --rss` prints its budgets: the measured
/// distribution, the budget, and an explicit PASS/WARN/FAIL line. Returns
/// whether the budget held.
fn report(label: &str, samples: &mut [Duration], budget: Duration, context: &str) -> bool {
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95).div_ceil(100).min(samples.len()) - 1];
    let max = *samples.last().expect("at least one sample");
    println!();
    println!("{label}");
    println!(
        "  p50 {:.1} ms | p95 {:.1} ms | max {:.1} ms over {} samples (budget p95 < {:.0} ms; {context})",
        p50.as_secs_f64() * 1000.0,
        p95.as_secs_f64() * 1000.0,
        max.as_secs_f64() * 1000.0,
        samples.len(),
        budget.as_secs_f64() * 1000.0,
    );
    if p95 > budget {
        println!(
            "  FAIL: p95 {:.1} ms exceeds the {:.0} ms overhead budget",
            p95.as_secs_f64() * 1000.0,
            budget.as_secs_f64() * 1000.0
        );
        return false;
    }
    if p95.as_secs_f64() > budget.as_secs_f64() * WARN_FRACTION {
        println!(
            "  WARN: p95 {:.1} ms is above {:.0}% of the overhead budget — little headroom left for the speech model",
            p95.as_secs_f64() * 1000.0,
            WARN_FRACTION * 100.0
        );
        return true;
    }
    println!("  PASS: within the overhead budget");
    true
}
