//! Wyoming protocol client (F5.1, ADR-007, docs/02 §9, docs/02 §12).
//!
//! Wyoming is a pipeline of independently-swappable out-of-process services
//! (ADR-007): VAD, STT, and TTS are three separate services, each exposed as
//! its own TCP endpoint on the private network (docs/02 §12). One
//! [`WyomingClient`] speaks to exactly one such endpoint and implements
//! whichever of the three `jarvis_application::voice` ports that endpoint
//! serves.
//!
//! Wire format (verified against the Wyoming protocol spec): each message is
//! a UTF-8 JSON header line (`type`, optional `data_length`,
//! `payload_length`), optionally followed by `data_length` bytes of
//! JSON-mergeable data, optionally followed by `payload_length` raw bytes
//! (audio PCM). Framing is implemented once in [`write_frame`]/[`read_frame`]
//! and reused by all three trait impls below.
//!
//! Error mapping: a failure to establish the TCP connection (refused,
//! unreachable, timed out) is [`VoiceError::Unavailable`]; anything that
//! breaks the wire protocol after that (truncated/oversized frame, invalid
//! JSON, a response missing a required field) is [`VoiceError::Malformed`].
//! Neither variant ever carries the raw `std::io::Error` text or the
//! configured address (invariant #5) — messages here are short, stable,
//! adapter-authored strings. Reducing them to the `health::REASON_CODES`
//! shape for the health tracker is F5.2's job, not this slice's.
//!
//! A failure *before* the stream exists (connect, and — for `synthesize`
//! specifically — the synchronous `audio-start` response the `Ok` return value
//! requires) is a returned `Err`. A failure *after* that arrives in-stream as
//! `VadEvent::Error` / `TranscriptEvent::Error` / an `Err` PCM chunk, mirroring
//! `ModelEvent::Error`. This distinction is load-bearing: a stream that simply
//! ends means the service finished normally, so a dropped connection must not
//! be reported the same way — otherwise a dead STT service is indistinguishable
//! from a user who said nothing. A clean EOF at a frame boundary (`read_frame`
//! → `Ok(None)`) is the normal end; a truncated one is an error.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::{BoxStream, StreamExt, poll_fn};
use jarvis_application::voice::{
    AudioFormat, SpeechSynthesizer, SpeechTranscriber, TranscriptEvent, VadEvent,
    VoiceActivityDetector, VoiceError,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Wall-clock bound on establishing the TCP connection to a Wyoming service.
/// An unreachable host must not hang the caller indefinitely (invariant #4);
/// mirrors `mcp_host::CONNECT_TIMEOUT`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on a single frame's header line. A well-formed Wyoming header is a few
/// hundred bytes; this bounds the memory a malfunctioning or hostile service
/// can force by emitting a long newline-less line (docs/06 §5 resource DoS —
/// mirrors `claude_cli::MAX_LINE_BYTES`).
const MAX_HEADER_LINE_BYTES: u64 = 64 * 1024;

/// Cap on a frame's `data` body (declared by `data_length`). The `data`
/// payloads this adapter sends/expects (audio format fields, transcript
/// text, synthesize text) are tiny; this bounds what a hostile/buggy service
/// can force it to buffer.
const MAX_DATA_BYTES: usize = 64 * 1024;

/// Cap on a frame's binary `payload` (declared by `payload_length`). One PCM
/// audio chunk is normally tens of KB; this gives headroom without letting a
/// hostile/buggy service force an unbounded allocation.
const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// Bound on items buffered between the socket task and the stream handed back
/// to the caller (and, for outbound audio, between the input stream and the
/// socket). An unbounded channel here is the same "denial of
/// wallet/resources" risk `MAX_RESULT_PROMPT_BYTES` (orchestrator) and
/// `MAX_AGENDA_EVENTS` (calendar.rs) guard elsewhere: a fast producer (mic
/// capture, a chatty VAD) must not be able to grow an unbounded backlog if
/// the consumer falls behind. 16 is generous for an event/chunk cadence
/// measured in items per second.
const CHANNEL_CAPACITY: usize = 16;

/// One configured Wyoming service endpoint. VAD, STT, and TTS are separate
/// services in this deployment (docs/02 §12) — construct one `WyomingClient`
/// per endpoint and implement only the trait(s) that service serves.
#[derive(Debug, Clone)]
pub struct WyomingClient {
    id: String,
    addr: String,
}

impl WyomingClient {
    /// `addr` is a `host:port` string passed directly to `TcpStream::connect`;
    /// the host's config loader resolves/validates it, this adapter does not.
    pub fn new(id: impl Into<String>, addr: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            addr: addr.into(),
        }
    }

    async fn connect(&self, cancel: &CancellationToken) -> Result<TcpStream, VoiceError> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(VoiceError::Cancelled),
            outcome = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&self.addr)) => {
                match outcome {
                    Ok(Ok(stream)) => Ok(stream),
                    // Connection refused, unreachable, or the timeout elapsed —
                    // all the same "service is not reachable" case to a caller;
                    // no raw io::Error text or address crosses this boundary.
                    Ok(Err(_)) | Err(_) => {
                        Err(VoiceError::Unavailable("connect failed".to_owned()))
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Wire framing
// ---------------------------------------------------------------------------

/// One decoded Wyoming message.
struct WireFrame {
    msg_type: String,
    data: Option<Value>,
    payload: Option<Vec<u8>>,
}

#[derive(Serialize)]
struct WireHeaderOut<'a> {
    #[serde(rename = "type")]
    msg_type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload_length: Option<usize>,
}

#[derive(Deserialize)]
struct WireHeaderIn {
    #[serde(rename = "type")]
    msg_type: String,
    data_length: Option<usize>,
    payload_length: Option<usize>,
}

/// Write one frame: header line, then `data_length` bytes of JSON data (if
/// any), then `payload_length` raw bytes (if any).
async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg_type: &str,
    data: Option<Value>,
    payload: Option<&[u8]>,
) -> Result<(), VoiceError> {
    let data_bytes = data
        .as_ref()
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|_| VoiceError::Malformed("could not encode frame data".to_owned()))?;
    let header = WireHeaderOut {
        msg_type,
        data_length: data_bytes.as_ref().map(Vec::len),
        payload_length: payload.map(<[u8]>::len),
    };
    let mut line = serde_json::to_vec(&header)
        .map_err(|_| VoiceError::Malformed("could not encode frame header".to_owned()))?;
    line.push(b'\n');
    writer
        .write_all(&line)
        .await
        .map_err(|_| VoiceError::Malformed("frame header write failed".to_owned()))?;
    if let Some(bytes) = &data_bytes {
        writer
            .write_all(bytes)
            .await
            .map_err(|_| VoiceError::Malformed("frame data write failed".to_owned()))?;
    }
    if let Some(bytes) = payload {
        writer
            .write_all(bytes)
            .await
            .map_err(|_| VoiceError::Malformed("frame payload write failed".to_owned()))?;
    }
    writer
        .flush()
        .await
        .map_err(|_| VoiceError::Malformed("frame flush failed".to_owned()))?;
    Ok(())
}

