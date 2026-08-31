use super::hub::*;
use super::replay::*;
use super::voice::*;
use async_trait::async_trait;
use futures_util::stream::{BoxStream, StreamExt};
use jarvis_application::orchestrator::{RunEventSink, RunUpdate};
use jarvis_application::voice::{AudioFormat, SpeechTranscriber, TranscriptEvent, VoiceError};
use jarvis_contracts::CONTRACT_VERSION;
use jarvis_contracts::envelope::Channel;
use jarvis_domain::ids::RunId;
use jarvis_domain::run::{RunOutcome, RunOutcomeKind, RunState};
use jarvis_infra::dispatcher::{OutboxPublisher, OutboxRecord};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

#[tokio::test]
async fn publish_builds_a_domain_envelope_carrying_the_outbox_seq() {
    let hub = WsHub::new();
    let mut rx = hub.subscribe();

    hub.publish(&OutboxRecord {
        id: 42,
        event_type: "run.state_changed".to_owned(),
        payload: json!({ "runId": RUN, "state": "model_running" }),
        created_at: OffsetDateTime::UNIX_EPOCH,
    })
    .await
    .unwrap();

    let env = rx.recv().await.unwrap();
    assert_eq!(env.seq, 42);
    // occurredAt reflects the stored commit time, not "now".
    assert_eq!(env.occurred_at, "1970-01-01T00:00:00Z");
    assert_eq!(env.v, CONTRACT_VERSION);
    assert_eq!(env.channel, Channel::Session);
    assert_eq!(env.event_type, "run.state_changed");
    // Payload forwarded verbatim; the type stays on the envelope only.
    assert_eq!(
        env.payload,
        json!({ "runId": RUN, "state": "model_running" })
    );
    assert_eq!(hub.high_water(), 42);
}

#[tokio::test]
async fn sink_broadcasts_deltas_and_drops_state_and_finished() {
    let hub = WsHub::new();
    let mut rx = hub.subscribe();
    let run_id: RunId = RUN.parse().unwrap();

    // State + finished are owned by the outbox path — dropped here.
    hub.emit(RunUpdate::StateChanged {
        run_id: run_id.clone(),
        state: RunState::ModelRunning,
    })
    .await;
    hub.emit(RunUpdate::Finished {
        run_id: run_id.clone(),
        outcome: RunOutcome {
            kind: RunOutcomeKind::Completed,
            detail: None,
        },
    })
    .await;
    // Only the transient delta is broadcast.
    hub.emit(RunUpdate::TextDelta {
        run_id: run_id.clone(),
        text: "hi".to_owned(),
    })
    .await;

    let env = rx.recv().await.unwrap();
    assert_eq!(env.event_type, "text.delta");
    assert_eq!(env.payload, json!({ "runId": RUN, "text": "hi" }));
    assert!(
        rx.try_recv().is_err(),
        "state/finished must not be broadcast"
    );
}

#[tokio::test]
async fn sink_maps_agenda_to_a_sensitivity_safe_hud_card() {
    let hub = WsHub::new();
    let mut rx = hub.subscribe();
    let run_id: RunId = RUN.parse().unwrap();
    let event = jarvis_application::calendar::CalendarEvent::new(
        "Dentist",
        SystemTime::UNIX_EPOCH,
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(3_600),
        false,
        jarvis_domain::location::Sensitivity::Sensitive,
    )
    .unwrap();

    hub.emit(RunUpdate::Agenda {
        run_id,
        events: vec![event],
    })
    .await;

    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.event_type, "hud.canvas");
    assert_eq!(
        envelope.payload["canvas"]["cards"][0]["type"],
        "card.agenda"
    );
    assert_eq!(
        envelope.payload["canvas"]["cards"][0]["events"][0],
        json!({
            "title": "Dentist",
            "start": "1970-01-01T00:00:00Z",
            "end": "1970-01-01T01:00:00Z",
            "allDay": false
        })
    );
    assert!(!envelope.payload.to_string().contains("sensitivity"));
}

#[test]
fn split_tagged_moves_the_type_out_of_the_payload() {
    let (event_type, payload) =
        split_tagged(json!({ "type": "text.delta", "runId": RUN, "text": "x" }));
    assert_eq!(event_type, "text.delta");
    assert_eq!(payload, json!({ "runId": RUN, "text": "x" }));
}

