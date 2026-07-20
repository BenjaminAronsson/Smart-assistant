//! Claude CLI adapter (F1.6, docs/02 §3, ADR-011): spawn `claude` binary,
//! stream-JSON parsing, health detection, cancellation with process cleanup.

use async_trait::async_trait;
use futures_util::stream::{BoxStream, StreamExt, unfold};
use jarvis_application::model::{
    FinishReason, ModelError, ModelEvent, ModelProvider, ModelRequest, ProfileId,
};
use serde::Deserialize;
use serde_json::json;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Stream events from claude CLI (docs/05 §4, streamed as newline-delimited JSON).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClaudeEvent {
    #[serde(rename_all = "camelCase")]
    ContentBlockDelta {
        #[serde(default)]
        #[allow(dead_code)]
        index: i64,
        delta: Delta,
    },
    #[serde(rename_all = "camelCase")]
    MessageStop { message: Message },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Delta {
    #[serde(rename_all = "camelCase")]
    TextDelta { text: String },
    #[serde(rename_all = "camelCase")]
    InputJsonDelta {
        #[allow(dead_code)]
        partial_json: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    #[serde(default)]
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    #[allow(dead_code)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    #[allow(dead_code)]
    model: String,
    stop_reason: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        #[allow(dead_code)]
        text: String,
    },
}

/// The idle timeout for reading from claude CLI output. If no event arrives
/// within this window, the run is cancelled and marked unhealthy (NFR-03).
const PROVIDER_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Per-line read cap for the provider's stream-json output. A well-formed event
/// is a few KiB; this bounds the memory a malfunctioning or hostile provider can
/// force by emitting a very long newline-less line (resource DoS, docs/06 §5). A
/// line that hits the cap is parsed as truncated JSON → `Malformed` → the child
/// is killed, same as any other garbage.
const MAX_LINE_BYTES: u64 = 1 << 20; // 1 MiB

/// Claude CLI adapter: spawns the binary, reads streaming JSON, handles cancellation.
pub struct ClaudeCliModel {
    /// Profile ID for health monitoring + error classification.
    profile: ProfileId,
}

impl ClaudeCliModel {
    pub fn new(profile: impl Into<String>) -> Self {
        Self {
            profile: ProfileId::new(profile),
        }
    }
}

#[async_trait]
impl ModelProvider for ClaudeCliModel {
    fn id(&self) -> ProfileId {
        self.profile.clone()
    }

    async fn run(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ModelEvent>, ModelError> {
        // Spawn the claude CLI process with streaming output.
        let mut child = tokio::process::Command::new("claude")
            .arg("api")
            .arg("messages")
            .arg("stream")
            .arg("--no-limit")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                // Classify spawn failures: network error on command lookup
                ModelError::Unavailable(format!("network_error: failed to spawn claude: {}", e))
            })?;

        let mut stdin = child.stdin.take().ok_or_else(|| {
            ModelError::Unavailable("network_error: failed to capture stdin".to_owned())
        })?;

        // Send the request as JSON over stdin.
        let request_json = json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 2048,
            "stream": true,
            "messages": [{"role": "user", "content": request.prompt}]
        });

        let json_str = format!("{}\n", request_json);
        if let Err(e) = stdin.write_all(json_str.as_bytes()).await {
            return Err(ModelError::Unavailable(format!(
                "network_error: failed to write request: {}",
                e
            )));
        }
        if let Err(e) = stdin.shutdown().await {
            return Err(ModelError::Unavailable(format!(
                "network_error: failed to close stdin: {}",
                e
            )));
        }

        let stdout = child.stdout.take().ok_or_else(|| {
            ModelError::Unavailable("network_error: failed to capture stdout".to_owned())
        })?;

        // Read streaming events from stdout using unfold to create a boxed stream.
        let stream = unfold(ReadState::new(child, stdout, cancel), |state| async move {
            state.next().await
        })
        .boxed();

        Ok(stream)
    }
}

