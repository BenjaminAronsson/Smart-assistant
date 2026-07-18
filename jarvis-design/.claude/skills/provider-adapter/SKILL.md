---
name: provider-adapter
description: Implementing ModelProvider adapters, especially the Claude Code CLI adapter - process control, stream-json parsing, quota/health detection, single-flight. Use whenever touching jarvis-adapters model code.
---

# Model provider adapters

Spec: docs/03 §4, docs/05 §4, ADR-004, ADR-011.

## Claude CLI adapter specifics
1. Spawn `claude -p --output-format stream-json` via tokio::process in the configured
   workdir; reasoning profile disables built-in tools (flag list from jarvisd.toml).
   Prompt via stdin where supported; NEVER secrets or prompt content in argv (process
   listings leak).
2. Parse stream-json line-by-line into `ModelEvent`; unknown event types log-and-skip
   (forward compatible); malformed JSON beyond N tolerance => ProviderError + unhealthy.
   ALL parsing is developed against fixtures in tests/fixtures/claude-cli/ (healthy
   stream, tool-proposal stream, quota error, auth error, truncated stream, garbage).
   Record new fixtures from real output once, by hand, reviewed for secrets.
3. Health classification from exit code + stderr + event content: AuthMissing |
   QuotaExhausted { reset_hint } | RateLimited | Malformed | IdleTimeout | Crash.
   Quota/rate => unhealthy with backoff (initial/max from config); reset window surfaced
   via the providers endpoint + a WS event.
4. Single-flight: the adapter owns a semaphore(1); the run queue (application layer)
   orders interactive > background. Never raise concurrency to "speed things up".
5. Cancellation = kill process group + await reap with timeout + assert no zombie
   (test with a fake sleeping CLI). Idle timeout likewise.
6. Fresh process per run by default; `--resume` only if explicitly modeled - context
   comes from Jarvis's assembler, not CLI session state.

## Any adapter
Implements the `ModelProvider` port only; adapter types never leak upward. Health feeds
the router; the router never silently switches when sensitivity forbids (test). The
`FakeModel` mirrors the same port and drives all orchestrator tests + golden traces -
keep it feature-equivalent (streaming, tool proposals, errors, delays).