struct FakeTranscriber;

#[async_trait]
impl SpeechTranscriber for FakeTranscriber {
    fn id(&self) -> &str {
        "fake-stt"
    }

    async fn transcribe(
        &self,
        mut audio: BoxStream<'static, Vec<u8>>,
        _format: AudioFormat,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, TranscriptEvent>, jarvis_application::voice::VoiceError> {
        while audio.next().await.is_some() {}
        Ok(Box::pin(futures_util::stream::iter([
            TranscriptEvent::Partial("hello".to_owned()),
            TranscriptEvent::Final("hello Jarvis".to_owned()),
        ])))
    }
}

#[tokio::test]
async fn voice_stream_routes_pcm_to_the_transcriber_and_broadcasts_hypotheses() {
    let hub = WsHub::new();
    let mut rx = hub.subscribe();
    let (finals_tx, mut finals_rx) = mpsc::channel(4);
    let mut active = Some(start_voice_stream(
        Arc::new(FakeTranscriber),
        Arc::clone(&hub),
        "stream-1".to_owned(),
        None,
        AudioFormat {
            sample_rate_hz: 16_000,
            sample_width_bytes: 2,
            channels: 1,
        },
        CancellationToken::new(),
        finals_tx,
    ));
    active
        .as_ref()
        .unwrap()
        .audio_tx
        .as_ref()
        .unwrap()
        .send(vec![0, 1, 2, 3])
        .await
        .unwrap();
    stop_voice_stream(&mut active).await;

    let partial = rx.recv().await.unwrap();
    let final_event = rx.recv().await.unwrap();
    assert_eq!(partial.channel, Channel::Voice);
    assert_eq!(partial.event_type, "voice.transcript");
    assert_eq!(
        partial.payload,
        json!({
            "streamId": "stream-1",
            "text": "hello",
            "final": false,
        })
    );
    assert_eq!(final_event.payload["final"], json!(true));
    assert_eq!(final_event.payload["text"], json!("hello Jarvis"));
    // The settled transcript is also handed to the socket loop, which is
    // the only place holding the device identity a run may be attributed to.
    assert_eq!(
        finals_rx.recv().await.map(|turn| turn.text).as_deref(),
        Some("hello Jarvis")
    );
}

struct BrokenTranscriber;

#[async_trait]
impl SpeechTranscriber for BrokenTranscriber {
    fn id(&self) -> &str {
        "broken-stt"
    }

