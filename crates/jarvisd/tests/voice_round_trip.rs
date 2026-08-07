//! F5.2 exit evidence: the full voice round trip — push-to-talk PCM → Wyoming
//! STT → final transcript → **a run started through the same path typed text
//! takes** → M4 deterministic-grammar-first routing → TTS response streamed back
//! as audio — plus barge-in and honest failure reporting (FR-13, docs/02 §9).
//!
//! Everything below drives the real daemon: real Postgres repositories, the real
//! outbox dispatcher, the real WS upgrade, the real
//! `jarvis_adapters::wyoming::WyomingClient`. Only the speech engines themselves
//! are fixtures (`voice_fixture`), per CLAUDE.md's fixture-first rule — and every
//! wait is bounded so a regression fails instead of wedging the suite.

mod voice_fixture;

use std::sync::Arc;
use std::time::Duration;

use jarvis_adapters::wyoming::WyomingClient;
use jarvis_application::testing::FakeModel;
use jarvis_application::voice::{SpeechSynthesizer, SpeechTranscriber};
use sqlx::PgPool;
use voice_fixture::{
    Harness, Received, SESSION, VoiceSocket, VoiceWiring, addr_of, audio_frame_count, events_of,
    payload_of,
};

/// Wall-clock ceiling for "the daemon should have answered by now" in these
/// fixture-driven tests. Generous relative to what the fixtures actually take.
const BUDGET: Duration = Duration::from_secs(10);

fn stt(url: &str) -> Option<Arc<dyn SpeechTranscriber>> {
    Some(Arc::new(WyomingClient::new("stt", addr_of(url))))
}

fn tts(url: &str) -> Option<Arc<dyn SpeechSynthesizer>> {
    Some(Arc::new(WyomingClient::new("tts", addr_of(url))))
}

fn is(event: &str) -> impl Fn(&Received) -> bool + '_ {
    move |received: &Received| matches!(received, Received::Event { event_type, .. } if event_type == event)
}

