//! S3 acceptance, socket side — the label the orchestrator produces actually
//! reaches the synthesizer, and reaches it *in time* (ADR-033 §4).
//!
//! The application-layer half of this lives in
//! `jarvis_application::speech_sensitivity_tests`; it proves the escalation is
//! emitted before any answer text. These prove the other half: this socket acts
//! on it, and acts on it for the right clause. Together they close the gap the
//! M8 security audit recorded — the routing constraint had been correct and
//! unreachable since F8.11 because nothing ever said `Sensitive`.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use futures_util::stream;
use jarvis_application::voice::{AudioFormat, SpeechSynthesizer, VoiceError};
use jarvis_contracts::CONTRACT_VERSION;
use jarvis_contracts::envelope::{Channel, EventEnvelope};
use jarvis_domain::identity::DeviceClass;
use jarvis_domain::ids::RunId;
use jarvis_domain::policy::SpeechSensitivity;
use tokio_util::sync::CancellationToken;

use super::hub::{OwnedId, WsHub, delivers_to_owner_of};
use super::voice::{ActiveSpeech, begin_speech, feed_speech};

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER_RUN: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
const THIS_DEVICE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB9";

/// Records the sensitivity each clause was synthesized under — the thing the
/// whole feature is about, and the only way to tell the two voices apart in a
/// test without a network.
#[derive(Default)]
struct RecordingSynthesizer {
    calls: Mutex<Vec<(String, SpeechSensitivity)>>,
}