/// Read one frame. Any truncation, oversized declared length, or invalid JSON
/// maps to [`VoiceError::Malformed`] — never a panic, never an indefinite
/// hang (the header-line read is capped at [`MAX_HEADER_LINE_BYTES`] the same
/// way `claude_cli`'s stream-json reader is capped).
/// Reads one frame. `Ok(None)` is a **clean** end of stream — the peer closed
/// the connection exactly at a frame boundary, which is how a Wyoming service
/// normally signals it is done. That is deliberately distinct from `Err`, a
/// broken/truncated frame: callers surface the latter as an error event and the
/// former as a normal end of stream, so "the service finished" is never
/// confused with "the service died".
async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<WireFrame>, VoiceError> {
    let mut line = String::new();
    let n = {
        let mut capped = reader.take(MAX_HEADER_LINE_BYTES);
        capped
            .read_line(&mut line)
            .await
            .map_err(|_| VoiceError::Malformed("frame header read failed".to_owned()))?
    };
    if n == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') {
        // Either the cap was hit before a newline, or the connection closed
        // mid-line — both are a broken frame, not a well-formed one.
        return Err(VoiceError::Malformed(
            "frame header line too long or truncated".to_owned(),
        ));
    }
    let header: WireHeaderIn = serde_json::from_str(line.trim_end())
        .map_err(|_| VoiceError::Malformed("frame header is not valid JSON".to_owned()))?;

    let data =
        match header.data_length {
            Some(len) if len <= MAX_DATA_BYTES => {
                let bytes = read_exact_bytes(reader, len).await?;
                Some(serde_json::from_slice(&bytes).map_err(|_| {
                    VoiceError::Malformed("frame data is not valid JSON".to_owned())
                })?)
            }
            Some(_) => return Err(VoiceError::Malformed("frame data too large".to_owned())),
            None => None,
        };

    let payload = match header.payload_length {
        Some(len) if len <= MAX_PAYLOAD_BYTES => Some(read_exact_bytes(reader, len).await?),
        Some(_) => return Err(VoiceError::Malformed("frame payload too large".to_owned())),
        None => None,
    };

    Ok(Some(WireFrame {
        msg_type: header.msg_type,
        data,
        payload,
    }))
}