/// Hold the button, speak, release — and a run starts, streams, and completes.
///
/// The durable evidence is the timeline: the transcript is committed as a **user
/// message** exactly as if it had been typed, which is what proves it took
/// `RunApi::start_turn` and not some voice-only shortcut (invariant #1).
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_final_transcript_starts_a_run_on_the_normal_path(pool: PgPool) {
    let stt_url = voice_fixture::stt_returning("tell me a bedtime story").await;
    let harness = Harness::start(
        pool,
        FakeModel::streaming(["Once upon a time."]),
        VoiceWiring {
            transcriber: stt(&stt_url),
            synthesizer: None,
        },
    )
    .await;

    let mut socket = harness.connect().await;
    socket
        .send_control(VoiceSocket::start_stream("s1", Some(SESSION)))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;
    socket.send_control(VoiceSocket::stop_stream("s1")).await;

    let received = socket.collect_until(BUDGET, is("run.completed")).await;
    let events = events_of(&received);
    assert!(
        events.contains(&"voice.transcript"),
        "the recognized text is shown live: {events:?}"
    );
    assert!(
        events.contains(&"run.started"),
        "a settled transcript starts a run: {events:?}"
    );
    assert!(events.contains(&"run.completed"), "{events:?}");

    let transcript = payload_of(&received, "voice.transcript").unwrap();
    assert_eq!(transcript["text"], "tell me a bedtime story");
    assert_eq!(transcript["final"], true);

    // The unrecognized utterance did reach the model — this is the contrast case
    // for the zero-model-call test below.
    assert!(harness.model.opened());

    let timeline = poll_for_messages(&harness, 2).await;
    let messages: Vec<&serde_json::Value> = timeline["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "message")
        .collect();
    assert_eq!(messages[0]["message"]["role"], "user");
    assert_eq!(
        messages[0]["message"]["content"][0]["text"], "tell me a bedtime story",
        "the transcript is committed as an ordinary user message"
    );
    assert_eq!(messages[1]["message"]["role"], "assistant");

    harness.shutdown.cancel();
}

/// M5 exit evidence #3, from the voice side: a recognized deterministic
/// utterance is answered locally and **never opens the model** — the M4
/// `DeterministicFirstProvider` sits on the run path, so voice inherits
/// quota-first routing without a second implementation of it (docs/02 §9
/// "quota-first routing").
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_deterministic_utterance_spoken_aloud_makes_zero_model_calls(pool: PgPool) {
    let stt_url = voice_fixture::stt_returning("15% of 230").await;
    let harness = Harness::start(
        pool,
        FakeModel::streaming(["the model must not be consulted"]),
        VoiceWiring {
            transcriber: stt(&stt_url),
            synthesizer: None,
        },
    )
    .await;

    let mut socket = harness.connect().await;
    socket
        .send_control(VoiceSocket::start_stream("s1", Some(SESSION)))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;
    socket.send_control(VoiceSocket::stop_stream("s1")).await;

    let received = socket.collect_until(BUDGET, is("run.completed")).await;
    let spoken: String = received
        .iter()
        .filter_map(|r| match r {
            Received::Event {
                event_type,
                payload,
            } if event_type == "text.delta" => payload["text"].as_str(),
            _ => None,
        })
        .collect();
    assert_eq!(spoken, "15% of 230 = 34.5");
    assert!(
        !harness.model.opened(),
        "a recognized deterministic utterance must cost zero model calls"
    );

    harness.shutdown.cancel();
}

/// The response leg: a completed run's text is synthesized clause by clause and
/// arrives as binary PCM bracketed by `voice.speak.start` / `voice.speak.stop`.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_spoken_response_reaches_the_client_as_bracketed_audio(pool: PgPool) {
    let stt_url = voice_fixture::stt_returning("15% of 230").await;
    let tts_url = voice_fixture::tts_streaming(3, 1024, Duration::from_millis(5)).await;
    let harness = Harness::start(
        pool,
        FakeModel::streaming(["unused"]),
        VoiceWiring {
            transcriber: stt(&stt_url),
            synthesizer: tts(&tts_url),
        },
    )
    .await;

    let mut socket = harness.connect().await;
    socket
        .send_control(VoiceSocket::start_stream("s1", Some(SESSION)))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;
    socket.send_control(VoiceSocket::stop_stream("s1")).await;

    let received = socket.collect_until(BUDGET, is("voice.speak.stop")).await;
    let events = events_of(&received);
    assert!(
        events.contains(&"voice.speak.start"),
        "audio is announced before it is sent: {events:?}"
    );
    let start = payload_of(&received, "voice.speak.start").unwrap();
    assert_eq!(start["sampleRateHz"], 22_050);
    assert_eq!(start["channels"], 1);
    assert!(start["utteranceId"].is_string());
    assert!(
        audio_frame_count(&received) > 0,
        "synthesized PCM must reach the client: {events:?}"
    );
    let stop = payload_of(&received, "voice.speak.stop").unwrap();
    assert_eq!(
        stop["reason"], "completed",
        "an utterance that finished says so"
    );
    assert_eq!(stop["utteranceId"], start["utteranceId"]);

    harness.shutdown.cancel();
}

/// **Barge-in** (the headline of F5.2, docs/02 §9: TTS "stops immediately on
/// barge-in"). New user audio while the answer is being spoken cancels the
/// in-flight synthesis through the existing `CancellationToken` plumbing, and —
/// the assertion that actually matters — **no further audio frame is emitted for
/// the cancelled utterance**.
///
/// The TTS fixture drips a chunk every 150 ms, so the utterance is guaranteed to
/// still be in flight when the interrupt arrives; without cancellation the
/// remaining ~40 chunks would keep arriving.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn barge_in_cancels_synthesis_and_stops_the_audio(pool: PgPool) {
    let stt_url = voice_fixture::stt_returning("15% of 230").await;
    let tts_url = voice_fixture::tts_streaming(40, 1024, Duration::from_millis(150)).await;
    let harness = Harness::start(
        pool,
        FakeModel::streaming(["unused"]),
        VoiceWiring {
            transcriber: stt(&stt_url),
            synthesizer: tts(&tts_url),
        },
    )
    .await;

    let mut socket = harness.connect().await;
    socket
        .send_control(VoiceSocket::start_stream("s1", Some(SESSION)))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;
    socket.send_control(VoiceSocket::stop_stream("s1")).await;

    // Wait until the answer is genuinely being spoken.
    let before = socket
        .collect_until(BUDGET, |r| matches!(r, Received::Audio(_)))
        .await;
    assert!(
        audio_frame_count(&before) > 0,
        "precondition: audio must be flowing before barge-in is meaningful"
    );

    // The user starts talking again. This is the whole interrupt: a new capture
    // stream, nothing else.
    socket
        .send_control(VoiceSocket::start_stream("s2", Some(SESSION)))
        .await;

    let after = socket
        .collect_until(Duration::from_secs(3), is("voice.speak.stop"))
        .await;
    let stop = payload_of(&after, "voice.speak.stop")
        .expect("barge-in must report the utterance as ended, not just fall silent");
    assert_eq!(stop["reason"], "cancelled");

    // Nothing more for that utterance — the load-bearing assertion. A synthesis
    // that merely "stopped being forwarded" would still leak frames here.
    let residue = socket.drain_for(Duration::from_millis(800)).await;
    assert_eq!(
        audio_frame_count(&residue),
        0,
        "no audio may follow the cancelled utterance: {:?}",
        events_of(&residue)
    );

    harness.shutdown.cancel();
}

