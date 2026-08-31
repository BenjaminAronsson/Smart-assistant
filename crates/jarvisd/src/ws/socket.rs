use super::hub::*;
use super::replay::*;
use super::voice::*;
use std::sync::Arc;
use std::time::SystemTime;

use axum::Extension;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderValue;
use axum::response::Response;
use jarvis_contracts::CONTRACT_VERSION;
use jarvis_contracts::envelope::{Channel, EventEnvelope};
use jarvis_contracts::voice::{VoiceControlDto, VoiceErrorCodeDto};
use jarvis_domain::identity::ClassScope;
use jarvis_domain::ids::SessionId;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

/// Inbound WS frame/message ceiling. Voice PCM chunks are intentionally kept
/// below this bound; the browser emits 20–40 ms frames (docs/05 §1), far below
/// the 64 MiB tungstenite default (DoS hardening, security-auditor F1.5).
pub(crate) const MAX_INBOUND_FRAME_BYTES: usize = 64 * 1024;

/// `GET /ws/v1` — authenticated WebSocket upgrade (the bearer middleware has
/// already validated the device when this runs).
///
/// A browser's native `WebSocket` constructor cannot set an `Authorization`
/// header on the handshake request, so `require_device` accepts the device
/// token as a WS subprotocol instead, behind the `WS_DEVICE_TOKEN_PROTOCOL`
/// sentinel (`crate::auth::ws_subprotocol_token`): a browser opens the
/// socket with `new WebSocket(url, [WS_DEVICE_TOKEN_PROTOCOL, token])`. The
/// handshake only *completes* if the server selects one of the offered
/// subprotocols, so the sentinel — and only the sentinel, never the token —
/// is echoed back here to complete it. Echoing the token itself would put a
/// bearer secret in a response header no log/proxy redaction list expects
/// (unlike `Authorization`); the sentinel carries no authority on its own.
pub async fn ws_upgrade(
    State(state): State<WsState>,
    Query(params): Query<WsParams>,
    // The device this socket authenticated as (inserted by `require_device`,
    // which every `/ws/v1` upgrade passes through). Carried into the socket task
    // because a voice turn started here must acquire **exactly** the
    // authorization context a typed message from the same device would — a run
    // spawned without an attributable device is deliberately given no policy
    // context at all (CF-15 fail-closed, `runs::RunEngine::spawn`), and a voice
    // transcript must not be the one input that quietly lands in that state.
    Extension(device): Extension<crate::auth::DeviceContext>,
    ws: WebSocketUpgrade,
) -> Response {
    // Absent `since` = live-only from now; `since=0` = replay everything (outbox
    // ids start at 1 and the filter is `id > since`); a negative value clamps to
    // a full replay rather than being rejected.
    let since = params.since.map(|s| s.max(0));
    // `requested_protocols()` is a `BTreeSet` internally (sorted, not
    // offer-order), so this is a presence check only — the sentinel's
    // *position* (must be offered first) is enforced order-sensitively in
    // `auth::ws_subprotocol_token`, which reads the raw header directly
    // rather than through this extractor.
    let offered_token_protocol = ws
        .requested_protocols()
        .any(|p| p.as_bytes() == crate::auth::WS_DEVICE_TOKEN_PROTOCOL.as_bytes());
    // Run control remains REST-only, but voice control frames and bounded PCM
    // chunks are legitimate inbound messages (docs/05 §1).
    let mut ws = ws
        .max_message_size(MAX_INBOUND_FRAME_BYTES)
        .max_frame_size(MAX_INBOUND_FRAME_BYTES);
    if offered_token_protocol {
        ws.set_selected_protocol(HeaderValue::from_static(
            crate::auth::WS_DEVICE_TOKEN_PROTOCOL,
        ));
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state, since, device))
}