async fn read_exact_bytes<R: AsyncRead + Unpin>(
    reader: &mut R,
    len: usize,
) -> Result<Vec<u8>, VoiceError> {
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .await
        .map_err(|_| VoiceError::Malformed("frame body read failed".to_owned()))?;
    Ok(buf)
}

fn audio_format_data(format: AudioFormat) -> Value {
    serde_json::json!({
        "rate": format.sample_rate_hz,
        "width": format.sample_width_bytes,
        "channels": format.channels,
    })
}

fn audio_format_from_data(data: Option<&Value>) -> Result<AudioFormat, VoiceError> {
    let data =
        data.ok_or_else(|| VoiceError::Malformed("audio-start is missing data".to_owned()))?;
    let rate = data
        .get("rate")
        .and_then(Value::as_u64)
        .ok_or_else(|| VoiceError::Malformed("audio-start missing rate".to_owned()))?;
    let width = data
        .get("width")
        .and_then(Value::as_u64)
        .ok_or_else(|| VoiceError::Malformed("audio-start missing width".to_owned()))?;
    let channels = data
        .get("channels")
        .and_then(Value::as_u64)
        .ok_or_else(|| VoiceError::Malformed("audio-start missing channels".to_owned()))?;
    Ok(AudioFormat {
        sample_rate_hz: rate as u32,
        sample_width_bytes: width as u16,
        channels: channels as u16,
    })
}

/// Wrap a bounded mpsc receiver as a `BoxStream`, matching the
/// `poll_fn`-over-`poll_recv` idiom already used in this crate
/// (`media_mpris::tests::tokio_stream_from`) rather than adding a
/// `tokio-stream` dependency for one call site.
fn as_stream<T: Send + 'static>(mut rx: mpsc::Receiver<T>) -> BoxStream<'static, T> {
    Box::pin(poll_fn(move |cx| rx.poll_recv(cx)))
}

/// Send `audio-start`, then one `audio-chunk` per item from `audio`, then
/// `audio-stop` once the input stream ends. Shared by the VAD and STT
/// writers (both frame outbound audio identically).
async fn send_audio_stream<W: AsyncWrite + Unpin>(
    writer: &mut W,
    mut audio: BoxStream<'static, Vec<u8>>,
    format: AudioFormat,
    cancel: &CancellationToken,
) -> Result<(), VoiceError> {
    write_frame(writer, "audio-start", Some(audio_format_data(format)), None).await?;
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(VoiceError::Cancelled),
            item = audio.next() => item,
        };
        match next {
            Some(chunk) => {
                write_frame(
                    writer,
                    "audio-chunk",
                    Some(audio_format_data(format)),
                    Some(&chunk),
                )
                .await?;
            }
            None => break,
        }
    }
    write_frame(writer, "audio-stop", None, None).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// VoiceActivityDetector
// ---------------------------------------------------------------------------

