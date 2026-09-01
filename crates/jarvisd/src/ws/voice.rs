use super::hub::*;
use super::replay::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::ws::WebSocket;
use futures_util::stream::{BoxStream, StreamExt, poll_fn};
use jarvis_application::voice::{
    AudioFormat, ClauseSegmenter, SpeechSensitivity, SpeechSynthesizer, SpeechTranscriber,
    TranscriptEvent, VoiceError,
};
use jarvis_contracts::CONTRACT_VERSION;
use jarvis_contracts::envelope::{Channel, EventEnvelope};
use jarvis_contracts::voice::{VoiceControlDto, VoiceErrorCodeDto, VoiceSpeakEndDto};
use jarvis_domain::ids::{RunId, SessionId};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Outbound synthesized-audio frame ceiling. A Wyoming `audio-chunk` is normally
/// a few KB, but the adapter tolerates up to `MAX_PAYLOAD_BYTES` (4 MiB), so the
/// socket re-chunks rather than emitting one frame a client (or an intermediary)
/// might refuse. Kept below [`crate::ws::socket::MAX_INBOUND_FRAME_BYTES`] so both directions of the
/// voice channel obey the same order of magnitude.
pub(crate) const MAX_OUTBOUND_AUDIO_FRAME_BYTES: usize = 32 * 1024;

/// Clauses that may sit queued for the synthesizer before backpressure is a real
/// problem rather than jitter. A model answer of 64 unspoken clauses means
/// synthesis has fallen hopelessly behind the response; the socket loop must
/// never *block* on this channel (it also carries every other client's events),
/// so an overflow is reported as a TTS failure instead of being waited out or
/// silently dropped.
pub(crate) const CLAUSE_QUEUE_CAPACITY: usize = 64;

/// PCM chunks buffered between the synthesis task and the socket loop.
pub(crate) const AUDIO_QUEUE_CAPACITY: usize = 32;

/// Bound on how long a cancelled synthesis task is awaited before the socket
/// loop stops waiting for it. Barge-in must be prompt (docs/02 §9: TTS "stops
/// immediately on barge-in"), and the task is already detached from the audio
/// path by then — the receiver is dropped, so it can emit nothing further.
const SPEECH_CANCEL_GRACE: Duration = Duration::from_millis(250);

/// Bound on how long a capture stream's transcription task is awaited after end
/// of speech (`voice.stream.stop`, barge-in, socket close, shutdown). Closing
/// the audio channel is what makes the STT service settle the utterance, so the
/// task is given room to produce it — but the settled turn reaches the socket
/// loop through the `finals` queue and the hub, never through this await, so the
/// bound is a pure liveness guard.
pub(crate) const VOICE_STREAM_SETTLE_GRACE: Duration = Duration::from_secs(5);

/// Bound on how long a **cancelled** transcription task is awaited before it is
/// abandoned. Same reasoning as [`SPEECH_CANCEL_GRACE`]: cancellation is already
/// signalled and the audio path is already severed, so the socket loop — which
/// also carries every other event for this client — must not stall on a slow
/// speech service winding down. Without this bound a task that cannot observe
/// its token (one blocked on a full `finals` queue, say) wedges the socket
/// permanently: no inbound frames, no outbound events, and no graceful drain.
pub(crate) const VOICE_STREAM_CANCEL_GRACE: Duration = Duration::from_millis(250);

/// Ceiling on a client-supplied `streamId`. The id is echoed into every
/// `voice.transcript`/`voice.error` envelope, and the hub **broadcasts those to
/// every connected socket** — so an id bounded only by the 64 KiB frame cap is
/// an amplification lever: one `voice.stream.start` becomes one oversized copy
/// per partial transcript per connected client. A real id is a short opaque
/// handle.
pub(crate) const MAX_STREAM_ID_CHARS: usize = 64;

/// The PCM format this daemon accepts on `voice.stream.start`.
/// `Config::validate` pins `[voice].audio` to `s16le` with a positive rate and
/// channel count, but that constrains the *daemon's* configuration only: the
/// per-stream format on this frame is client-controlled and is forwarded
/// verbatim to the speech service, so it is validated here rather than trusted.
/// A mismatch is rejected rather than coerced, so the audio the service receives
/// stays exactly the audio the client said it was sending.
const VOICE_SAMPLE_WIDTH_BYTES: u16 = 2; // s16le
const VOICE_MAX_CHANNELS: u16 = 2;
const VOICE_MIN_SAMPLE_RATE_HZ: u32 = 8_000;
const VOICE_MAX_SAMPLE_RATE_HZ: u32 = 48_000;

