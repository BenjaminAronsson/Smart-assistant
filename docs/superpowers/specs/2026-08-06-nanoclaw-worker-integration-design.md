# NanoClaw worker integration — design

**Status:** Draft, pending owner review
**Date:** 2026-08-06
**Relates to:** FR-20 (`docs/01-requirements.md:46`), ADR-026 precedent (`docs/adr/README.md:524-541`), M3a worker pattern (browser worker / coding worker)

## Context

The owner runs [nanoclaw](https://github.com/nanocoai/nanoclaw) today as their working
agent, reached only through Telegram. NanoClaw is a separate TypeScript project: a host
process spins up one Docker container per chat session, the container runs an autonomous
agent loop via Anthropic's Agent SDK, and it already has strong multi-channel reach
(WhatsApp/Telegram/Slack/Discord/Gmail), persistent markdown memory, and the ability to
spawn sub-processes inside its sandbox. The owner wants "the memories and capabilities of
nanoclaw" available through "at least the presentation layer of Jarvis" — one system
instead of two front doors.

Jarvis already reserves a requirement slot for this: **FR-20** ("support chat channels
through separate adapters or an OpenClaw bridge", Could-have) and ADR-026 explicitly
deferred inbound-channel/bridge work to FR-20's own future ADR. NanoClaw markets itself as
an OpenClaw alternative, so it's a direct fit for that slot, not a new category of work.

The constraint that shapes this design: Jarvis's invariants forbid any "call tools until
the model feels done" loop outside the orchestrator, and forbid any execution path that
bypasses `policy::evaluate`. NanoClaw's container *is* exactly that kind of autonomous
loop. Making nanoclaw the top-level brain behind Jarvis's UI would mean Jarvis's
presentation layer fronts an engine with no policy gate — a real invariant violation, not
a style question. The owner did not ask for that trade explicitly, and this design avoids
it by defaulting to the option that doesn't require it.

## Decision

Integrate nanoclaw as a **policy-gated worker**, using the same shape Jarvis already built
twice in M3a: the browser worker (`crates/jarvis-adapters/src/browser.rs`) and the coding
worker (`crates/jarvis-adapters/src/coding.rs`). Concretely, the coding worker's shape is
the template:

- A narrow transport trait (`CodingTransport` in `coding.rs:122-139`) that owns nothing
  but "send a request, get a response, honor the deadline." The nanoclaw equivalent —
  call it `NanoclawTransport` — sends one delegated task and returns one result.
- A host-owned wrapper (`CodingWorkerHost`, `coding.rs:166-314`) that holds the transport,
  an `ArtifactStore`, and the actor identity; it is the only thing that decides what the
  worker's output becomes (an `ArtifactManifest`) and writes the durable audit trail. A
  `NanoclawWorkerHost` does the same for nanoclaw's output.
- A declared `ToolPolicy` the host owns, never the worker (`coding_patch_policy()`,
  `coding.rs:140-155`) — nanoclaw's process never gets to assert its own safety, matching
  the same rule MCP-imported tools follow (`jarvisd/src/tools.rs:9-13`).
- Registration through the single site, `crates/jarvisd/src/tools.rs`, timeout-wrapped
  like every other tool.

Jarvis's orchestrator remains the only thing deciding *when* to invoke nanoclaw. Once
invoked, nanoclaw's internal loop is opaque to Jarvis — the same way the coding worker's
internal reasoning is opaque — but the invocation itself is one policy-gated,
timeout-bounded, cancellable, audited tool call, identical in kind to every other tool
Jarvis runs.

### Process boundary

Jarvis's worker talks to **nanoclaw's existing host process**, not to its containers
directly. NanoClaw's host already owns session lookup, container lifecycle, and the
`inbound.db`/`outbound.db` message contract; reimplementing that inside Jarvis would
duplicate a two-level DB design nanoclaw already got right (single-writer files,
`journal_mode=DELETE` to survive VirtioFS). Two sub-options for the transport, to resolve
during `/feature`:

1. **CLI/subprocess transport** — Jarvis spawns nanoclaw's CLI (`nanoclaw.sh` / `bin/`)
   per invocation, matching `ChildCodingTransport`'s stdin/stdout pattern
   (`coding.rs:323-438`) almost exactly.
2. **HTTP transport** — nanoclaw exposes a host-side API nanoclaw's own webhook server
   (`src/webhook-server.ts`) already partially provides; Jarvis calls it and polls for
   completion.

Recommendation: start with (1) — no new long-lived dependency, closest to the coding
worker's already-reviewed pattern, easiest to sandbox and timeout. Revisit (2) only if
nanoclaw's host doesn't expose a clean one-shot CLI entry point for "run this task in
group X, give me the transcript back."

### Tool contract

`worker.nanoclaw.delegate`:

- **Input:** natural-language task, target nanoclaw agent group (folder), optional
  session/thread hint. Host-authored only, same rule as `CodingRequest`
  (`coding.rs:75-89`: "Only the host constructs this").
- **Output:** the full response transcript nanoclaw's container wrote to
  `outbound.db`'s `messages_out`, captured as an `ArtifactManifest`
  (`ArtifactKind` — likely a new `AgentTranscript` or reuse `CodeText`/plain text kind,
  decide in `/feature`) plus a durable `artifact.created` audit event, mirroring
  `produce_patch_artifact` (`coding.rs:207-277`).
- **Risk tier:** open question for the ADR, not decided here. The coding worker is R1
  because it only produces a patch — nothing is applied without a separate approval.
  NanoClaw's task can have real external effects (it can message a Telegram chat, spawn
  sub-processes) inside its own sandbox, and Jarvis cannot see or gate those sub-actions
  individually. Two candidates: (a) R2, requiring an explicit `ExecutionGrant` per
  delegation, accepting that "exact effect" in the grant will be coarser than usual
  because the sub-task is opaque; (b) R1 *if and only if* nanoclaw's own execution is
  configured to be side-effect-free for Jarvis-originated tasks (e.g. a dedicated agent
  group with outbound channel delivery disabled, so the worst case is "wasted compute,"
  not "sent a message on my behalf"). Recommendation: (b) for the first slice — constrain
  the blast radius at the nanoclaw config layer so Jarvis can honestly offer R1, then
  revisit R2 once real external side effects are wired.

### Memory ingestion

NanoClaw's memory is plain markdown with YAML frontmatter (OKF v0.1) — no vector DB, no
embeddings, one concept per file under `groups/<folder>/memory/`. That is cheap to expose
today, before Jarvis's own M4 memory system exists:

- **First slice:** a read-only tool (same shape as `fs_read.rs`, but scoped to nanoclaw's
  memory directory) so a Jarvis run can pull nanoclaw's `index.md`/linked facts into
  context on demand. Provenance matters here — don't let nanoclaw facts look
  Jarvis-native; tag results as sourced from nanoclaw, not silently merged, matching the
  spirit of M4's planned `source`/`confidence`/`sensitivity` fields
  (`docs/02-architecture.md` §7) even before that schema exists.
- **Later (M4):** map nanoclaw's OKF files onto `memory_sources`/`memories`
  (`docs/04-data-model.md`) as one ingestible source type. No design decision needed now
  beyond keeping the first-slice tool's output shape (source + file path + content)
  compatible with becoming a `memory_sources` row later.

### Presentation

No contract changes needed for the first slice. A nanoclaw delegation is just another
tool call in the existing run stream — the web shell already renders tool calls and
artifacts generically. FR-20's larger scope (nanoclaw's *inbound* channels, e.g. a
Telegram message arriving and starting a Jarvis run rather than Jarvis calling out to
nanoclaw) is explicitly out of scope for this slice; that direction needs jarvisd to
accept nanoclaw as a paired device via the existing `/api/v1/auth/pair` +
`POST /sessions/{id}/messages` + `GET /ws/v1` surface (already generic enough, per this
session's exploration — no new trait required), but it's a separate feature.

### ADR

A new ADR is needed before implementation, extending ADR-026's precedent. It must decide:

1. The trust boundary for an opaque external agent — what Jarvis is willing to assert
   about a `worker.nanoclaw.delegate` call when it cannot itself audit nanoclaw's
   internal tool use.
2. The risk tier (R1-constrained-blast-radius vs. R2) and the exact wording of what an
   `ExecutionGrant` promises the user when the delegated task is opaque.
3. Whether nanoclaw's channel adapters are ever invoked directly by Jarvis, or whether
   every interaction always crosses through this one worker boundary (recommend: always
   through the worker boundary, for this slice — keeps one trust boundary instead of many).

## Scope for the first PR

**In:**
- `NanoclawTransport` trait + CLI-subprocess implementation.
- `NanoclawWorkerHost` producing an artifact + audit event.
- `worker.nanoclaw.delegate` tool descriptor + registration in `jarvisd/src/tools.rs`.
- Read-only nanoclaw-memory tool.
- The ADR.
- One golden-trace-style end-to-end test against a fixture transport (no live nanoclaw
  dependency in CI, matching `docs/policy-grants` skill's fixture-first rule) plus a
  manually-verified run against a real local nanoclaw instance, documented in the PR.

**Out (later features):**
- FR-20's inbound direction (nanoclaw/Telegram messages starting Jarvis runs).
- M4 `memory_sources` schema integration (first slice stays file-read-only).
- Any change to nanoclaw itself (it stays an untouched external dependency).
- Multi-channel parity (WhatsApp/Slack/Discord) — only the delegate path matters first.

## Verification

- `cargo test -p jarvis-adapters nanoclaw` — transport + host unit tests against a fake
  transport (pattern: `FakeWorker`/`FakeArtifacts` in `coding.rs:439-520`).
- `cargo xtask arch-test` — confirm no new dependency leaks into `jarvis-domain`/
  `jarvis-application`.
- Manual: run nanoclaw locally, issue a Jarvis run that invokes
  `worker.nanoclaw.delegate`, confirm an artifact + audit row appear and the web shell
  renders the result like any other tool call.
- `security-auditor` review before merge (worker.nanoclaw.delegate is exactly the kind of
  diff it's scoped for — touches tools/policy/adapters).