#[async_trait]
impl VoiceActivityDetector for WyomingClient {
    fn id(&self) -> &str {
        &self.id
    }

    async fn detect(
        &self,
        audio: BoxStream<'static, Vec<u8>>,
        format: AudioFormat,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, VadEvent>, VoiceError> {
        let stream = self.connect(&cancel).await?;
        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut writer = write_half;

        let write_cancel = cancel.clone();
        tokio::spawn(async move {
            // Best-effort: a write failure or cancellation here just stops
            // sending; the read task below independently ends the output
            // stream on its own error/cancellation/EOF, so this can't hang
            // the caller even if the send side breaks silently.
            let _ = send_audio_stream(&mut writer, audio, format, &write_cancel).await;
        });

        let (tx, rx) = mpsc::channel::<VadEvent>(CHANNEL_CAPACITY);
        let read_cancel = cancel;
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    biased;
                    () = read_cancel.cancelled() => break,
                    frame = read_frame(&mut reader) => frame,
                };
                let event = match frame {
                    Ok(Some(f)) => match f.msg_type.as_str() {
                        "voice-started" => VadEvent::VoiceStarted,
                        "voice-stopped" => VadEvent::VoiceStopped,
                        _ => continue, // an unrecognized event type is ignored, not fatal
                    },
                    // Clean end of stream: the service finished and closed.
                    Ok(None) => break,
                    // A broken frame is reported, never silently swallowed.
                    Err(error) => {
                        let _ = tx.send(VadEvent::Error(error)).await;
                        break;
                    }
                };
                if tx.send(event).await.is_err() {
                    break; // the caller dropped the stream
                }
            }
        });

        Ok(as_stream(rx))
    }
}

// ---------------------------------------------------------------------------
// SpeechTranscriber
// ---------------------------------------------------------------------------

#[async_trait]
impl SpeechTranscriber for WyomingClient {
    fn id(&self) -> &str {
        &self.id
    }

    async fn transcribe(
        &self,
        audio: BoxStream<'static, Vec<u8>>,
        format: AudioFormat,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, TranscriptEvent>, VoiceError> {
        let stream = self.connect(&cancel).await?;
        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut writer = write_half;

        let write_cancel = cancel.clone();
        tokio::spawn(async move {
            let sent = tokio::select! {
                biased;
                () = write_cancel.cancelled() => Err(VoiceError::Cancelled),
                sent = write_frame(&mut writer, "transcribe", None, None) => sent,
            };
            if sent.is_err() {
                return;
            }
            let _ = send_audio_stream(&mut writer, audio, format, &write_cancel).await;
        });

        let (tx, rx) = mpsc::channel::<TranscriptEvent>(CHANNEL_CAPACITY);
        let read_cancel = cancel;
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    biased;
                    () = read_cancel.cancelled() => break,
                    frame = read_frame(&mut reader) => frame,
                };
                let frame = match frame {
                    Ok(Some(f)) => f,
                    // Clean end of stream: the service finished and closed.
                    Ok(None) => break,
                    // A broken frame is reported, never silently swallowed —
                    // otherwise a dead STT service reaches the orchestrator as
                    // an empty transcript stream, i.e. as "the user said
                    // nothing".
                    Err(error) => {
                        let _ = tx.send(TranscriptEvent::Error(error)).await;
                        break;
                    }
                };
                match frame.msg_type.as_str() {
                    "transcript" => {
                        // The base Wyoming `transcript` type does not itself
                        // distinguish partial vs. final results, and this
                        // adapter has no evidence a specific service sends
                        // more than one per request — every `transcript`
                        // frame is treated as `Final`. `Partial` is left
                        // unconstructed here rather than guessed at.
                        let text = frame
                            .data
                            .as_ref()
                            .and_then(|d| d.get("text"))
                            .and_then(Value::as_str);
                        let Some(text) = text else {
                            let _ = tx
                                .send(TranscriptEvent::Error(VoiceError::Malformed(
                                    "transcript frame has no text".to_owned(),
                                )))
                                .await;
                            break;
                        };
                        if tx
                            .send(TranscriptEvent::Final(text.to_owned()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    _ => continue, // ignore unrelated event types
                }
            }
        });

        Ok(as_stream(rx))
    }
}