async fn handle_socket(
    mut socket: WebSocket,
    state: WsState,
    since: Option<i64>,
    device: crate::auth::DeviceContext,
) {
    // Subscribe BEFORE replaying so no live event slips through the gap. Any
    // overlap between replay and live is deduped by the client on `seq` (the
    // outbox id is unique and monotonic).
    let mut rx = state.hub.subscribe();
    // Subscribe, THEN verify. A `broadcast` receiver never sees values sent
    // before `subscribe()`, and this device was authorized earlier — in
    // `require_device`, before the upgrade completed. Subscribing first and
    // re-reading the device second leaves no window: a revocation before the
    // subscribe is caught by the read, one after it is caught by the bus.
    let mut revocations = state.revocations.subscribe();
    if let Some(identity) = &state.identity {
        match identity.is_device_active(&device.device_id).await {
            Ok(true) => {
                // Presence (F7.4): the device list distinguishes "paired" from
                // "actually here". Best-effort — a presence write must never
                // refuse a connection that is otherwise authorized.
                if let Err(e) = identity
                    .touch_last_seen(&device.device_id, SystemTime::now())
                    .await
                {
                    tracing::warn!(error = %e, "recording device presence failed");
                }
            }
            Ok(false) => {
                tracing::info!(
                    device_id = %device.device_id,
                    "closing socket at upgrade: device was revoked during the handshake"
                );
                let _ = socket
                    .send(Message::Close(Some(CloseFrame {
                        code: REVOKED_CLOSE_CODE,
                        reason: "device revoked".into(),
                    })))
                    .await;
                return;
            }
            // Fail closed: unable to confirm the device is still authorized.
            Err(e) => {
                tracing::error!(error = %e, "closing socket: revocation re-check failed");
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
        }
    }
    let mut voice_stream: Option<ActiveVoiceStream> = None;
    // Every capture stream THIS socket has opened. The currently-open stream is
    // not enough: a stream's **final** transcript settles after the stream is
    // torn down (that is what "final" means), so keying delivery on the live
    // stream would drop the socket's own last utterance — the one that starts
    // the run. Bounded, because it grows with client behaviour.
    let mut owned_streams: std::collections::VecDeque<OwnedId> = std::collections::VecDeque::new();
    // Presence lasts exactly as long as this task: the guard deregisters on
    // every exit path, including the ones that return early.
    let _presence = state.connected.mark_present(device.device_id.clone());
    // Re-assert this node's surface (F7.7). Sent through the hub, so F7.5's
    // targeting filter delivers it to exactly this device: what the node ends
    // up showing is current state, not a replayed backlog of commands.
    if let Some(placement) = state.surfaces.current(&device.device_id) {
        state
            .hub
            .broadcast_display(&placement, Some(device.device_id.as_str()));
    }
    let mut speech: Option<ActiveSpeech> = None;
    // Settled transcripts travel task → loop, because only the loop holds the
    // authenticated device identity a run must be attributed to.
    let (finals_tx, mut finals_rx) = mpsc::channel::<VoiceTurn>(4);

    if let Some(since) = since
        && replay_since(&mut socket, &state, since, &device)
            .await
            .is_err()
    {
        return; // client gone (or replay failed → it can REST-resync)
    }

    macro_rules! shut_down {
        () => {{
            stop_voice_stream(&mut voice_stream).await;
            let _ = cancel_speech(&mut socket, &state.hub, &mut speech).await;
            return;
        }};
    }

    loop {
        tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => {
                stop_voice_stream(&mut voice_stream).await;
                let _ = cancel_speech(&mut socket, &state.hub, &mut speech).await;
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            // The owner revoked a device. Polled second only to shutdown: a
            // socket that keeps streaming to a revoked device is the exact
            // failure `POST /devices/{id}/revoke` exists to prevent, and
            // "immediate" cannot mean "at the client's next reconnect".
            revoked = revocations.recv() => match revoked {
                Ok(id) if id == device.device_id => {
                    tracing::info!(device_id = %id, "closing socket: device revoked");
                    stop_voice_stream_with(&mut voice_stream, StreamStop::Immediately).await;
                    let _ = cancel_speech(&mut socket, &state.hub, &mut speech).await;
                    let _ = socket.send(Message::Close(Some(CloseFrame {
                        code: REVOKED_CLOSE_CODE,
                        reason: "device revoked".into(),
                    }))).await;
                    return;
                }
                // Someone else's device.
                Ok(_) => {}
                // We missed some announcements and cannot know whether ours
                // was among them, so we assume it was (fail closed). The
                // client reconnects; if it is still authorized, the reconnect
                // succeeds and nothing was lost but a round trip.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "revocation feed lagged — closing socket to re-authorize");
                    stop_voice_stream_with(&mut voice_stream, StreamStop::Immediately).await;
                    let _ = cancel_speech(&mut socket, &state.hub, &mut speech).await;
                    let _ = socket.send(Message::Close(Some(CloseFrame {
                        code: REVOKED_CLOSE_CODE,
                        reason: "re-authorize".into(),
                    }))).await;
                    return;
                }
                // No sender left anywhere in the process: nothing can ever
                // announce a revocation to this socket again. Unreachable
                // while this task holds `state` (which owns the bus, and
                // therefore a `Sender`) — but the invariant must not depend on
                // that ownership detail surviving a refactor, so close rather
                // than run on with revocation silently disabled. Also stops a
                // permanently-ready arm from starving the ones below it.
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::error!(
                        "revocation bus closed — closing socket rather than serving it \
                         with revocation disabled"
                    );
                    stop_voice_stream_with(&mut voice_stream, StreamStop::Immediately).await;
                    let _ = cancel_speech(&mut socket, &state.hub, &mut speech).await;
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
            },
            // A settled transcript. It becomes a run through `RunApi::start_turn`
            // — the same use case `POST /sessions/{id}/messages` calls — so it
            // gets M4's deterministic-grammar-first routing, the policy context
            // of *this* authenticated device, and no shortcut of any kind
            // (invariant #1). Awaited inline rather than spawned so the run is
            // durably created before the loop resumes: the deltas it will emit
            // are already buffered on `rx`, so binding speech to it here cannot
            // miss the start of the answer.
            //
            // Polled BEFORE the inbound and fan-out arms, deliberately.
            // `finals` is BOUNDED, and `biased` means an always-ready arm
            // starves the ones after it: with this arm last, a client that
            // pipelines frames — or a busy fan-out — kept the loop permanently
            // occupied and the queue permanently full, so the transcription
            // tasks feeding it had nowhere to put their results. Work the user
            // has already committed to (an utterance that is *finished*) drains
            // ahead of work that is only just arriving.
            Some(turn) = finals_rx.recv() => {
                if start_voice_turn(&mut owned_streams, &mut socket, &state, &device, &mut speech, turn).await.is_err() {
                    shut_down!();
                }
            }
            received = rx.recv() => match received {
                Ok(envelope) => {
                    // CF-8: what this device may see, decided per envelope.
                    if !delivers_to_owner_of(
                        &envelope,
                        device.class,
                        device.device_id.as_str(),
                        &owned_streams,
                    ) {
                        continue;
                    }
                    if send_envelope(&mut socket, &envelope).await.is_err() {
                        shut_down!();
                    }
                    // The spoken response is assembled from the run's own text
                    // deltas as they pass through this socket (F5.2).
                    if feed_speech(&mut speech, &envelope).is_err() {
                        tracing::warn!("speech clause queue overflowed; cancelling the utterance");
                        state.hub.broadcast_voice_error(
                            speech.as_ref().map(|s| s.utterance_id.as_str()).unwrap_or_default(),
                            VoiceErrorCodeDto::TtsFailed,
                        );
                        if cancel_speech(&mut socket, &state.hub, &mut speech).await.is_err() {
                            shut_down!();
                        }
                    }
                }
                // Too far behind: close so the client reconnects and resyncs
                // (persisted events are recovered via `?since=` / REST).
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    stop_voice_stream(&mut voice_stream).await;
                    let _ = cancel_speech(&mut socket, &state.hub, &mut speech).await;
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => shut_down!(),
            },
            // Inbound voice frames are the one exception to REST-only commands;
            // run control remains on the audited REST surface.
            //
            // Polled BEFORE the outbound speech arms, deliberately: `biased`
            // means an always-ready arm starves the ones after it, and a
            // faster-than-realtime synthesizer keeps the audio channel
            // permanently ready. With the order reversed, the very frame that
            // triggers barge-in could never be read while audio was flowing —
            // the one case this feature exists to handle.
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Close(_))) | None => shut_down!(),
                Some(Ok(Message::Text(text))) => {
                    let Ok(control) = serde_json::from_str::<VoiceControlDto>(&text) else {
                        continue;
                    };
                    match control {
                        VoiceControlDto::StreamStart {
                            stream_id,
                            session_id,
                            sample_rate_hz,
                            sample_width_bytes,
                            channels,
                        } => {
                            // Validated BEFORE the frame is allowed to do
                            // anything at all: a malformed `voice.stream.start`
                            // is not a barge-in, so it must not cancel the
                            // answer currently being spoken either.
                            //
                            // No `voice.error` is emitted for a rejected id: that
                            // event carries the very `streamId` under suspicion,
                            // and broadcasting it to every connected socket is
                            // the amplification this check exists to prevent. It
                            // is logged by length only, and the frame is dropped
                            // like any other unparseable one above — the browser
                            // already fails closed on the absence of a
                            // transcript.
                            // F7.6: capture is a *capability*, not something
                            // any authenticated socket may do. A display-only
                            // node opening a microphone stream is either
                            // misconfigured or hostile; either way the daemon
                            // must not start feeding a speech service on its
                            // behalf, and the attempt is worth keeping.
                            if !device.holds(ClassScope::VoiceCapture.as_str()) {
                                tracing::warn!(
                                    device_id = %device.device_id,
                                    class = %device.class,
                                    "refusing voice capture: device holds no `voice-capture` scope"
                                );
                                if let Some(audit) = &state.audit {
                                    let event = jarvis_domain::audit::AuditEvent {
                                        occurred_at: SystemTime::now(),
                                        actor: format!("device:{}", device.device_id),
                                        event_type: "voice.capture_denied".to_owned(),
                                        target: format!("device:{}", device.device_id),
                                        correlation_id: None,
                                        payload_json: serde_json::json!({
                                            "deviceClass": device.class.as_str(),
                                            "reason": "device holds no `voice-capture` scope",
                                        })
                                        .to_string(),
                                    };
                                    if let Err(e) = audit.record(&event).await {
                                        tracing::error!(error = %e, "capture-denial audit failed");
                                    }
                                }
                                // Sent on THIS socket rather than broadcast:
                                // the hub's voice channel is filtered by
                                // `voice-capture` (F7.4), so a broadcast
                                // refusal would be dropped before reaching the
                                // very device being refused. A per-connection
                                // rejection is not a household event anyway.
                                let refusal = EventEnvelope {
                                    v: CONTRACT_VERSION,
                                    seq: state.hub.high_water(),
                                    channel: Channel::Voice,
                                    event_type: "voice.error".to_owned(),
                                    occurred_at: now_rfc3339(),
                                    trace_id: None,
                                    resource_version: None,
                                    payload: serde_json::json!({
                                        "streamId": stream_id,
                                        "code": "voice.capture_denied",
                                    }),
                                };
                                if send_envelope(&mut socket, &refusal).await.is_err() {
                                    shut_down!();
                                }
                                continue;
                            }
                            if !stream_id_is_acceptable(&stream_id) {
                                tracing::warn!(
                                    stream_id_len = stream_id.len(),
                                    "rejected voice.stream.start: unacceptable streamId"
                                );
                                continue;
                            }
                            // This socket now owns the stream, so its events —
                            // including the final transcript that settles after
                            // teardown — reach it and nothing else (F7.4).
                            register_owned_stream(
                                &mut owned_streams,
                                OwnedId::Stream(stream_id.clone()),
                            );
                            // The per-stream format is client-controlled and is
                            // handed straight to the speech service; the
                            // `[voice].audio` config constrains only what the
                            // daemon itself is set up for, so it is checked here.
                            let Some(format) = accepted_audio_format(
                                sample_rate_hz,
                                sample_width_bytes,
                                channels,
                            ) else {
                                tracing::warn!(
                                    %stream_id,
                                    sample_rate_hz,
                                    sample_width_bytes,
                                    channels,
                                    "rejected voice.stream.start: unsupported capture format"
                                );
                                continue;
                            };
                            // BARGE-IN (docs/02 §9: TTS "stops immediately on
                            // barge-in"). The user speaking again supersedes the
                            // answer being spoken, so synthesis is cancelled
                            // here — before any audio of the new turn is even
                            // read — through the existing cancellation token
                            // (invariant #4), not a new mechanism.
                            if cancel_speech(&mut socket, &state.hub, &mut speech).await.is_err() {
                                shut_down!();
                            }
                            stop_voice_stream(&mut voice_stream).await;
                            // An unparseable session id confers nothing: the
                            // turn is transcribed and displayed, but no run is
                            // started against a session that does not exist.
                            let session_id = session_id.and_then(|id| id.parse::<SessionId>().ok());
                            if let Some(transcriber) = &state.transcriber {
                                let cancel = state.shutdown.child_token();
                                voice_stream = Some(start_voice_stream(
                                    Arc::clone(transcriber),
                                    Arc::clone(&state.hub),
                                    stream_id,
                                    session_id,
                                    format,
                                    cancel,
                                    finals_tx.clone(),
                                ));
                            }
                        }
                        VoiceControlDto::StreamStop { stream_id } => {
                            if voice_stream
                                .as_ref()
                                .is_some_and(|active| active.stream_id == stream_id)
                            {
                                stop_voice_stream(&mut voice_stream).await;
                            }
                        }
                        // Speak frames are daemon→client only; a client that
                        // sends one is ignored rather than obeyed — nothing on
                        // the inbound path may cause the daemon to speak.
                        VoiceControlDto::SpeakStart { .. } | VoiceControlDto::SpeakStop { .. } => {}
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    if let Some(tx) = voice_stream.as_mut().and_then(|active| active.audio_tx.as_ref()) {
                        let _ = tx.send(bytes.to_vec()).await;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => shut_down!(),
            },
            chunk = next_speech_chunk(&mut speech) => {
                if forward_speech_chunk(&mut socket, &state, &mut speech, chunk).await.is_err() {
                    shut_down!();
                }
            }
        }
    }
}