pub(crate) struct ActiveVoiceStream {
    pub(crate) stream_id: String,
    pub(crate) audio_tx: Option<mpsc::Sender<Vec<u8>>>,
    pub(crate) cancel: CancellationToken,
    pub(crate) task: tokio::task::JoinHandle<()>,
}

fn audio_stream(rx: mpsc::Receiver<Vec<u8>>) -> BoxStream<'static, Vec<u8>> {
    let mut rx = rx;
    Box::pin(poll_fn(move |cx| rx.poll_recv(cx)))
}

/// Whether a client-supplied `streamId` may be adopted — and therefore echoed
/// into broadcast envelopes (see [`MAX_STREAM_ID_CHARS`]). Bounded, non-empty,
/// and free of control characters, which have no place in an opaque handle and
/// could corrupt a client's rendering of it. Rejected rather than truncated: a
/// truncated id still echoes attacker-chosen bytes and silently renames the
/// stream out from under the client that opened it.
pub(crate) fn stream_id_is_acceptable(stream_id: &str) -> bool {
    !stream_id.is_empty()
        && stream_id.chars().count() <= MAX_STREAM_ID_CHARS
        && !stream_id.chars().any(char::is_control)
}

/// The client's declared capture format, or `None` when it is not one this
/// daemon will forward to a speech service (see [`VOICE_SAMPLE_WIDTH_BYTES`]).
pub(crate) fn accepted_audio_format(
    sample_rate_hz: u32,
    sample_width_bytes: u16,
    channels: u16,
) -> Option<AudioFormat> {
    let acceptable = sample_width_bytes == VOICE_SAMPLE_WIDTH_BYTES
        && (1..=VOICE_MAX_CHANNELS).contains(&channels)
        && (VOICE_MIN_SAMPLE_RATE_HZ..=VOICE_MAX_SAMPLE_RATE_HZ).contains(&sample_rate_hz);
    acceptable.then_some(AudioFormat {
        sample_rate_hz,
        sample_width_bytes,
        channels,
    })
}

/// Map an adapter-side failure to the stable wire code for its leg. The
/// adapter's own message is deliberately dropped here (it is only ever logged),
/// so no transport text reaches the browser.
fn stt_error_code(error: &VoiceError) -> VoiceErrorCodeDto {
    match error {
        VoiceError::Unavailable(_) => VoiceErrorCodeDto::SttUnavailable,
        VoiceError::Malformed(_) | VoiceError::Cancelled => VoiceErrorCodeDto::SttFailed,
    }
}

fn tts_error_code(error: &VoiceError) -> VoiceErrorCodeDto {
    match error {
        VoiceError::Unavailable(_) => VoiceErrorCodeDto::TtsUnavailable,
        VoiceError::Malformed(_) | VoiceError::Cancelled => VoiceErrorCodeDto::TtsFailed,
    }
}

/// A settled voice turn on its way from the transcription task to the socket
/// loop. It carries its own session because the capture stream it came from is
/// already torn down by the time the loop sees it (release-to-talk *is* the end
/// of the stream) — looking the session up from the live stream would lose it.
pub(crate) struct VoiceTurn {
    pub(crate) stream_id: String,
    pub(crate) session_id: Option<SessionId>,
    pub(crate) text: String,
}