/// State machine for reading from the subprocess, held across stream iterations.
struct ReadState {
    child: Child,
    reader: BufReader<tokio::process::ChildStdout>,
    cancel: CancellationToken,
    finished: bool,
}

impl ReadState {
    fn new(child: Child, stdout: tokio::process::ChildStdout, cancel: CancellationToken) -> Self {
        Self {
            child,
            reader: BufReader::new(stdout),
            cancel,
            finished: false,
        }
    }

    /// Read the next event from the stream, returning (event, updated_state).
    /// Returns None when the stream is exhausted.
    async fn next(mut self) -> Option<(ModelEvent, Self)> {
        loop {
            if self.finished {
                return None;
            }

            // Exit on cancellation
            if self.cancel.is_cancelled() {
                let _ = self.child.kill().await;
                self.finished = true;
                return Some((
                    ModelEvent::Error(ModelError::Unavailable("cancelled".to_owned())),
                    self,
                ));
            }

            let mut line = String::new();
            // Cap each line read so one unbounded line can't exhaust memory
            // within the idle window (NIT 3). `Take` bounds this single read; a
            // fresh cap applies each iteration. Scoped so the `&mut self.reader`
            // borrow ends before the match arms move `self`.
            let deadline = {
                let mut capped = (&mut self.reader).take(MAX_LINE_BYTES);
                tokio::time::timeout(PROVIDER_IDLE_TIMEOUT, capped.read_line(&mut line)).await
            };

            match deadline {
                Ok(Ok(0)) => {
                    // EOF: process finished normally
                    self.finished = true;
                    #[allow(clippy::collapsible_if)]
                    if let Ok(status) = self.child.wait().await {
                        if !status.success() {
                            return Some((
                                ModelEvent::Error(ModelError::Malformed(
                                    "claude CLI exited with non-zero status".to_owned(),
                                )),
                                self,
                            ));
                        }
                    }
                    return None;
                }
                Ok(Ok(_)) => {
                    line = line.trim().to_owned();
                    if line.is_empty() {
                        // Skip blank lines and continue
                        continue;
                    }

                    match serde_json::from_str::<ClaudeEvent>(&line) {
                        Ok(ClaudeEvent::ContentBlockDelta { delta, .. }) => {
                            match delta {
                                Delta::TextDelta { text } => {
                                    return Some((ModelEvent::TextDelta(text), self));
                                }
                                Delta::InputJsonDelta { partial_json: _ } => {
                                    // F1.6 does not support tools; skip tool input
                                    continue;
                                }
                            }
                        }
                        Ok(ClaudeEvent::MessageStop { message }) => {
                            let finish_reason = match message.stop_reason.as_str() {
                                "end_turn" => FinishReason::Stop,
                                "max_tokens" => FinishReason::Length,
                                _ => FinishReason::Stop,
                            };
                            let _ = self.child.kill().await;
                            self.finished = true;
                            return Some((ModelEvent::Done(finish_reason), self));
                        }
                        Ok(ClaudeEvent::Unknown) => {
                            // Skip unknown event types and continue
                            continue;
                        }
                        Err(e) => {
                            warn!(error = %e, "failed to parse claude event");
                            let _ = self.child.kill().await;
                            self.finished = true;
                            return Some((
                                ModelEvent::Error(ModelError::Malformed(
                                    "failed to parse event".to_owned(),
                                )),
                                self,
                            ));
                        }
                    }
                }
                Ok(Err(e)) => {
                    let _ = self.child.kill().await;
                    self.finished = true;
                    return Some((
                        ModelEvent::Error(ModelError::Malformed(format!("read error: {}", e))),
                        self,
                    ));
                }
                Err(_) => {
                    // Timeout: process is stuck
                    let _ = self.child.kill().await;
                    self.finished = true;
                    return Some((
                        ModelEvent::Error(ModelError::Unavailable(
                            "timeout: provider idle timeout".to_owned(),
                        )),
                        self,
                    ));
                }
            }
        }
    }
}
