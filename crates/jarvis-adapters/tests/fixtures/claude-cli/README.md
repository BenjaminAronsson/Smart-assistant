# claude-cli parser fixtures

Newline-delimited JSON samples of `claude -p --output-format stream-json
--include-partial-messages` output (ADR-004, docs/03 §4, docs/05 §4), driving the
`classify_line` unit tests in `crates/jarvis-adapters/src/claude_cli.rs`.

| Fixture | Exercises |
|---|---|
| `healthy_stream.jsonl` | system init + partial text deltas + `end_turn` + success result → `TextDelta`×2, `Done(Stop)` |
| `max_tokens_stream.jsonl` | `stop_reason: "max_tokens"` → `Done(Length)` |
| `tool_delta_skipped.jsonl` | `input_json_delta` is skipped, not surfaced as text |
| `error_result.jsonl` | `result` with `is_error: true` → `Unavailable("provider_error: …")` |
| `garbage.jsonl` | non-JSON line → `Malformed` |
| `truncated_stream.jsonl` | deltas with no terminal `result` (EOF becomes the terminal in the async reader) |

## Provenance / caveat

These are **hand-authored** against the documented CLI stream-json envelope, not
captured from a live `claude` process (the provider-adapter skill's "record from
real output once, reviewed for secrets" step is still owed). They are internally
consistent and lock the parser's behaviour, but the exact envelope must be
reconciled against **one real captured sample** before this adapter is trusted in
production — in particular the `--allowedTools` disable flag and whether
`--include-partial-messages` is the right switch for token deltas on the installed
CLI version. Capture with a throwaway prompt in the controlled workdir, scrub any
session ids / paths, and diff against these.