/// `session_id` is the conversation this push-to-talk turn belongs to, from the
/// `voice.stream.start` frame. Absent ⇒ the transcript is displayed but no run
/// is started: a run needs a session, and inventing one server-side would be a
/// second, weaker way to create conversations than the audited REST endpoint.
pub(crate) fn start_voice_stream(
    transcriber: Arc<dyn SpeechTranscriber>,
    hub: Arc<WsHub>,
    stream_id: String,
    session_id: Option<SessionId>,
    format: AudioFormat,
    cancel: CancellationToken,
    finals: mpsc::Sender<VoiceTurn>,
) -> ActiveVoiceStream {
    let (tx, rx) = mpsc::channel(32);
    let task_stream_id = stream_id.clone();
    let task_session_id = session_id.clone();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        let result = transcriber
            .transcribe(audio_stream(rx), format, task_cancel.clone())
            .await;
        let mut transcript = match result {
            Ok(transcript) => transcript,
            Err(error) => {
                tracing::warn!(%error, stream_id = %task_stream_id, "voice transcription could not start");
                hub.broadcast_voice_error(&task_stream_id, stt_error_code(&error));
                return;
            }
        };
        while let Some(event) = transcript.next().await {
            match event {
                TranscriptEvent::Partial(text) => {
                    hub.broadcast_voice_transcript(&task_stream_id, text, false)
                }
                TranscriptEvent::Final(text) => {
                    hub.broadcast_voice_transcript(&task_stream_id, text.clone(), true);
                    // Hand the settled transcript to the socket loop, which owns
                    // the authenticated device identity and therefore the only
                    // path that may start a run. One final per turn: a service
                    // emitting more would otherwise be able to start unbounded
                    // runs from a single button press.
                    //
                    // Handed over WITHOUT waiting, deliberately. `finals` is
                    // bounded, and the socket loop is not always draining it —
                    // it is, for instance, inside this very stream's teardown
                    // awaiting this task. A blocking `send` there is an
                    // unbounded await that no `CancellationToken` can reach
                    // (cancelling a token does not interrupt a blocked `send`),
                    // which is exactly the "not cancellable" case invariant #4
                    // forbids: it wedged the socket loop permanently — no
                    // inbound frames, no outbound events, no graceful drain.
                    //
                    // A full queue means the loop is already four settled turns
                    // behind, which push-to-talk cannot reach without
                    // pipelining. The transcript itself was broadcast above, so
                    // the user still sees what was heard; only the run is not
                    // started, and that is reported rather than waited out.
                    let turn = VoiceTurn {
                        stream_id: task_stream_id.clone(),
                        session_id: task_session_id.clone(),
                        text,
                    };
                    if let Err(error) = finals.try_send(turn) {
                        tracing::warn!(
                            stream_id = %task_stream_id,
                            reason = match error {
                                mpsc::error::TrySendError::Full(_) => "queue full",
                                mpsc::error::TrySendError::Closed(_) => "socket closed",
                            },
                            "settled voice transcript starts no run"
                        );
                    }
                    break;
                }
                // A mid-stream STT failure. A stream that simply ends means the
                // service finished normally, so this must surface as its own
                // event — otherwise a dead STT service is indistinguishable from
                // silence to the user (F5.1's `TranscriptEvent::Error` doc).
                TranscriptEvent::Error(error) => {
                    tracing::warn!(%error, stream_id = %task_stream_id, "voice transcription failed mid-stream");
                    hub.broadcast_voice_error(&task_stream_id, stt_error_code(&error));
                    break;
                }
            }
        }
    });
    ActiveVoiceStream {
        stream_id,
        audio_tx: Some(tx),
        cancel,
        task,
    }
}

/// End one capture stream: close its audio channel (end of speech), then stop
/// waiting for its transcription task within a bounded, cancellable staircase.
///
/// Every wait here is bounded. Dropping the audio sender is what lets the STT
/// service settle the utterance, so the task gets [`VOICE_STREAM_SETTLE_GRACE`]
/// to finish normally; past that it is cancelled, given
/// [`VOICE_STREAM_CANCEL_GRACE`] to unwind, and finally aborted — the same
/// escalation [`cancel_speech`] applies to synthesis. The second wait used to be
/// an unbounded `task.await`, which meant a task that could not observe its
/// token wedged the socket loop for good: no inbound frames, no outbound events,
/// and the `state.shutdown` branch never reached, so graceful drain never
/// completed for that connection.
pub(crate) async fn stop_voice_stream(active: &mut Option<ActiveVoiceStream>) {
    stop_voice_stream_with(active, StreamStop::LetItSettle).await;
}

/// Whether a capture stream being torn down deserves the settle grace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamStop {
    /// Graceful teardown: the utterance in flight is the owner's, and letting
    /// the STT service settle it is what makes barge-in and shutdown clean.
    LetItSettle,
    /// The device's authority is **gone** (F7.1 revocation). A revoked
    /// microphone's speech is not a turn worth completing: keeping the settle
    /// grace would feed up to `VOICE_STREAM_SETTLE_GRACE` more of its audio to
    /// the speech service, broadcast the resulting transcript, and delay the
    /// close frame by the same amount. Cancel first, ask nothing.
    Immediately,
}

