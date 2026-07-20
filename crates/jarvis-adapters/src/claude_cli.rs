//! Claude CLI adapter (F1.6, docs/02 §3, docs/03 §4, ADR-004, ADR-011).
//!
//! Invokes the Claude Code CLI in the transitional shape ADR-004 mandates:
//! `claude -p --output-format stream-json` in a **controlled workdir**, with the
//! reasoning profile's **built-in tools disabled** (Jarvis tools are the only
//! action path — invariant 1, ADR-014). The prompt crosses on **stdin, never in
//! argv** (process listings leak — invariant 5, provider-adapter skill). No model
//! string is chosen here: the CLI uses the subscription default, so this adapter
//! never couples the domain to a specific model id or the raw Messages API.
//!
//! `--output-format stream-json --include-partial-messages` yields newline-
//! delimited JSON where each line is a CLI envelope object (`system` / `stream_event`
//! / `assistant` / `result`). The token-level deltas Jarvis streams to the UI arrive
//! wrapped in `stream_event`; `result` is the authoritative terminal. Parsing is
//! developed against fixtures in `tests/fixtures/claude-cli/` (see the test module).

use async_trait::async_trait;
use futures_util::stream::{BoxStream, StreamExt, unfold};
use jarvis_application::model::{
    FinishReason, ModelError, ModelEvent, ModelProvider, ModelRequest, ProfileId,
};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Per-line read cap for the provider's stream-json output. A well-formed event
/// is a few KiB; this bounds the memory a malfunctioning or hostile provider can
/// force by emitting a very long newline-less line (resource DoS, docs/06 §5). A
/// line that hits the cap is parsed as truncated JSON → `Malformed` → the child
/// is killed, same as any other garbage.
const MAX_LINE_BYTES: u64 = 1 << 20; // 1 MiB

/// Adapter configuration (docs/09 §1 `[providers.claude-cli]`, ADR-004). The host
/// builds this from `jarvisd.toml`; the adapter never reads config files itself.
#[derive(Debug, Clone)]
pub struct ClaudeCliConfig {
    /// The CLI binary, resolved on the PATH of the service user (docs/09 §1).
    pub binary: String,
    /// Controlled working directory the process is spawned in (ADR-004). Created
    /// idempotently at spawn so a fresh host doesn't fail; ops provisions it in
    /// production.
    pub workdir: PathBuf,
    /// Reasoning profile disables the CLI's built-in file/shell/web tools —
    /// Jarvis tools are the only action path (invariant 1, ADR-004/014).
    pub disable_builtin_tools: bool,
    /// Idle read timeout: no event within this window ⇒ the run is cancelled and
    /// the provider marked unhealthy (NFR-03).
    pub idle_timeout: Duration,
}

impl Default for ClaudeCliConfig {
    fn default() -> Self {
        // Mirrors the documented `[providers.claude-cli]` defaults (docs/09 §1).
        Self {
            binary: "claude".to_owned(),
            workdir: PathBuf::from("/var/lib/jarvis/claude-work"),
            disable_builtin_tools: true,
            idle_timeout: Duration::from_secs(60),
        }
    }
}

/// Claude CLI adapter: spawns the binary, reads streaming JSON, handles cancellation.
pub struct ClaudeCliModel {
    /// Profile ID for health monitoring + error classification.
    profile: ProfileId,
    config: ClaudeCliConfig,
}

impl ClaudeCliModel {
    /// Construct with the documented default configuration (docs/09 §1).
    pub fn new(profile: impl Into<String>) -> Self {
        Self::with_config(profile, ClaudeCliConfig::default())
    }

