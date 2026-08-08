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

/// Ceiling for the wedge regression. Generous on purpose: a socket recovering
/// from an over-capacity handover legitimately waits out one settle grace, and
/// the point of the bound is that a *permanent* wedge fails the test instead of
/// hanging the suite — not to measure how quickly it recovers.
const WEDGE_BUDGET: Duration = Duration::from_secs(20);

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

/// A pipelined burst of `voice.stream.start` frames must not be able to wedge
/// the socket — the M5 audit's most severe finding, and the one an ordinary
/// fast barge-in storm on a slow STT reaches by accident.
///
/// Each `voice.stream.start` ends the previous capture stream **inline**, and
/// each ended stream hands one settled turn to the socket loop's bounded
/// `finals` queue — which the loop cannot be draining, because it is inside that
/// very teardown. Enough starts back to back and the handover has nowhere to go.
/// Before the fix the loop then waited out its grace, cancelled a token the
/// blocked `send` could not observe, and awaited the task **forever**: no
/// inbound frames, no outbound events, and the `state.shutdown` branch never
/// polled again, so graceful drain never completed for that connection.
///
/// Both waits below are bounded, so a regression fails here rather than hanging
/// the suite (which is exactly how this bug presented: a test binary that never
/// finished).
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_pipelined_burst_of_capture_streams_cannot_wedge_the_socket(pool: PgPool) {
    let stt_url = voice_fixture::stt_returning("15% of 230").await;
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
    // Six starts: five of them close a predecessor, i.e. one more settled turn
    // than the handover queue can hold. They name no session deliberately —
    // filling the queue is what this exercises, and six concurrent runs would
    // only add load that has nothing to do with the wedge.
    for n in 0..6 {
        socket
            .send_control(VoiceSocket::start_stream(&format!("burst-{n}"), None))
            .await;
        socket.send_pcm(vec![0u8; 640]).await;
    }
    socket
        .send_control(VoiceSocket::stop_stream("burst-5"))
        .await;

    // Still serving: an ordinary turn on the same connection is transcribed and
    // run to completion afterwards.
    socket
        .send_control(VoiceSocket::start_stream("after-burst", Some(SESSION)))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;
    socket
        .send_control(VoiceSocket::stop_stream("after-burst"))
        .await;

    let received = socket
        .collect_until(WEDGE_BUDGET, is("run.completed"))
        .await;
    assert!(
        events_of(&received).contains(&"run.completed"),
        "the socket must still serve a turn after a pipelined burst: {:?}",
        events_of(&received)
    );

    // ...and graceful drain still reaches this connection.
    harness.shutdown.cancel();
    assert!(
        socket.closed_within(WEDGE_BUDGET).await,
        "a shutdown must still close this socket — a wedged loop never polls its shutdown branch"
    );
}

/// A second voice turn supersedes the answer still being spoken, so the
/// utterance it replaces is **cancelled and reported**, never silently dropped.
///
/// Barge-in does not cover this case: it fires at `voice.stream.start`, which
/// here is strictly *before* the first turn's transcript has even settled — so
/// nothing is being spoken yet when it runs. The overwrite happens later, when
/// the second settled transcript is dequeued, and before the fix it replaced the
/// `ActiveSpeech` outright: the old synthesis task was neither cancelled nor
/// aborted (it kept pulling PCM from the speech service, holding that connection
/// open), and the client never received the `voice.speak.stop` its playback
/// bookkeeping waits for — it would still believe the first utterance was live.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_second_voice_turn_reports_the_utterance_it_supersedes(pool: PgPool) {
    let stt_url = voice_fixture::stt_returning("15% of 230").await;
    // Slow enough that the first answer is unambiguously still being spoken
    // when the second turn lands.
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
        .send_control(VoiceSocket::start_stream("a", Some(SESSION)))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;
    // Opening capture B is what releases A: it ends stream A inline. Nothing is
    // being spoken yet at this point, so this is not the barge-in path.
    socket
        .send_control(VoiceSocket::start_stream("b", Some(SESSION)))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;

    // A's answer is now genuinely being spoken.
    let spoken = socket
        .collect_until(BUDGET, |r| matches!(r, Received::Audio(_)))
        .await;
    let start_a = payload_of(&spoken, "voice.speak.start")
        .expect("precondition: the first answer must be announced and speaking")
        .clone();
    assert!(audio_frame_count(&spoken) > 0);

    // Release B. Its transcript becomes a second turn, which supersedes A.
    socket.send_control(VoiceSocket::stop_stream("b")).await;

    let after = socket.collect_until(BUDGET, is("voice.speak.stop")).await;
    let stop = payload_of(&after, "voice.speak.stop")
        .expect("a superseded utterance must be reported as ended, not silently replaced");
    assert_eq!(
        stop["utteranceId"], start_a["utteranceId"],
        "the bracket that closes must be the superseded utterance's own"
    );
    assert_eq!(stop["reason"], "cancelled");

    harness.shutdown.cancel();
}