// ---------------------------------------------------------------------------
// SpeechSynthesizer
// ---------------------------------------------------------------------------

#[async_trait]
impl SpeechSynthesizer for WyomingClient {
    fn id(&self) -> &str {
        &self.id
    }

    async fn synthesize(
        &self,
        text: &str,
        cancel: CancellationToken,
    ) -> Result<(AudioFormat, BoxStream<'static, Result<Vec<u8>, VoiceError>>), VoiceError> {
        let stream = self.connect(&cancel).await?;
        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut writer = write_half;

        let request = serde_json::json!({ "text": text });
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(VoiceError::Cancelled),
            sent = write_frame(&mut writer, "synthesize", Some(request), None) => sent?,
        };

        // The `Ok` shape requires the format up front, so the first response
        // frame is read synchronously here — a malformed or missing
        // `audio-start` is exactly the "connected fine, but the service is
        // broken" case that must surface as an `Err`, not an empty stream.
        let start = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(VoiceError::Cancelled),
            frame = read_frame(&mut reader) => frame?,
        };
        let Some(start) = start else {
            return Err(VoiceError::Malformed(
                "connection closed before audio-start".to_owned(),
            ));
        };
        if start.msg_type != "audio-start" {
            return Err(VoiceError::Malformed(
                "expected audio-start as the first synthesize response frame".to_owned(),
            ));
        }
        let format = audio_format_from_data(start.data.as_ref())?;

        let (tx, rx) = mpsc::channel::<Result<Vec<u8>, VoiceError>>(CHANNEL_CAPACITY);
        let read_cancel = cancel;
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    biased;
                    () = read_cancel.cancelled() => break,
                    frame = read_frame(&mut reader) => frame,
                };
                let frame = match frame {
                    Ok(Some(f)) => f,
                    // The peer closed without an explicit `audio-stop`. The
                    // utterance is truncated, so this is reported rather than
                    // passed off as a complete one.
                    Ok(None) => {
                        let _ = tx
                            .send(Err(VoiceError::Malformed(
                                "connection closed before audio-stop".to_owned(),
                            )))
                            .await;
                        break;
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error)).await;
                        break;
                    }
                };
                match frame.msg_type.as_str() {
                    "audio-chunk" => {
                        let Some(chunk) = frame.payload else {
                            let _ = tx
                                .send(Err(VoiceError::Malformed(
                                    "audio-chunk frame has no payload".to_owned(),
                                )))
                                .await;
                            break;
                        };
                        if tx.send(Ok(chunk)).await.is_err() {
                            break;
                        }
                    }
                    "audio-stop" => break, // the utterance completed normally
                    _ => continue,
                }
            }
        });

        Ok((format, as_stream(rx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::WriteHalf;
    use tokio::net::TcpListener;

    /// Bind an ephemeral port, accept exactly one connection, and hand it to
    /// `handler`. Returns the `host:port` string a [`WyomingClient`] connects
    /// to — the fixture side of every test below speaks just enough of the
    /// Wyoming protocol (via the same [`write_frame`]/[`read_frame`] the
    /// client uses) to drive one exchange.
    async fn spawn_fixture<F, Fut>(handler: F) -> String
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture listener");
        let addr = listener
            .local_addr()
            .expect("fixture local_addr")
            .to_string();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("fixture accept");
            handler(stream).await;
        });
        addr
    }

    fn test_format() -> AudioFormat {
        AudioFormat {
            sample_rate_hz: 16_000,
            sample_width_bytes: 2,
            channels: 1,
        }
    }

    /// Bound every test with a hard wall-clock ceiling: a regression that
    /// makes the client hang must fail the test, not wedge the suite.
    async fn bounded<F: std::future::Future>(fut: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(5), fut)
            .await
            .expect("test exceeded its bound — the client hung")
    }

    /// Fixture-side frame read: fails the test on a protocol error *or* on an
    /// unexpected clean EOF, which `read_frame` now reports as `Ok(None)`.
    async fn expect_frame<R: AsyncBufRead + Unpin>(reader: &mut R, what: &str) -> WireFrame {
        read_frame(reader)
            .await
            .unwrap_or_else(|error| panic!("{what}: {error}"))
            .unwrap_or_else(|| panic!("{what}: unexpected end of stream"))
    }

    #[tokio::test]
    async fn vad_reports_voice_started_then_stopped_in_order() {
        let addr = spawn_fixture(|stream| async move {
            let (read_half, write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);
            let mut writer = write_half;

            let start = expect_frame(&mut reader, "audio-start").await;
            assert_eq!(start.msg_type, "audio-start");
            let chunk = expect_frame(&mut reader, "audio-chunk").await;
            assert_eq!(chunk.msg_type, "audio-chunk");
            assert_eq!(chunk.payload.as_deref(), Some(&b"abc"[..]));
            let stop = expect_frame(&mut reader, "audio-stop").await;
            assert_eq!(stop.msg_type, "audio-stop");

            write_frame(&mut writer, "voice-started", None, None)
                .await
                .expect("write voice-started");
            write_frame(&mut writer, "voice-stopped", None, None)
                .await
                .expect("write voice-stopped");
        })
        .await;

        bounded(async {
            let client = WyomingClient::new("vad-test", addr);
            let audio: BoxStream<'static, Vec<u8>> =
                futures_util::stream::iter(vec![b"abc".to_vec()]).boxed();
            let mut events = client
                .detect(audio, test_format(), CancellationToken::new())
                .await
                .expect("detect must succeed");

            let mut collected = Vec::new();
            while let Some(event) = events.next().await {
                collected.push(event);
            }
            assert_eq!(
                collected,
                vec![VadEvent::VoiceStarted, VadEvent::VoiceStopped]
            );
        })
        .await;
    }

    #[tokio::test]
    async fn stt_returns_a_final_transcript() {
        let addr = spawn_fixture(|stream| async move {
            let (read_half, write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);
            let mut writer = write_half;

            let req = expect_frame(&mut reader, "transcribe").await;
            assert_eq!(req.msg_type, "transcribe");
            let start = expect_frame(&mut reader, "audio-start").await;
            assert_eq!(start.msg_type, "audio-start");
            let chunk = expect_frame(&mut reader, "audio-chunk").await;
            assert_eq!(chunk.payload.as_deref(), Some(&b"xyz"[..]));
            let stop = expect_frame(&mut reader, "audio-stop").await;
            assert_eq!(stop.msg_type, "audio-stop");

            write_frame(
                &mut writer,
                "transcript",
                Some(serde_json::json!({ "text": "hello world" })),
                None,
            )
            .await
            .expect("write transcript");
        })
        .await;

        bounded(async {
            let client = WyomingClient::new("stt-test", addr);
            let audio: BoxStream<'static, Vec<u8>> =
                futures_util::stream::iter(vec![b"xyz".to_vec()]).boxed();
            let mut events = client
                .transcribe(audio, test_format(), CancellationToken::new())
                .await
                .expect("transcribe must succeed");

            let first = events.next().await.expect("one transcript event");
            assert_eq!(first, TranscriptEvent::Final("hello world".to_owned()));
        })
        .await;
    }

    /// The distinction this test pins down is the whole reason
    /// `TranscriptEvent::Error` exists: a service that dies mid-turn must not
    /// be indistinguishable from a user who said nothing. Without the error
    /// event, both reach the orchestrator as an empty stream.
    #[tokio::test]
    async fn a_broken_stt_service_reports_an_error_not_an_empty_transcript() {
        let addr = spawn_fixture(|stream| async move {
            let (read_half, write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);
            let mut writer = write_half;

            let _ = expect_frame(&mut reader, "transcribe").await;
            // Garbage instead of a frame header: the service is broken.
            writer
                .write_all(b"this is not a frame header\n")
                .await
                .expect("write garbage");
            writer.flush().await.expect("flush");
        })
        .await;

        bounded(async {
            let client = WyomingClient::new("stt-broken", addr);
            let audio: BoxStream<'static, Vec<u8>> =
                futures_util::stream::iter(vec![b"xyz".to_vec()]).boxed();
            let mut events = client
                .transcribe(audio, test_format(), CancellationToken::new())
                .await
                .expect("transcribe opens fine; the failure is mid-stream");

            let event = events
                .next()
                .await
                .expect("a broken service must produce an event, not an empty stream");
            assert!(
                matches!(event, TranscriptEvent::Error(VoiceError::Malformed(_))),
                "expected a malformed-protocol error event, got {event:?}"
            );
        })
        .await;
    }

    /// A service that closes cleanly after its last event has *finished*, not
    /// failed — the counterpart to the test above. Conflating the two would
    /// make every healthy turn end in a spurious error.
    #[tokio::test]
    async fn a_clean_close_after_the_transcript_is_not_an_error() {
        let addr = spawn_fixture(|stream| async move {
            let (read_half, write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);
            let mut writer = write_half;

            let _ = expect_frame(&mut reader, "transcribe").await;
            write_frame(
                &mut writer,
                "transcript",
                Some(serde_json::json!({ "text": "all done" })),
                None,
            )
            .await
            .expect("write transcript");
            // Drop the connection: a normal, complete exchange.
            drop(writer);
        })
        .await;

        bounded(async {
            let client = WyomingClient::new("stt-clean", addr);
            let audio: BoxStream<'static, Vec<u8>> =
                futures_util::stream::iter(vec![b"xyz".to_vec()]).boxed();
            let mut events = client
                .transcribe(audio, test_format(), CancellationToken::new())
                .await
                .expect("transcribe must succeed");

            assert_eq!(
                events.next().await,
                Some(TranscriptEvent::Final("all done".to_owned()))
            );
            assert_eq!(
                events.next().await,
                None,
                "a clean close is the end of the stream, never an error event"
            );
        })
        .await;
    }

    /// A truncated utterance must be reported, not passed off as a complete
    /// one — a caller that believed it had the whole utterance would play a
    /// silently clipped response.
    #[tokio::test]
    async fn tts_reports_an_error_when_the_audio_is_cut_short() {
        let addr = spawn_fixture(|stream| async move {
            let (read_half, write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);
            let mut writer = write_half;

            let _ = expect_frame(&mut reader, "synthesize").await;
            write_frame(
                &mut writer,
                "audio-start",
                Some(serde_json::json!({ "rate": 22_050, "width": 2, "channels": 1 })),
                None,
            )
            .await
            .expect("write audio-start");
            write_frame(&mut writer, "audio-chunk", None, Some(b"AAAA"))
                .await
                .expect("write chunk");
            // Close without `audio-stop`: the utterance is incomplete.
            drop(writer);
        })
        .await;

        bounded(async {
            let client = WyomingClient::new("tts-truncated", addr);
            let (_format, mut chunks) = client
                .synthesize("hello there", CancellationToken::new())
                .await
                .expect("synthesize opens fine; the failure is mid-stream");

            assert_eq!(chunks.next().await, Some(Ok(b"AAAA".to_vec())));
            let last = chunks
                .next()
                .await
                .expect("a truncated utterance must surface, not just end");
            assert!(
                matches!(last, Err(VoiceError::Malformed(_))),
                "expected a malformed-protocol error chunk, got {last:?}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn tts_reassembles_pcm_chunks_and_reports_the_format() {
        let addr = spawn_fixture(|stream| async move {
            let (read_half, write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);
            let mut writer = write_half;

            let req = expect_frame(&mut reader, "synthesize").await;
            assert_eq!(req.msg_type, "synthesize");
            assert_eq!(
                req.data
                    .as_ref()
                    .and_then(|d| d.get("text"))
                    .and_then(Value::as_str),
                Some("hello there")
            );

            write_frame(
                &mut writer,
                "audio-start",
                Some(serde_json::json!({ "rate": 22_050, "width": 2, "channels": 1 })),
                None,
            )
            .await
            .expect("write audio-start");
            write_frame(&mut writer, "audio-chunk", None, Some(b"AAAA"))
                .await
                .expect("write chunk 1");
            write_frame(&mut writer, "audio-chunk", None, Some(b"BBB"))
                .await
                .expect("write chunk 2");
            write_frame(&mut writer, "audio-stop", None, None)
                .await
                .expect("write audio-stop");
        })
        .await;

        bounded(async {
            let client = WyomingClient::new("tts-test", addr);
            let (format, mut chunks) = client
                .synthesize("hello there", CancellationToken::new())
                .await
                .expect("synthesize must succeed");

            assert_eq!(
                format,
                AudioFormat {
                    sample_rate_hz: 22_050,
                    sample_width_bytes: 2,
                    channels: 1,
                }
            );

            let mut pcm = Vec::new();
            while let Some(chunk) = chunks.next().await {
                pcm.extend_from_slice(&chunk.expect("no chunk should be an error"));
            }
            assert_eq!(pcm, b"AAAABBB".to_vec());
        })
        .await;
    }

    #[tokio::test]
    async fn cancellation_ends_the_stream_promptly() {
        let addr = spawn_fixture(|stream| async move {
            let (read_half, write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);
            // Held for the fixture's lifetime so the connection stays open
            // without ever sending a response — only the client's own
            // cancellation may end the caller's stream.
            let _write_half: WriteHalf<TcpStream> = write_half;
            let _ = read_frame(&mut reader).await; // audio-start
            std::future::pending::<()>().await;
        })
        .await;

        bounded(async {
            let client = WyomingClient::new("cancel-test", addr);
            let cancel = CancellationToken::new();
            // Never yields, so only cancellation can end the writer side too.
            let audio: BoxStream<'static, Vec<u8>> = futures_util::stream::pending().boxed();
            let mut events = client
                .detect(audio, test_format(), cancel.clone())
                .await
                .expect("detect must succeed");

            // Let the background read/write tasks reach their blocking state.
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();

            let next = tokio::time::timeout(Duration::from_secs(2), events.next())
                .await
                .expect("cancellation must end the stream promptly, not hang");
            assert_eq!(next, None, "a cancelled stream must end, not keep waiting");
        })
        .await;
    }

    #[tokio::test]
    async fn a_refused_connection_returns_unavailable_not_a_hang() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a throwaway listener");
        let addr = listener.local_addr().expect("local_addr").to_string();
        drop(listener); // free the port; nothing is listening on it anymore

        bounded(async {
            let client = WyomingClient::new("refused-test", addr);
            // `.map(|_| ())` discards the (non-`Debug`) audio stream so the
            // assertion failure message below can print the outcome.
            let result = client
                .synthesize("hi", CancellationToken::new())
                .await
                .map(|_| ());
            assert!(
                matches!(result, Err(VoiceError::Unavailable(_))),
                "expected Unavailable, got {result:?}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_non_json_header_line_is_malformed_not_a_panic() {
        let addr = spawn_fixture(|stream| async move {
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);
            let _ = read_frame(&mut reader).await; // synthesize request
            write_half
                .write_all(b"not json at all\n")
                .await
                .expect("write garbage line");
        })
        .await;

        bounded(async {
            let client = WyomingClient::new("malformed-json-test", addr);
            let result = client
                .synthesize("hi", CancellationToken::new())
                .await
                .map(|_| ());
            assert!(
                matches!(result, Err(VoiceError::Malformed(_))),
                "expected Malformed, got {result:?}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_header_missing_type_is_malformed_not_a_panic() {
        let addr = spawn_fixture(|stream| async move {
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);
            let _ = read_frame(&mut reader).await; // synthesize request
            write_half
                .write_all(b"{}\n")
                .await
                .expect("write header without type");
        })
        .await;

        bounded(async {
            let client = WyomingClient::new("malformed-type-test", addr);
            let result = client
                .synthesize("hi", CancellationToken::new())
                .await
                .map(|_| ());
            assert!(
                matches!(result, Err(VoiceError::Malformed(_))),
                "expected Malformed, got {result:?}"
            );
        })
        .await;
    }
}