pub(crate) async fn stop_voice_stream_with(
    active: &mut Option<ActiveVoiceStream>,
    stop: StreamStop,
) {
    let Some(mut stream) = active.take() else {
        return;
    };
    stream.audio_tx.take();
    if stop == StreamStop::Immediately {
        stream.cancel.cancel();
        if tokio::time::timeout(VOICE_STREAM_CANCEL_GRACE, &mut stream.task)
            .await
            .is_err()
        {
            tracing::warn!(
                stream_id = %stream.stream_id,
                "revoked device's transcription task did not unwind; abandoning it"
            );
            stream.task.abort();
        }
        return;
    }
    if tokio::time::timeout(VOICE_STREAM_SETTLE_GRACE, &mut stream.task)
        .await
        .is_err()
    {
        stream.cancel.cancel();
        if tokio::time::timeout(VOICE_STREAM_CANCEL_GRACE, &mut stream.task)
            .await
            .is_err()
        {
            tracing::warn!(
                stream_id = %stream.stream_id,
                "voice transcription task did not settle after cancellation; abandoning it"
            );
            stream.task.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Spoken response leg (F5.2, docs/02 §9)
// ---------------------------------------------------------------------------

/// One item on the synthesis task → socket-loop path. The task never touches the
/// `WebSocket` (the socket loop owns it exclusively); it reports what it
/// produced and the loop decides what reaches the client.
pub(crate) enum SpeechChunk {
    /// The first clause synthesized; carries the negotiated PCM format.
    Started(AudioFormat),
    Audio(Vec<u8>),
    Ended(VoiceSpeakEndDto, Option<VoiceErrorCodeDto>),
}

/// Spoken output for one run's response, in flight on this socket.
pub(crate) struct ActiveSpeech {
    pub(crate) utterance_id: String,
    pub(crate) run_id: RunId,
    /// Cancelled on barge-in / socket close / shutdown. A child of the socket's
    /// token, so every ancestor cancellation reaches it (invariant #4) — this is
    /// the existing `CancellationToken` plumbing, not a second mechanism.
    pub(crate) cancel: CancellationToken,
    pub(crate) task: tokio::task::JoinHandle<()>,
    /// `None` once the response finished and the queue was closed.
    pub(crate) clauses: Option<mpsc::Sender<String>>,
    pub(crate) audio: mpsc::Receiver<SpeechChunk>,
    pub(crate) segmenter: ClauseSegmenter,
    pub(crate) announced: bool,
    /// Whether this run's answer must stay in the house (S3, ADR-033 §4).
    ///
    /// Shared with the synthesis task rather than passed to it, because the
    /// answer to "may a third party say this?" is not known when the utterance
    /// starts: the run has not run yet. It arrives mid-flight, on the same
    /// socket subscription the text does.
    ///
    /// Set-only. There is no code path that clears it — a run that has touched
    /// private content has touched it, and `escalate` in the domain is the same
    /// one-way rule expressed for values.
    pub(crate) sensitive: Arc<AtomicBool>,
}

/// Drive synthesis for one utterance: clauses in, PCM out, strictly in order.
///
/// Sequential by construction — clause N+1 is not synthesized until clause N's
/// audio has been handed over — because spoken output that arrives out of order
/// is worse than spoken output that arrives late.
async fn speak_task(
    synthesizer: Arc<dyn SpeechSynthesizer>,
    mut clauses: mpsc::Receiver<String>,
    out: mpsc::Sender<SpeechChunk>,
    // Whether this utterance may be spoken by a third-party voice (F8.11, S3).
    // Labelled by the producer, never sniffed from the text: a heuristic
    // guessing whether a sentence is private fails open and silently.
    //
    // Read **per clause**, immediately before each `synthesize` call, rather
    // than captured once when the task starts. That is not defensive style, it
    // is the only ordering that can work: the escalation is a consequence of
    // the run, so it necessarily arrives after the utterance began. Reading it
    // late is what lets a "let me check…" opener go out in the nice voice while
    // the sentence that actually quotes the calendar does not.
    //
    // Mirrors how the ElevenLabs adapter reads its consent gate — per
    // utterance, so a change takes effect on the next sentence (ADR-033 §2).
    sensitive: Arc<AtomicBool>,
    cancel: CancellationToken,
) {
    let mut announced = false;
    let mut ended = VoiceSpeakEndDto::Completed;
    let mut failure: Option<VoiceErrorCodeDto> = None;

    'utterance: loop {
        let clause = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            clause = clauses.recv() => clause,
        };
        let Some(clause) = clause else { break };

        // Read here, per clause — see the parameter's note. `Acquire` pairs
        // with the socket loop's `Release` store so the escalation is visible
        // to this task by the time the clause it labels arrives.
        let speech_sensitivity = if sensitive.load(Ordering::Acquire) {
            SpeechSensitivity::Sensitive
        } else {
            SpeechSensitivity::Normal
        };

        let (format, mut pcm) = match synthesizer
            .synthesize(&clause, speech_sensitivity, cancel.clone())
            .await
        {
            Ok(started) => started,
            Err(VoiceError::Cancelled) => {
                ended = VoiceSpeakEndDto::Cancelled;
                break;
            }
            Err(error) => {
                tracing::warn!(%error, "speech synthesis could not start");
                ended = VoiceSpeakEndDto::Failed;
                failure = Some(tts_error_code(&error));
                break;
            }
        };
        if !announced {
            if out.send(SpeechChunk::Started(format)).await.is_err() {
                return; // socket loop gone; nothing left to report to
            }
            announced = true;
        }

        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    ended = VoiceSpeakEndDto::Cancelled;
                    break 'utterance;
                }
                next = pcm.next() => next,
            };
            match next {
                Some(Ok(bytes)) => {
                    if out.send(SpeechChunk::Audio(bytes)).await.is_err() {
                        return;
                    }
                }
                // A truncated utterance is reported, never passed off as a
                // complete one (the `Result` chunk exists for exactly this).
                Some(Err(error)) => {
                    tracing::warn!(%error, "speech synthesis failed mid-utterance");
                    ended = VoiceSpeakEndDto::Failed;
                    failure = Some(tts_error_code(&error));
                    break 'utterance;
                }
                None => break, // this clause finished; go on to the next
            }
        }
    }

    if cancel.is_cancelled() {
        ended = VoiceSpeakEndDto::Cancelled;
        failure = None;
    }
    let _ = out.send(SpeechChunk::Ended(ended, failure)).await;
}