/// Everything a client puts on `voice.stream.start` is checked at the boundary.
///
/// `streamId` is echoed into every `voice.transcript` and `voice.error` the hub
/// **broadcasts to every connected socket**, so an id bounded only by the 64 KiB
/// frame cap is an amplification lever; the rejection itself must not echo it
/// back either. The PCM format is handed straight to the speech service —
/// `[voice].audio` in the config constrains only what the *daemon* is set up
/// for, never what a client may declare per stream.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn an_unacceptable_voice_stream_start_is_rejected_at_the_boundary(pool: PgPool) {
    let stt_url = voice_fixture::stt_returning("15% of 230").await;
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
    let oversized = "x".repeat(4_096);
    socket
        .send_control(VoiceSocket::start_stream(&oversized, Some(SESSION)))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;
    socket
        .send_control(VoiceSocket::stop_stream(&oversized))
        .await;

    let received = socket.drain_for(Duration::from_millis(500)).await;
    let events = events_of(&received);
    assert!(
        !events.contains(&"voice.transcript") && !events.contains(&"run.started"),
        "a rejected capture stream starts nothing: {events:?}"
    );
    for item in &received {
        if let Received::Event { payload, .. } = item {
            assert!(
                !payload.to_string().contains(&oversized),
                "the rejected id must never reach a broadcast envelope"
            );
        }
    }

    // A control-character id is rejected on the same grounds.
    socket
        .send_control(VoiceSocket::start_stream("bad\nid", Some(SESSION)))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;
    let received = socket.drain_for(Duration::from_millis(500)).await;
    assert!(
        !events_of(&received).contains(&"voice.transcript"),
        "a control-character id starts nothing: {:?}",
        events_of(&received)
    );

    // 32-bit samples at an absurd rate on 64 channels: nothing the configured
    // s16le pipeline can honestly claim to have captured.
    socket
        .send_control(serde_json::json!({
            "type": "voice.stream.start",
            "streamId": "wrong-format",
            "sessionId": SESSION,
            "sampleRateHz": 4_000_000_u32,
            "sampleWidthBytes": 4,
            "channels": 64,
        }))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;
    socket
        .send_control(VoiceSocket::stop_stream("wrong-format"))
        .await;

    let received = socket.drain_for(Duration::from_millis(500)).await;
    let events = events_of(&received);
    assert!(
        !events.contains(&"voice.transcript") && !events.contains(&"run.started"),
        "an unsupported capture format starts no transcription: {events:?}"
    );

    // The connection itself is unharmed.
    socket
        .send_control(VoiceSocket::start_stream("ok", Some(SESSION)))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;
    socket.send_control(VoiceSocket::stop_stream("ok")).await;
    let received = socket.collect_until(BUDGET, is("voice.transcript")).await;
    assert_eq!(
        payload_of(&received, "voice.transcript").unwrap()["streamId"],
        "ok"
    );

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