/// Turn a settled transcript into a run, then bind spoken output to it.
async fn start_voice_turn(
    owned_streams: &mut std::collections::VecDeque<OwnedId>,
    socket: &mut WebSocket,
    state: &WsState,
    device: &crate::auth::DeviceContext,
    speech: &mut Option<ActiveSpeech>,
    turn: VoiceTurn,
) -> Result<(), ()> {
    // This turn supersedes whatever was still being spoken, so the previous
    // utterance is stopped **through `cancel_speech`** before it can be
    // replaced. Dropping an `ActiveSpeech` neither cancels its token nor aborts
    // its task, so a bare overwrite left the old synthesis pulling PCM from the
    // speech service — holding that connection open — and left the client
    // without the `voice.speak.stop` its playback bookkeeping is waiting for.
    // Barge-in does not cover this: it fires at `voice.stream.start`, which is
    // strictly before the previous stream's final is dequeued here.
    cancel_speech(socket, &state.hub, speech).await?;

    let stream_id = turn.stream_id;
    let Some(runs) = state.runs.as_ref() else {
        return Ok(()); // no run surface mounted; transcript display only
    };
    let Some(session_id) = turn.session_id else {
        tracing::debug!(%stream_id, "voice transcript has no session; not starting a run");
        return Ok(());
    };

    let ack = match runs.start_turn(&session_id, device, turn.text).await {
        Ok(ack) => ack,
        Err(error) => {
            tracing::warn!(?error, %stream_id, "voice transcript could not start a run");
            return Ok(());
        }
    };

    // This socket started this run, so this socket is the one the answer
    // belongs to (F8.5). Recorded before synthesis is even considered: it is a
    // statement about who asked, not about who can speak, and `delivers_to_
    // owner_of` needs it to let the run's own text deltas past the Session
    // channel's `ui` rule — which a satellite never satisfies.
    register_owned_stream(owned_streams, OwnedId::Run(ack.run_id.as_str().to_owned()));

    if let Some(synthesizer) = state.synthesizer.as_ref() {
        // The utterance's token is a child of the socket's shutdown token, so
        // shutdown, socket loss and barge-in all reach it (invariant #4).
        *speech = Some(begin_speech(
            Arc::clone(synthesizer),
            ack.run_id,
            state.shutdown.child_token(),
        ));
        // Voice-pipeline errors are broadcast keyed by the *utterance* id in
        // the same `streamId` field a capture stream uses, so this socket must
        // own that id too or it would never hear that its own speech failed
        // (F7.4).
        if let Some(active) = speech.as_ref() {
            register_owned_stream(owned_streams, OwnedId::Stream(active.utterance_id.clone()));
        }
    }
    Ok(())
}