pub(crate) fn begin_speech(
    synthesizer: Arc<dyn SpeechSynthesizer>,
    run_id: RunId,
    cancel: CancellationToken,
) -> ActiveSpeech {
    let (clause_tx, clause_rx) = mpsc::channel(CLAUSE_QUEUE_CAPACITY);
    let (audio_tx, audio_rx) = mpsc::channel(AUDIO_QUEUE_CAPACITY);
    // Starts `Normal` and is raised by `feed_speech` if the run says so (S3).
    // Starting `Sensitive` and relaxing would be the safer-looking default and
    // the wrong one: nothing ever lowers it, so every answer in the house would
    // be local forever and the label would stop meaning anything.
    let sensitive = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn(speak_task(
        synthesizer,
        clause_rx,
        audio_tx,
        Arc::clone(&sensitive),
        cancel.clone(),
    ));
    ActiveSpeech {
        // Opaque, per-utterance: the client uses it only to discard audio that
        // belongs to an utterance it has already been told ended.
        utterance_id: ulid::Ulid::new().to_string(),
        run_id,
        cancel,
        task,
        clauses: Some(clause_tx),
        audio: audio_rx,
        segmenter: ClauseSegmenter::new(),
        announced: false,
        sensitive,
    }
}

/// Await the next chunk of the in-flight utterance, or park forever when nothing
/// is being spoken (so the socket's `select!` has a branch it can always poll).
pub(crate) async fn next_speech_chunk(speech: &mut Option<ActiveSpeech>) -> Option<SpeechChunk> {
    match speech {
        Some(active) => active.audio.recv().await,
        None => std::future::pending().await,
    }
}