    async fn transcribe(
        &self,
        _audio: BoxStream<'static, Vec<u8>>,
        _format: AudioFormat,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, TranscriptEvent>, VoiceError> {
        Ok(Box::pin(futures_util::stream::iter([
            TranscriptEvent::Error(VoiceError::Unavailable("connect failed".to_owned())),
        ])))
    }
}

/// A dead STT service must be distinguishable from silence: without a
/// `voice.error` event the browser sees only the absence of a transcript.
#[tokio::test]
async fn a_broken_stt_service_surfaces_voice_error_rather_than_silence() {
    let hub = WsHub::new();
    let mut rx = hub.subscribe();
    let (finals_tx, mut finals_rx) = mpsc::channel(4);
    let mut active = Some(start_voice_stream(
        Arc::new(BrokenTranscriber),
        Arc::clone(&hub),
        "stream-err".to_owned(),
        None,
        AudioFormat {
            sample_rate_hz: 16_000,
            sample_width_bytes: 2,
            channels: 1,
        },
        CancellationToken::new(),
        finals_tx,
    ));
    stop_voice_stream(&mut active).await;

    let event = rx.recv().await.unwrap();
    assert_eq!(event.channel, Channel::Voice);
    assert_eq!(event.event_type, "voice.error");
    assert_eq!(
        event.payload,
        json!({ "streamId": "stream-err", "code": "voice.stt_unavailable" })
    );
    // No transcript is invented from a failed recognition, so no run starts.
    assert!(finals_rx.recv().await.is_none());
}

/// The settled turn is handed over on a **bounded** queue, and the socket
/// loop is by construction not draining it while it is inside this very
/// teardown. A handover with nowhere to go must therefore not make the
/// teardown unbounded — it did, before this test: the loop waited 5 s,
/// cancelled a token a blocked `send` could not observe, and then awaited
/// the task forever, wedging the whole connection (no inbound frames, no
/// outbound events, and the `state.shutdown` branch never polled again).
///
/// The handover must therefore not *wait* at all: the assertion is that the
/// teardown finishes well inside the settle grace, which fails both for the
/// original unbounded await (it never returns) and for a merely-bounded
/// blocking `send` (it would burn the whole grace on every such frame — a
/// stall a client can trigger at will).
#[tokio::test]
async fn a_blocked_transcript_handover_cannot_wedge_the_capture_teardown() {
    let started = std::time::Instant::now();
    let hub = WsHub::new();
    // Capacity 1, already full, and nothing will ever read it: exactly the
    // state a pipelined burst of `voice.stream.start` frames produces.
    let (finals_tx, _finals_rx) = mpsc::channel::<VoiceTurn>(1);
    finals_tx
        .send(VoiceTurn {
            stream_id: "already-queued".to_owned(),
            session_id: None,
            text: "already queued".to_owned(),
        })
        .await
        .unwrap();

    let cancel = CancellationToken::new();
    let mut active = Some(start_voice_stream(
        Arc::new(FakeTranscriber),
        Arc::clone(&hub),
        "stream-wedge".to_owned(),
        None,
        AudioFormat {
            sample_rate_hz: 16_000,
            sample_width_bytes: 2,
            channels: 1,
        },
        cancel.clone(),
        finals_tx,
    ));

    tokio::time::timeout(
        VOICE_STREAM_SETTLE_GRACE + VOICE_STREAM_CANCEL_GRACE + Duration::from_secs(5),
        stop_voice_stream(&mut active),
    )
    .await
    .expect("stopping a capture stream must be bounded even when its handover is blocked");
    assert!(active.is_none());
    assert!(
        started.elapsed() < VOICE_STREAM_SETTLE_GRACE,
        "a blocked handover must not cost the settle grace; took {:?}",
        started.elapsed()
    );
    // The stream settled on its own, so nothing had to be cancelled.
    assert!(!cancel.is_cancelled());
}

/// The id is echoed into events the hub broadcasts to **every** connected
/// socket, so it is bounded at the boundary rather than trusted to be sane.
#[test]
fn an_unbounded_or_control_laden_stream_id_is_not_acceptable() {
    assert!(stream_id_is_acceptable("s1"));
    assert!(stream_id_is_acceptable(&"x".repeat(MAX_STREAM_ID_CHARS)));
    assert!(!stream_id_is_acceptable(""));
    assert!(!stream_id_is_acceptable(
        &"x".repeat(MAX_STREAM_ID_CHARS + 1)
    ));
    // Bounded in CHARACTERS, not bytes: a multi-byte id of acceptable
    // length is fine, and a long one is still rejected.
    assert!(stream_id_is_acceptable("é"));
    assert!(!stream_id_is_acceptable(
        &"é".repeat(MAX_STREAM_ID_CHARS + 1)
    ));
    assert!(!stream_id_is_acceptable("has\nnewline"));
    assert!(!stream_id_is_acceptable("has\u{7}bell"));
}

/// The per-stream format is client-controlled and is handed straight to the
/// speech service; only the format the daemon is configured for is accepted.
#[test]
fn only_the_configured_capture_format_is_accepted() {
    assert_eq!(
        accepted_audio_format(16_000, 2, 1),
        Some(AudioFormat {
            sample_rate_hz: 16_000,
            sample_width_bytes: 2,
            channels: 1,
        })
    );
    assert!(accepted_audio_format(48_000, 2, 2).is_some());
    // Not s16le.
    assert!(accepted_audio_format(16_000, 4, 1).is_none());
    assert!(accepted_audio_format(16_000, 0, 1).is_none());
    // Nonsense channel counts and rates never reach the speech service.
    assert!(accepted_audio_format(16_000, 2, 0).is_none());
    assert!(accepted_audio_format(16_000, 2, 64).is_none());
    assert!(accepted_audio_format(0, 2, 1).is_none());
    assert!(accepted_audio_format(u32::MAX, 2, 1).is_none());
}

#[test]
fn seq_clamps_a_nonpositive_id() {
    assert_eq!(seq_of(7), 7);
    assert_eq!(seq_of(0), 0);
    assert_eq!(seq_of(-1), 0);
}