/// Relay one item from the synthesis task to the client.
async fn forward_speech_chunk(
    socket: &mut WebSocket,
    state: &WsState,
    speech: &mut Option<ActiveSpeech>,
    chunk: Option<SpeechChunk>,
) -> Result<(), ()> {
    let Some(active) = speech.as_mut() else {
        return Ok(());
    };
    match chunk {
        Some(SpeechChunk::Started(format)) => {
            active.announced = true;
            let control = VoiceControlDto::SpeakStart {
                utterance_id: active.utterance_id.clone(),
                run_id: Some(active.run_id.as_str().to_owned()),
                sample_rate_hz: format.sample_rate_hz,
                sample_width_bytes: format.sample_width_bytes,
                channels: format.channels,
            };
            send_speak_control(socket, &state.hub, &control).await
        }
        Some(SpeechChunk::Audio(bytes)) => {
            for frame in bytes.chunks(MAX_OUTBOUND_AUDIO_FRAME_BYTES) {
                socket
                    .send(Message::Binary(frame.to_vec().into()))
                    .await
                    .map_err(|_| ())?;
            }
            Ok(())
        }
        // Terminal for this utterance, either way: report it, then forget it.
        Some(SpeechChunk::Ended(reason, failure)) => {
            let utterance_id = active.utterance_id.clone();
            let announced = active.announced;
            *speech = None;
            if let Some(code) = failure {
                state.hub.broadcast_voice_error(&utterance_id, code);
            }
            if announced {
                let control = VoiceControlDto::SpeakStop {
                    utterance_id,
                    reason,
                };
                return send_speak_control(socket, &state.hub, &control).await;
            }
            Ok(())
        }
        // The task ended without a terminal chunk (it only does that when the
        // socket loop is gone); nothing left to speak.
        None => {
            *speech = None;
            Ok(())
        }
    }
}