/// Stop the in-flight utterance **now** (barge-in, socket close, shutdown).
///
/// Taking the [`ActiveSpeech`] out of the socket loop drops its receiver, so the
/// loop structurally cannot emit another audio frame for that utterance no
/// matter what the synthesis task does next; cancelling the token then aborts
/// the synthesis stream at the adapter. The task is awaited only briefly — it is
/// already detached from the audio path — so barge-in is not gated on a slow
/// TTS service winding down.
pub(crate) async fn cancel_speech(
    socket: &mut WebSocket,
    hub: &WsHub,
    speech: &mut Option<ActiveSpeech>,
) -> Result<(), ()> {
    let Some(mut active) = speech.take() else {
        return Ok(());
    };
    active.cancel.cancel();
    drop(active.clauses.take());
    if tokio::time::timeout(SPEECH_CANCEL_GRACE, &mut active.task)
        .await
        .is_err()
    {
        // Cancellation is signalled and the audio path is severed; leaving the
        // task to unwind on its own is bounded by the adapter's own cancellation
        // handling, and the socket must not stall waiting for it.
        active.task.abort();
    }
    if active.announced {
        send_speak_control(
            socket,
            hub,
            &VoiceControlDto::SpeakStop {
                utterance_id: active.utterance_id,
                reason: VoiceSpeakEndDto::Cancelled,
            },
        )
        .await?;
    }
    Ok(())
}

/// Feed one broadcast envelope into the in-flight utterance: text deltas for the
/// spoken run become clauses; its terminal event closes the clause queue.
///
/// Reading the run's text off the socket's own subscription is deliberate — the
/// response is already on this stream, so no second sink, no second copy of the
/// answer, and no coupling from the run engine to the voice channel.
pub(crate) fn feed_speech(
    speech: &mut Option<ActiveSpeech>,
    envelope: &EventEnvelope,
) -> Result<(), ()> {
    let Some(active) = speech.as_mut() else {
        return Ok(());
    };
    if envelope.payload["runId"].as_str() != Some(active.run_id.as_str()) {
        return Ok(());
    }
    match envelope.event_type.as_str() {
        // S3/ADR-033 §4. Handled **before** `text.delta` in this match for
        // readers, not for correctness — correctness comes from the transport:
        // the orchestrator emits this before the deltas derived from the
        // private content, one ordered broadcast carries both, and this loop
        // drains that stream in order. So the flag is already set by the time
        // the clause it governs is pushed to the synthesis task.
        //
        // `Release` pairs with the synthesis task's `Acquire` load.
        "run.speech_sensitive" => {
            active.sensitive.store(true, Ordering::Release);
        }
        "text.delta" => {
            let Some(text) = envelope.payload["text"].as_str() else {
                return Ok(());
            };
            let clauses = active.segmenter.push(text);
            let Some(sender) = active.clauses.as_ref() else {
                return Ok(());
            };
            for clause in clauses {
                // Never block the socket loop on the synthesizer: this task also
                // carries every other client event. A full queue means synthesis
                // has fallen hopelessly behind, which is a failure to report,
                // not a wait to absorb or a clause to silently drop.
                if sender.try_send(clause).is_err() {
                    return Err(());
                }
            }
        }
        // Terminal for the spoken response, either because the run finished or
        // because it parked in degraded mode: in both cases no further text is
        // coming, so what has been buffered is spoken and the queue closes
        // rather than the utterance hanging open until the socket dies.
        "run.completed" | "run.queued" | "degraded.queued" => {
            if let Some(sender) = active.clauses.as_ref()
                && let Some(tail) = active.segmenter.flush()
                && sender.try_send(tail).is_err()
            {
                return Err(());
            }
            // Closing the queue is what tells the synthesis task the response is
            // complete; it drains what is queued, then reports `Completed`.
            drop(active.clauses.take());
        }
        _ => {}
    }
    Ok(())
}

/// Send a `voice.speak.*` control frame. Server→client text frames are always
/// envelopes (docs/05 §3), so the `VoiceControlDto` tag rides the envelope
/// `type` exactly like a transient event's.
pub(crate) async fn send_speak_control(
    socket: &mut WebSocket,
    hub: &WsHub,
    control: &VoiceControlDto,
) -> Result<(), ()> {
    let (event_type, payload) =
        split_tagged(serde_json::to_value(control).expect("voice control serializes"));
    let envelope = EventEnvelope {
        v: CONTRACT_VERSION,
        seq: hub.high_water(),
        channel: Channel::Voice,
        event_type,
        occurred_at: now_rfc3339(),
        trace_id: None,
        resource_version: None,
        payload,
    };
    send_envelope(socket, &envelope).await
}