    /// Construct with host-supplied configuration (`[providers.claude-cli]`).
    pub fn with_config(profile: impl Into<String>, config: ClaudeCliConfig) -> Self {
        Self {
            profile: ProfileId::new(profile),
            config,
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
        // ADR-004: spawn in a controlled workdir. Create it idempotently so a
        // fresh dev/test host doesn't fail to spawn; ops owns it in production.
        if let Err(e) = tokio::fs::create_dir_all(&self.config.workdir).await {
            return Err(ModelError::Unavailable(format!(
                "network_error: claude workdir unavailable: {e}"
            )));
        }

        // ADR-004 invocation: `claude -p --output-format stream-json` in the
        // controlled workdir. `--verbose` is required by the CLI to stream JSON
        // under `-p`; `--include-partial-messages` yields token-level deltas.
        let mut command = tokio::process::Command::new(&self.config.binary);
        command
            .arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--include-partial-messages")
            .current_dir(&self.config.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if self.config.disable_builtin_tools {
            // Reasoning profile: permit no built-in tools. An explicit empty
            // allowlist grants none — Jarvis tools become the sole action path
            // when the tool registry lands in M2 (ADR-004). Confirm this flag's
            // semantics against the installed CLI version when locking fixtures.
            command.arg("--allowedTools").arg("");
        }

        let mut child = command.spawn().map_err(|e| {
            // Spawn failure (binary missing on PATH, workdir gone) is a network-
            // class unavailability the router queues behind (health::reason_code).
            ModelError::Unavailable(format!("network_error: failed to spawn claude: {e}"))
        })?;

        // Prompt crosses on stdin, NEVER in argv (invariant 5). Closing stdin
        // signals end-of-prompt so `-p` proceeds.
        let mut stdin = child.stdin.take().ok_or_else(|| {
            ModelError::Unavailable("network_error: failed to capture stdin".to_owned())
        })?;
        if let Err(e) = stdin.write_all(request.prompt.as_bytes()).await {
            return Err(ModelError::Unavailable(format!(
                "network_error: failed to write request: {e}"
            )));
        }
        if let Err(e) = stdin.shutdown().await {
            return Err(ModelError::Unavailable(format!(
                "network_error: failed to close stdin: {e}"
            )));
        }

        let stdout = child.stdout.take().ok_or_else(|| {
            ModelError::Unavailable("network_error: failed to capture stdout".to_owned())
        })?;

        // Read streaming events from stdout using unfold to create a boxed stream.
        let state = ReadState::new(child, stdout, cancel, self.config.idle_timeout);
        let stream = unfold(state, |state| async move { state.next().await }).boxed();

        Ok(stream)
    }
}

// --- stream-json envelope (docs/05 §4) ------------------------------------------------
//
// `claude -p --output-format stream-json --include-partial-messages` emits one JSON
// object per line. We care about two: `stream_event` (wraps a verbatim Anthropic
// streaming event, our source of token deltas + stop reason) and `result` (the
// authoritative terminal, carrying success/error). Everything else — `system`
// init, complete `assistant`/`user` echoes, and any future line type — is skipped
// for forward compatibility.

/// A top-level CLI stream-json line.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CliMessage {
    /// Wraps a verbatim Anthropic streaming event.
    StreamEvent { event: StreamEvent },
    /// The terminal result line.
    Result(ResultLine),
    /// `system` / `assistant` / `user` / anything else: ignored.
    #[serde(other)]
    Other,
}

/// The inner Anthropic streaming event (only the variants we act on).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    ContentBlockDelta {
        delta: Delta,
    },
    MessageDelta {
        delta: MessageDelta,
    },
    /// `message_start` / `content_block_start` / `content_block_stop` /
    /// `message_stop` / `ping`: no action (terminal is the `result` line).
    #[serde(other)]
    Other,
}

/// A content-block delta payload.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Delta {
    TextDelta {
        text: String,
    },
    /// `input_json_delta` (tool input): skipped — the reasoning profile has no
    /// tools, and tool calls are not an M1 path.
    #[serde(other)]
    Other,
}

/// The `message_delta` payload carries the turn's stop reason.
#[derive(Debug, Deserialize)]
struct MessageDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

/// The terminal `result` line.
#[derive(Debug, Deserialize)]
struct ResultLine {
    #[serde(default)]
    subtype: String,
    #[serde(default)]
    is_error: bool,
}

/// The outcome of parsing one output line — a pure reduction of the envelope,
/// unit-tested against fixtures without spawning a process.
#[derive(Debug, PartialEq, Eq)]
enum ParseStep {
    /// Emit a text delta to the stream.
    Text(String),
    /// Record the turn's finish reason (applied when the terminal arrives).
    Finish(FinishReason),
    /// The authoritative terminal: the turn completed successfully.
    Done,
    /// A terminal failure (error result, or unparseable line).
    Failed(ModelError),
    /// Nothing to emit; keep reading.
    Skip,
}