impl RecordingSynthesizer {
    fn calls(&self) -> Vec<(String, SpeechSensitivity)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl SpeechSynthesizer for RecordingSynthesizer {
    fn id(&self) -> &str {
        "recording"
    }

    async fn synthesize(
        &self,
        text: &str,
        sensitivity: SpeechSensitivity,
        _cancel: CancellationToken,
    ) -> Result<
        (
            AudioFormat,
            futures_util::stream::BoxStream<'static, Result<Vec<u8>, VoiceError>>,
        ),
        VoiceError,
    > {
        self.calls
            .lock()
            .unwrap()
            .push((text.to_owned(), sensitivity));
        Ok((
            AudioFormat {
                sample_rate_hz: 16_000,
                sample_width_bytes: 2,
                channels: 1,
            },
            Box::pin(stream::iter([Ok(vec![0u8; 4])])),
        ))
    }
}

fn envelope(event_type: &str, payload: serde_json::Value) -> EventEnvelope {
    EventEnvelope {
        v: CONTRACT_VERSION,
        seq: 1,
        channel: Channel::Session,
        event_type: event_type.to_owned(),
        occurred_at: "2026-09-01T09:00:00Z".to_owned(),
        trace_id: None,
        resource_version: None,
        payload,
    }
}

fn delta(run: &str, text: &str) -> EventEnvelope {
    envelope(
        "text.delta",
        serde_json::json!({ "runId": run, "text": text }),
    )
}

fn escalation(run: &str) -> EventEnvelope {
    envelope("run.speech_sensitive", serde_json::json!({ "runId": run }))
}

fn speech_for(run: &str, synth: &Arc<RecordingSynthesizer>) -> ActiveSpeech {
    begin_speech(
        Arc::clone(synth) as Arc<dyn SpeechSynthesizer>,
        run.parse::<RunId>().unwrap(),
        CancellationToken::new(),
    )
}

// ---- the socket acts on the label ----------------------------------------

#[tokio::test]
async fn an_escalated_run_is_synthesized_as_sensitive() {
    let synth = Arc::new(RecordingSynthesizer::default());
    let mut speech = Some(speech_for(RUN, &synth));

    // The orchestrator's ordering: escalation, then the text that quotes the
    // private thing, then the terminal event that flushes and closes.
    feed_speech(&mut speech, &escalation(RUN)).unwrap();
    feed_speech(&mut speech, &delta(RUN, "Your mediation is at ten.")).unwrap();
    feed_speech(
        &mut speech,
        &envelope("run.completed", serde_json::json!({ "runId": RUN })),
    )
    .unwrap();

    let active = speech.take().expect("utterance is still active");
    active.task.await.expect("synthesis task finished");

    let calls = synth.calls();
    assert!(!calls.is_empty(), "the clause must have been synthesized");
    for (text, sensitivity) in &calls {
        assert_eq!(
            *sensitivity,
            SpeechSensitivity::Sensitive,
            "clause {text:?} went out as {sensitivity:?} — a third-party voice would have \
             read it aloud"
        );
    }
}

#[tokio::test]
async fn an_unescalated_run_is_synthesized_as_normal() {
    // The complement: without this, "always Sensitive" would pass the test
    // above while silently retiring the cloud voice ADR-033 §3 keeps.
    let synth = Arc::new(RecordingSynthesizer::default());
    let mut speech = Some(speech_for(RUN, &synth));

    feed_speech(&mut speech, &delta(RUN, "It is sunny.")).unwrap();
    feed_speech(
        &mut speech,
        &envelope("run.completed", serde_json::json!({ "runId": RUN })),
    )
    .unwrap();

    let active = speech.take().expect("utterance is still active");
    active.task.await.expect("synthesis task finished");

    for (text, sensitivity) in &synth.calls() {
        assert_eq!(
            *sensitivity,
            SpeechSensitivity::Normal,
            "clause {text:?} was needlessly downgraded to the local voice"
        );
    }
}

#[tokio::test]
async fn escalation_mid_utterance_labels_the_quoting_clause() {
    // The realistic shape of a tool run: the model opens with a filler clause
    // before the tool has returned, and only the later clause quotes the
    // private result.
    //
    // What is asserted is only that the quoting clause is `Sensitive`. It
    // deliberately does **not** assert the opener is `Normal`, which would be
    // testing a scheduling accident: whether the opener escapes as `Normal`
    // depends on whether `speak_task` was polled between the socket loop's two
    // awaits, and here — a current-thread runtime with three synchronous
    // `feed_speech` calls — it never is, so both clauses are queued before the
    // task runs at all. In production that race resolves either way and both
    // outcomes are acceptable: the per-clause read *permits* the opener to stay
    // `Normal`, it does not promise it, and losing that permission costs voice
    // quality on one clause rather than privacy.
    let synth = Arc::new(RecordingSynthesizer::default());
    let mut speech = Some(speech_for(RUN, &synth));

    feed_speech(&mut speech, &delta(RUN, "Let me check. ")).unwrap();
    feed_speech(&mut speech, &escalation(RUN)).unwrap();
    feed_speech(&mut speech, &delta(RUN, "Your mediation is at ten. ")).unwrap();
    feed_speech(
        &mut speech,
        &envelope("run.completed", serde_json::json!({ "runId": RUN })),
    )
    .unwrap();

    let active = speech.take().expect("utterance is still active");
    active.task.await.expect("synthesis task finished");

    let calls = synth.calls();
    let private = calls
        .iter()
        .find(|(text, _)| text.contains("mediation"))
        .expect("the quoting clause was synthesized");
    assert_eq!(
        private.1,
        SpeechSensitivity::Sensitive,
        "the clause after the escalation must be local-only: {calls:?}"
    );
}

#[tokio::test]
async fn an_escalation_for_another_run_does_not_leak_across_utterances() {
    // `feed_speech` filters by run id for every other event; this one must not
    // be the exception, or one room's private answer would silence-route
    // another room's weather report — and, worse, the reverse would hold for
    // any future de-escalation.
    let synth = Arc::new(RecordingSynthesizer::default());
    let mut speech = Some(speech_for(RUN, &synth));

    feed_speech(&mut speech, &escalation(OTHER_RUN)).unwrap();
    assert!(
        !speech.as_ref().unwrap().sensitive.load(Ordering::Acquire),
        "another run's escalation must not label this utterance"
    );
}

// ---- the REAL producer, end to end ---------------------------------------

/// **The envelope built the way the daemon actually builds it.**
///
/// Every other test in this file hand-writes the escalation envelope, which
/// pins `feed_speech` and the delivery rule but proves nothing about the thing
/// that emits them. That gap is this repository's most-repeated bug: an adapter
/// fixture that constructs input its own way, hiding a total mismatch with the
/// real caller while every test stays green (M5 ×3, and again at the M6 gate,
/// where every approved Home Assistant action was denied in production).
///
/// Here it would fail open and silently — a serialization that put `runId`
/// somewhere `delivers_to_owner_of` does not look, or an event-type string that
/// drifted from `SPOKEN_RUN_EVENTS`, means the node is never told and speaks
/// private content in the vendor voice. So this drives
/// `RunEventSink::emit` — what the orchestrator actually calls — and feeds the
/// **received** envelope through both the delivery rule and the synthesis path.
#[tokio::test]
async fn the_daemons_own_escalation_envelope_routes_and_labels() {
    use jarvis_application::orchestrator::{RunEventSink, RunUpdate};

    let hub = WsHub::new();
    let mut rx = hub.subscribe();
    let run_id: RunId = RUN.parse().unwrap();

    hub.emit(RunUpdate::SpeechSensitivityEscalated {
        run_id: run_id.clone(),
    })
    .await;

    let env = rx.recv().await.expect("the hub broadcast an envelope");
    assert_eq!(env.event_type, "run.speech_sensitive");
    assert_eq!(env.channel, Channel::Session);

    // The delivery rule must recognise the real payload — this is the assertion
    // a hand-built fixture cannot make.
    let owned: std::collections::VecDeque<OwnedId> =
        [OwnedId::Run(RUN.to_owned())].into_iter().collect();
    assert!(
        delivers_to_owner_of(&env, DeviceClass::RoomNode, THIS_DEVICE, &owned),
        "the daemon's own escalation envelope must reach the node that started \
         the run; payload was {:?}",
        env.payload
    );

    // And the synthesis path must act on it.
    let synth = Arc::new(RecordingSynthesizer::default());
    let mut speech = Some(speech_for(RUN, &synth));
    feed_speech(&mut speech, &env).unwrap();
    feed_speech(&mut speech, &delta(RUN, "Your mediation is at ten.")).unwrap();
    feed_speech(
        &mut speech,
        &envelope("run.completed", serde_json::json!({ "runId": RUN })),
    )
    .unwrap();

    let active = speech.take().expect("utterance is still active");
    active.task.await.expect("synthesis task finished");

    let calls = synth.calls();
    assert!(!calls.is_empty(), "the clause must have been synthesized");
    for (text, sensitivity) in &calls {
        assert_eq!(
            *sensitivity,
            SpeechSensitivity::Sensitive,
            "clause {text:?} went out as {sensitivity:?}"
        );
    }
}

// ---- the label reaches the node that speaks ------------------------------

#[test]
fn the_owning_node_receives_the_escalation_and_others_do_not() {
    // A deliberate widening of `SPOKEN_RUN_EVENTS`, tested the way that
    // constant's own comment demands: the node that started the run must hear
    // it, because it is the one choosing a voice.
    //
    // The interesting column is the satellite one. An `OwnerUi` holds `ui` and
    // therefore receives Session-channel events for its own session whether or
    // not it started the run — that is the pre-existing rule for `text.delta`
    // and every other Session event, and this event reveals strictly less than
    // the answer text already reaching that same console. A `voice-node` /
    // `room-node` holds no `ui` scope and gets in only through run ownership,
    // which is exactly where a leak between rooms would show up.
    let owned: std::collections::VecDeque<OwnedId> =
        [OwnedId::Run(RUN.to_owned())].into_iter().collect();
    let mine = escalation(RUN);
    let theirs = escalation(OTHER_RUN);

    for class in [DeviceClass::VoiceNode, DeviceClass::RoomNode] {
        assert!(
            delivers_to_owner_of(&mine, class, THIS_DEVICE, &owned),
            "{class} owning the run must receive its own escalation — without it the \
             node speaks the answer in whatever voice it defaulted to"
        );
        assert!(
            !delivers_to_owner_of(&theirs, class, THIS_DEVICE, &owned),
            "{class} must not receive another room's escalation"
        );
    }

    // And a satellite that owns nothing hears none of it.
    let nothing = std::collections::VecDeque::new();
    assert!(!delivers_to_owner_of(
        &mine,
        DeviceClass::RoomNode,
        THIS_DEVICE,
        &nothing
    ));
}