/// A dead STT service must be distinguishable from a user who said nothing.
/// Before F5.2 this was logged and swallowed; it is now a `voice.error` the HUD
/// can show.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_dead_stt_service_reports_voice_error_not_silence(pool: PgPool) {
    let stt_url = voice_fixture::stt_dying().await;
    let harness = Harness::start(
        pool,
        FakeModel::streaming(["unused"]),
        VoiceWiring {
            transcriber: stt(&stt_url),
            synthesizer: None,
        },
    )
    .await;

    let mut socket = harness.connect().await;
    socket
        .send_control(VoiceSocket::start_stream("s1", Some(SESSION)))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;
    socket.send_control(VoiceSocket::stop_stream("s1")).await;

    let received = socket.collect_until(BUDGET, is("voice.error")).await;
    let error = payload_of(&received, "voice.error")
        .expect("a broken STT service must surface as an error event");
    assert_eq!(error["code"], "voice.stt_failed");
    assert_eq!(error["streamId"], "s1");
    assert!(
        !events_of(&received).contains(&"run.started"),
        "no transcript is invented from a failed recognition, so no run starts"
    );

    harness.shutdown.cancel();
}

/// The same honesty on the output leg: a TTS service that dies is reported,
/// never passed off as a response that simply had nothing to say.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_dead_tts_service_reports_voice_error_not_silence(pool: PgPool) {
    let stt_url = voice_fixture::stt_returning("15% of 230").await;
    let tts_url = voice_fixture::tts_dying().await;
    let harness = Harness::start(
        pool,
        FakeModel::streaming(["unused"]),
        VoiceWiring {
            transcriber: stt(&stt_url),
            synthesizer: tts(&tts_url),
        },
    )
    .await;

    let mut socket = harness.connect().await;
    socket
        .send_control(VoiceSocket::start_stream("s1", Some(SESSION)))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;
    socket.send_control(VoiceSocket::stop_stream("s1")).await;

    let received = socket.collect_until(BUDGET, is("voice.error")).await;
    let error = payload_of(&received, "voice.error")
        .expect("a broken TTS service must surface as an error event");
    assert_eq!(error["code"], "voice.tts_failed");
    // The text answer still completed: a mute TTS never costs the user the run.
    assert!(events_of(&received).contains(&"run.completed"));

    harness.shutdown.cancel();
}

/// A capture stream that names no session is transcribed and displayed, but
/// starts nothing. Inventing a session server-side would be a second way to
/// create conversations that the REST surface does not audit.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_transcript_with_no_session_starts_no_run(pool: PgPool) {
    let stt_url = voice_fixture::stt_returning("tell me a bedtime story").await;
    let harness = Harness::start(
        pool,
        FakeModel::streaming(["must not run"]),
        VoiceWiring {
            transcriber: stt(&stt_url),
            synthesizer: None,
        },
    )
    .await;

    let mut socket = harness.connect().await;
    socket
        .send_control(VoiceSocket::start_stream("s1", None))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;
    socket.send_control(VoiceSocket::stop_stream("s1")).await;

    let received = socket.collect_until(BUDGET, is("voice.transcript")).await;
    assert!(events_of(&received).contains(&"voice.transcript"));
    let more = socket.drain_for(Duration::from_millis(500)).await;
    assert!(
        !events_of(&more).contains(&"run.started"),
        "no session ⇒ no run: {:?}",
        events_of(&more)
    );
    assert!(!harness.model.opened());

    harness.shutdown.cancel();
}

/// Poll the timeline until the assistant reply has committed (it lands just
/// after the run completes).
async fn poll_for_messages(harness: &Harness, want: usize) -> serde_json::Value {
    for _ in 0..200 {
        let timeline = harness.timeline().await;
        let count = timeline["items"]
            .as_array()
            .map(|items| items.iter().filter(|i| i["type"] == "message").count())
            .unwrap_or(0);
        if count >= want {
            return timeline;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timeline never reached {want} messages");
}