/// Reduce one stream-json line to a [`ParseStep`]. Pure and total: any parse
/// failure becomes `Failed(Malformed)` rather than propagating.
fn classify_line(line: &str) -> ParseStep {
    match serde_json::from_str::<CliMessage>(line) {
        Ok(CliMessage::StreamEvent { event }) => match event {
            StreamEvent::ContentBlockDelta {
                delta: Delta::TextDelta { text },
            } => ParseStep::Text(text),
            StreamEvent::ContentBlockDelta {
                delta: Delta::Other,
            } => ParseStep::Skip,
            StreamEvent::MessageDelta { delta } => match delta.stop_reason.as_deref() {
                Some("max_tokens") => ParseStep::Finish(FinishReason::Length),
                Some(_) => ParseStep::Finish(FinishReason::Stop),
                None => ParseStep::Skip,
            },
            StreamEvent::Other => ParseStep::Skip,
        },
        Ok(CliMessage::Result(result)) => {
            if result.is_error {
                // Coarse mapping (M1): any error result is provider unavailability
                // the router queues behind. Only a stable, non-sensitive subtype
                // token crosses the boundary (invariant 5, docs/06 §5); fine-
                // grained quota/auth classification is F1.7's refinement.
                ParseStep::Failed(ModelError::Unavailable(format!(
                    "provider_error: {}",
                    stable_subtype(&result.subtype)
                )))
            } else {
                ParseStep::Done
            }
        }
        Ok(CliMessage::Other) => ParseStep::Skip,
        Err(e) => {
            warn!(error = %e, "failed to parse claude event");
            ParseStep::Failed(ModelError::Malformed("failed to parse event".to_owned()))
        }
    }
}

/// Reduce a CLI error `subtype` to a stable token so no arbitrary provider text
/// crosses the trust boundary (invariant 5).
fn stable_subtype(subtype: &str) -> &'static str {
    match subtype {
        "error_max_turns" => "error_max_turns",
        "error_during_execution" => "error_during_execution",
        _ => "unknown",
    }
}

/// State machine for reading from the subprocess, held across stream iterations.
struct ReadState {
    child: Child,
    reader: BufReader<tokio::process::ChildStdout>,
    cancel: CancellationToken,
    idle_timeout: Duration,
    /// The finish reason seen on `message_delta`, applied when `result` arrives.
    finish: FinishReason,
    finished: bool,
}

impl ReadState {
    fn new(
        child: Child,
        stdout: tokio::process::ChildStdout,
        cancel: CancellationToken,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            child,
            reader: BufReader::new(stdout),
            cancel,
            idle_timeout,
            finish: FinishReason::Stop,
            finished: false,
        }
    }

    /// Kill the child and mark the stream finished, returning a terminal event.
    async fn terminate(mut self, event: ModelEvent) -> Option<(ModelEvent, Self)> {
        let _ = self.child.kill().await;
        self.finished = true;
        Some((event, self))
    }

    /// Read the next event from the stream, returning (event, updated_state).
    /// Returns None when the stream is exhausted.
    async fn next(mut self) -> Option<(ModelEvent, Self)> {
        loop {
            if self.finished {
                return None;
            }

            // Exit on cancellation: kill + reap, then surface the terminal error.
            if self.cancel.is_cancelled() {
                return self
                    .terminate(ModelEvent::Error(ModelError::Unavailable(
                        "cancelled".to_owned(),
                    )))
                    .await;
            }

            let mut line = String::new();
            // Cap each line read so one unbounded line can't exhaust memory within
            // the idle window. `Take` bounds this single read; a fresh cap applies
            // each iteration. Scoped so the `&mut self.reader` borrow ends before
            // the match arms move `self`.
            let deadline = {
                let mut capped = (&mut self.reader).take(MAX_LINE_BYTES);
                tokio::time::timeout(self.idle_timeout, capped.read_line(&mut line)).await
            };

            match deadline {
                Ok(Ok(0)) => {
                    // EOF: the process ended. A `result` line normally produces
                    // `Done` before this; a stream that ends without one, or with
                    // a non-zero exit, is a truncated/failed run.
                    self.finished = true;
                    if let Ok(status) = self.child.wait().await
                        && !status.success()
                    {
                        return Some((
                            ModelEvent::Error(ModelError::Malformed(
                                "claude CLI exited with non-zero status".to_owned(),
                            )),
                            self,
                        ));
                    }
                    return None;
                }
                Ok(Ok(_)) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match classify_line(trimmed) {
                        ParseStep::Text(text) => return Some((ModelEvent::TextDelta(text), self)),
                        ParseStep::Finish(reason) => {
                            self.finish = reason;
                            continue;
                        }
                        ParseStep::Done => {
                            let reason = self.finish;
                            return self.terminate(ModelEvent::Done(reason)).await;
                        }
                        ParseStep::Failed(error) => {
                            return self.terminate(ModelEvent::Error(error)).await;
                        }
                        ParseStep::Skip => continue,
                    }
                }
                Ok(Err(e)) => {
                    return self
                        .terminate(ModelEvent::Error(ModelError::Malformed(format!(
                            "read error: {e}"
                        ))))
                        .await;
                }
                Err(_) => {
                    // Idle timeout: the process is stuck. `timeout:` prefix lets
                    // health::reason_code classify it (NFR-03).
                    return self
                        .terminate(ModelEvent::Error(ModelError::Unavailable(
                            "timeout: provider idle timeout".to_owned(),
                        )))
                        .await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the pure line classifier over a fixture, mirroring `ReadState`'s
    /// reduction (finish reason latched on `message_delta`, applied at `result`),
    /// and collect the resulting model events — the same sequence the async loop
    /// would emit, without spawning a process.
    fn drive(fixture: &str) -> Vec<ModelEvent> {
        let mut events = Vec::new();
        let mut finish = FinishReason::Stop;
        for line in fixture.lines().map(str::trim).filter(|l| !l.is_empty()) {
            match classify_line(line) {
                ParseStep::Text(text) => events.push(ModelEvent::TextDelta(text)),
                ParseStep::Finish(reason) => finish = reason,
                ParseStep::Done => {
                    events.push(ModelEvent::Done(finish));
                    break;
                }
                ParseStep::Failed(error) => {
                    events.push(ModelEvent::Error(error));
                    break;
                }
                ParseStep::Skip => {}
            }
        }
        events
    }

    const HEALTHY: &str = include_str!("../tests/fixtures/claude-cli/healthy_stream.jsonl");
    const MAX_TOKENS: &str = include_str!("../tests/fixtures/claude-cli/max_tokens_stream.jsonl");
    const TOOL_DELTA: &str = include_str!("../tests/fixtures/claude-cli/tool_delta_skipped.jsonl");
    const ERROR_RESULT: &str = include_str!("../tests/fixtures/claude-cli/error_result.jsonl");
    const GARBAGE: &str = include_str!("../tests/fixtures/claude-cli/garbage.jsonl");
    const TRUNCATED: &str = include_str!("../tests/fixtures/claude-cli/truncated_stream.jsonl");

    #[test]
    fn healthy_stream_yields_text_then_done_stop() {
        assert_eq!(
            drive(HEALTHY),
            vec![
                ModelEvent::TextDelta("Hello".to_owned()),
                ModelEvent::TextDelta(", world".to_owned()),
                ModelEvent::Done(FinishReason::Stop),
            ]
        );
    }

    #[test]
    fn max_tokens_stop_reason_maps_to_length() {
        let events = drive(MAX_TOKENS);
        assert_eq!(events.last(), Some(&ModelEvent::Done(FinishReason::Length)));
    }

    #[test]
    fn tool_input_delta_is_skipped_not_emitted() {
        // input_json_delta must not surface as text; the run still completes.
        assert_eq!(
            drive(TOOL_DELTA),
            vec![
                ModelEvent::TextDelta("ok".to_owned()),
                ModelEvent::Done(FinishReason::Stop),
            ]
        );
    }

    #[test]
    fn error_result_maps_to_unavailable_with_stable_code() {
        let events = drive(ERROR_RESULT);
        assert_eq!(
            events.last(),
            Some(&ModelEvent::Error(ModelError::Unavailable(
                "provider_error: error_during_execution".to_owned()
            )))
        );
    }

    #[test]
    fn garbage_line_is_malformed() {
        assert_eq!(
            drive(GARBAGE),
            vec![ModelEvent::Error(ModelError::Malformed(
                "failed to parse event".to_owned()
            ))]
        );
    }

    #[test]
    fn truncated_stream_emits_deltas_then_no_terminal() {
        // No `result` line: deltas surface, but the loop never emits Done — the
        // async reader treats the subsequent EOF as the terminal (see `next`).
        assert_eq!(
            drive(TRUNCATED),
            vec![ModelEvent::TextDelta("partial".to_owned())]
        );
    }

    #[test]
    fn system_and_assistant_lines_are_ignored() {
        assert_eq!(
            classify_line(r#"{"type":"system","subtype":"init","tools":[]}"#),
            ParseStep::Skip
        );
        assert_eq!(
            classify_line(r#"{"type":"assistant","message":{"content":[]}}"#),
            ParseStep::Skip
        );
    }

    #[test]
    fn unknown_stop_reason_is_stop_not_error() {
        assert_eq!(
            classify_line(
                r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"}}}"#
            ),
            ParseStep::Finish(FinishReason::Stop)
        );
    }
}
