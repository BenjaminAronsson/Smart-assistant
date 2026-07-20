# 02 — Target system architecture

## 1. Shape of the system

A **modular monolith in Rust** (`jarvisd`) owns sessions, deterministic orchestration,
policy, memory, artifacts, audit, and presentation state. Everything that carries unusual
dependencies, executes untrusted work, or needs independent lifecycle runs **out of
process**: model servers, MCP tool servers, the browser worker, the coding worker, voice
services, Home Assistant, and the desktop agent.

```
┌────────────────────────────────────────────────────────────────────────┐
│  Clients (Z1)                                                          │
│  Angular shell (Chromium app windows) · jarvis-agent (Hyprland IPC)    │
└───────────────▲───────────────────────────────▲────────────────────────┘
                │ REST + WebSocket (versioned)  │ agent WS
┌───────────────┴───────────────────────────────┴────────────────────────┐
│  jarvisd — Rust modular monolith (Z0)                                  │
│  gateway │ conversations │ orchestrator │ models │ tools │ policy      │
│  memory  │ artifacts     │ automations  │ integrations │ observability │
└──┬─────────┬───────────┬───────────┬───────────┬───────────┬───────────┘
   │ stdio/  │ HTTP      │ HTTP/WS   │ Wyoming   │ spawn     │ sqlx
   │ SSE     │           │           │ TCP       │ stdio     │
┌──▼───┐ ┌───▼────┐ ┌────▼─────┐ ┌───▼────┐ ┌────▼─────┐ ┌───▼──────────┐
│ MCP  │ │ Ollama │ │ Home     │ │ Voice  │ │ Claude   │ │ PostgreSQL   │
│ tool │ │ /llama │ │ Assistant│ │ (VAD/  │ │ Code CLI │ │ + pgvector   │
│ srvs │ │ .cpp   │ │          │ │ STT/   │ │ (Z2)     │ │ + artifact   │
│ (Z3) │ │ (Z2)   │ │ (Z2)     │ │ TTS)   │ │          │ │   CAS store  │
└──────┘ └────────┘ └──────────┘ └────────┘ └──────────┘ └──────────────┘
```

Trust zones Z0–Z5 are defined in `06-security.md` §2.

## 2. Why this shape (summary; full rationale in ADRs)

- One deployable, one debugger, one transaction boundary — right for a solo owner
  ([ADR-002](adr/README.md#adr-002)).
- The seams that may later split are already process boundaries: voice, model workers,
  tool workers, browser, remote nodes.
- No internal message broker until a second machine proves the need
  ([ADR-010](adr/README.md#adr-010)); modules communicate in-process and publish
  post-commit domain events via a transactional outbox; clients receive them over
  WebSocket.

## 3. Crate map and module boundaries

| Crate | Owns | May depend on |
|---|---|---|
| `jarvis-domain` | Entities, value types, `RunState`, risk tiers, grant types, budget types. Pure logic, no I/O. | std, serde, thiserror only |
| `jarvis-application` | Use cases, orchestrator state machine, context assembler, router, policy engine, **ports** (traits: `ModelProvider`, `ToolExecutor`, `Repository`, `EventPublisher`, `Clock`, `SecretStore`). | `jarvis-domain` |
| `jarvis-contracts` | Versioned wire DTOs, WS event envelope, JSON Schemas (schemars). | `jarvis-domain` (read-only mapping) |
| `jarvis-infra` | sqlx repositories, migrations, outbox, artifact CAS, keyring secret store, OTel wiring. | application ports |
| `jarvis-adapters` | `claude_cli`, `home_assistant`, `mcp_host` (rmcp), `wyoming`, `embeddings` (fastembed). `ollama`/`anthropic_api` land later behind the same `ModelProvider` port (ADR-011). Each adapter behind an application port. | application ports |
| `jarvisd` | axum host: REST routes, WS hub, auth, DI wiring, config, health. | all above |
| `jarvis-agent` | Desktop agent binary: Hyprland request/event sockets, window placement, app launch allowlist, PTT hotkey, audio capture hand-off. | `jarvis-contracts` |
| `xtask` | Codegen (schemas → TS types), arch tests, golden-trace runner. | dev-only |

**Dependency rule (NFR-08):** domain ← application ← {contracts, infra, adapters} ← jarvisd.
Provider SDK types, sqlx entities, axum extractors, and Hyprland details must never appear
in `jarvis-domain`/`jarvis-application`. Enforced by `cargo xtask arch-test` and `cargo deny`.

Module responsibilities (mirrors the crate internals, one Rust module per row):

| Module | Owned responsibility |
|---|---|
| identity | Local user identity, device pairing, scopes, capability grants. |
| conversations | Sessions, messages, branches, context references, streaming presentation events. |
| orchestrator | Run state machine, budgets, cancellation, planner/executor transitions, recovery. |
| models | Provider registry, capability profiles, routing, prompt assembly, usage, fallback. |
| tools | Tool catalogue, MCP clients, native tools, policy metadata, execution, result normalization. |
| policy | Risk classification, grants, approval requests/decisions, compensating actions. |
| memory | Candidate extraction, review, retention, embeddings, retrieval, provenance. |
| artifacts | Manifests, versions, builders, renderers, CAS storage, sandbox launch. |
| automations | Schedules, triggers, execution identities, missed-run behavior, policy re-evaluation. |
| integrations | Home Assistant, browser, OS/files, future channels. |
| observability | Audit events, traces, metrics, diagnostics bundle, privacy-safe logs. |

## 4. Orchestrator state machine (FR-01..07, ADR-003)

```rust
pub enum RunState {
    Received,        // input persisted, identity + idempotency validated
    ContextReady,    // bounded context assembled within budget
    ModelRunning,    // provider stream active
    PolicyReview,    // proposal validated, risk classified
    WaitingApproval, // exact approval card published with expiry
    ToolRunning,     // one bounded tool call executing
    Replanning,      // observations returned to model, budget decremented
    Responding,      // final output streaming
    Completed, Failed, Cancelled,   // terminal; idempotent
}
```

The loop is code, not model:

```rust
while !run.state.is_terminal() {
    cancel.check()?;
    budgets.check(&run)?;
    run = match run.state {
        Received                     => context.prepare(run).await?,
        ContextReady | Replanning    => model_step.execute(run).await?,
        ModelRunning                 => unreachable!("driven inside model_step"),
        PolicyReview                 => policy_step.evaluate(run).await?,
        WaitingApproval              => approval_step.wait(run).await?,
        ToolRunning                  => tool_step.execute(run).await?,
        Responding                   => response_step.commit(run).await?,
        Completed | Failed | Cancelled => break,
    };
    checkpoints.save(&run).await?;   // safe-transition checkpoint (NFR-05/13)
}
```

Rules: max replanning loops, per-run `RunBudget { max_model_turns, max_tool_calls,
max_duration, max_artifact_bytes }`, cancellation propagated via `CancellationToken` to
model process, tool workers, and audio. Checkpoints make restart-recovery reconcile via
idempotency keys instead of duplicating mutations.

## 5. Runtime sequence — tool-using request

1. Client sends message with session ID, device context, idempotency key.
2. `conversations` persists the user message and starts a run (ack <100 ms).
3. Context assembler builds a **bounded, inspectable** package: recent messages, rolling
   summary, pinned facts, retrieved memories, referenced artifacts, display/home state,
   tool schemas — each item with provenance, sensitivity, token estimate. The user can
   inspect what will be sent to a cloud provider (NFR-02).
4. Router selects a model profile by task class, required capabilities, sensitivity,
   availability, latency/cost budget. It never asks a model to select itself.
5. Model returns text and/or structured tool proposals.
6. Policy engine validates tool identity, arguments, target resource, grants, risk class.
7. For R2/R3, the client receives an approval card with exact effects and expiry.
8. Executor invokes native tool or MCP server with cancellation, timeout, correlation,
   least-privilege credentials.
9. Result validator checks schema, size, policy labels; orchestrator replans within
   budget or completes.
10. Final response + artifact links stream to subscribed displays; memory candidates and
    audit events commit transactionally with the run outcome.

## 6. Artifacts (FR-08, FR-18)

An artifact is a durable versioned output with an immutable manifest: id, version,
creator run, SHA-256, media type, renderer, sources, sensitivity, build environment,
capabilities. Blobs live in a content-addressed file store keyed by SHA-256; PostgreSQL
holds manifests and provenance ([ADR-008](adr/README.md#adr-008)). Initial renderers:
Markdown/HTML, code/text, images, simple charts. Generated web apps are bundle artifacts
executed only in the sandbox defined in `06-security.md` §6.

## 7. Memory (FR-16)

Four layers — working context (automatic, expiring), episodic summaries (automatic
candidates, configurable retention), semantic facts (explicit confirmation or repeated
high-confidence evidence), procedural playbooks (versioned, reviewed, never inferred from
one risky action). Raw messages are history, not memory. Every memory records source,
confidence, sensitivity, scope, retention rule, and the embedded text. Retrieval combines
pgvector similarity with deterministic filters (user, project, time, source, sensitivity)
and records which memories influenced a run. "Forget" removes derived embeddings too. **Secret-shaped content never enters memory
(catalog O2):** if the user asks Jarvis to "remember" something recognizable as a
credential (passwords, keys, codes), the memory module declines to store it as plaintext
memory and offers the keyring path instead — memory rows are not a secret store.

## 8. Desktop shell, agent, and browser/OS automation (FR-09/10/15)

- **Angular shell**, served by `jarvisd`, is state-driven: server maintains logical
  surfaces (Conversation, RunTimeline, ApprovalTray, ArtifactCanvas, AmbientStatus,
  Diagnostics); a display profile maps surfaces to monitor/workspace.
- **jarvis-agent** launches Chromium app-mode windows with stable app IDs and uses
  Hyprland's request/event UNIX sockets (requests short-lived, events async) to place,
  focus, and observe windows. It exposes narrow commands only: list
  monitors/workspaces/windows, launch allowlisted app, focus/move window, capture approved
  screenshot region, read clipboard on explicit request. **It is not a shell.**
- **Browser automation** runs Playwright in a dedicated worker process with isolated
  profiles per trust domain, visible mode for consequential operations, credentials in the
  browser/secret store (never prompts), and typed tool actions (navigate, extract, click,
  download, screenshot) with audit evidence.
- Complex system work is delegated to a sandboxed coding worker producing a reviewable
  patch artifact — never direct host mutation.

## 9. Voice pipeline (FR-13, ADR-007, ADR-011)

Milestone order: push-to-talk → barge-in → wake word → room attribution. VAD (Silero)
gates audio and detects end-of-turn; STT (faster-whisper or whisper.cpp) produces partial
and final transcripts; TTS (Piper or alternative) starts from complete clauses and stops
immediately on barge-in. All engines run as Wyoming-compatible services out of process, so
they can change without touching the core, and Home Assistant voice satellites become the
whole-house upgrade path. Wake-word and voice-model licensing is reviewed per asset.

**CPU-only reality (A-04).** STT and TTS are viable without a GPU: faster-whisper
`base`/`small` int8 and Piper both run near-real-time on a modern desktop CPU. Validate
NFR-04 on the reference machine at M5; if the CPU misses the 0.8 s transcript budget,
relax the budget or drop one STT model size — do not block the milestone on hardware.

**Quota-first routing.** A final transcript is first matched against the deterministic
intent grammar (HA sentence triggers + Jarvis direct commands). Only unresolved
utterances become an LLM run, so routine voice commands ("turn off the kitchen lights",
"stop", "what's the time") cost zero Claude quota and work in degraded mode.

## 10. Home Assistant integration (FR-14, ADR-006)

HA is the home system of record. Jarvis uses a dedicated least-privilege token, caches
entity/area metadata but treats HA as authoritative, and exposes a **curated** tool layer
(`home.get_state`, `home.set_light`, `home.execute_scene`, `home.run_script`) — never the
whole service namespace. Approvals show friendly name + entity ID. Jarvis may propose new
HA automations only through R2 approval with a diff. The home functions when Jarvis is down.

## 11. Automations (FR-17)

Stored intents with trigger, execution identity, input template, allowed tools, provider
policy, budget, retry/missed-run behavior, and notification policy. **Policy is
re-evaluated at execution time** — a creation-time approval does not authorize a changed
target later. Condition watches use bounded polling or event subscriptions and stay silent
when false.

## 11a. Media integration (FR-21/22, ADR-012)

Three mechanisms, deliberately different in depth:

- **MPRIS (universal local control).** A `media_mpris` adapter (zbus, D-Bus session bus)
  discovers `org.mpris.MediaPlayer2.*` players and exposes `media.playback`
  (play/pause/next/previous/seek/volume) plus a `media.state` transient WS event feeding
  the media bar (`12` §5). This one adapter controls the Spotify desktop app, Chromium
  playing YouTube — and anything else that registers, with no per-service work.
- **Spotify Web API (service-level actions).** `spotify` adapter: OAuth
  authorization-code + PKCE with refresh token in the keyring; scopes limited to
  playback/read/playlist-read; tools `spotify.search`, `spotify.play` (uri + Connect
  device), `spotify.play_playlist { name }`, `spotify.queue_add`. Playback-control
  endpoints require Premium — detected and surfaced, not assumed. Playlist mutations are
  R2; playback is R1 with a config volume cap. Artist-only resolution defaults to that
  artist's shuffled-top-tracks context, no clarification for the common case;
  `play_playlist` resolves against the user's own saved playlists first, public search
  only as fallback (ADR-022).
- **Cast-a-link (web video).** `media.open_url` launches/reuses the dedicated media
  Chromium window (own app-id, own profile, no credentials) on a chosen display via
  `jarvis-agent`; from there MPRIS provides transport control. Optional YouTube Data API
  key enables `youtube.search`; without it, search goes through the browser worker.

Risk tiers: transport control and volume-within-cap R1; playlist/library mutation R2;
volume above cap requires approval (hearing protection is a real reversibility question).
Media tools are registered in the standard catalogue — no special path.

**"What's playing" is a first-class query (FR-32, ADR-022).** Answered from the same
MPRIS metadata feeding the media bar — spoken answer plus a now-playing card
(title/artist/album, `mpris:artUrl` when the active player exposes it) — not just passive
display. Multi-player ambiguity ("two players active") asks via the ADR-016 fluent
single-question pattern, never a picker — the media-integration skill's ambiguity
handling cross-references that rule explicitly rather than leaving the ask-mechanism
unstated.

## 11b. General web search & fetch (FR-25, ADR-014)

The default open-domain knowledge source, used whenever no dedicated integration
(Home Assistant, Spotify, MPRIS, Maps/PMTiles) covers the query — current facts, "who is
this", weather, restaurant/place lookups, and any entity image the HUD wants to show.

- **Two tools, R0/read-only**: `web.search { query } -> [{ title, url, snippet }]` and
  `web.fetch { url } -> { title, text, primary_image_url?, source_url }`. The provider
  behind `web.search` is a config-swappable adapter (default: Brave Search API; any
  keyed or self-hosted provider fits the same port — no core change to switch).
- **No separate image API.** `web.fetch` extracts a representative image from the page
  itself (Open Graph `og:image` / primary `<img>`) rather than calling a dedicated image
  search service. Every image sourced this way carries its `source_url` end to end into
  the HUD card, rendered as a small visible attribution link (`12` §2.3) — this is both
  the copyright-safe move (never presented as Jarvis's own content) and the simplest
  implementation.
- **Fetched content is Z4 untrusted** (`06` §2), same as any web page or document: it is
  schema-validated, size-truncated, and stripped of anything resembling instructions
  before it reaches the model as tool-result content; a malicious page cannot use
  `web.fetch` as an injection vector into a tool call (`06` §5 threat table already
  covers this — FR-25 is explicitly named as subject to it, not a new exception).
- **Routing**: the context assembler / router treats "current officeholder", "current
  price/score/status", "what's the weather", and similar time-sensitive phrasing as a
  signal to prefer `web.search` over answering from model memory — the failure mode
  being guarded against is a stale or hallucinated answer to a question with a
  factual, checkable, current answer.
- **HUD card mapping**: search/fetch results populate the existing entity/place/value
  card types (`12` §2.3) unchanged — this is a data-sourcing fix, not a new card type.
  A card with no extractable image renders text-only rather than fabricating one.
- **Out of v1 scope**: a dedicated Places API (ratings/hours/menus structured data) and a
  dedicated weather API are deferred (ADR-014 revisit triggers) — FR-25 covers them at
  best-effort quality via search+fetch until/unless that quality proves insufficient.

## 11c. Location provider (FR-26, ADR-015)

Location-dependent `web.search` calls ("nearby", "near me", "find a X close by") need
coordinates, not just text. A `LocationProvider` port resolves them in order: (1) paired
device GPS (`jarvis-agent` or a mobile client, when the location scope is granted), (2)
configured home coordinate (`[location] home_lat/home_lon`), (3) IP geolocation as a
coarse last resort, explicitly labeled approximate when used. Resolved coordinates are
attached to the search query and pass through the context assembler like any other
context item — labeled, provenance-tracked, sensitivity-classified (NFR-02) — never
silently attached to an outbound cloud request.

## 11d. News synthesis: interest profile and contested-topic framing (FR-29/30)

Topicless "what's the news" resolves against a user **news-interest profile** (`[news]`
config: topics, sources, optional weights — ADR-019) into concrete per-topic headline
queries rendered as headlines/digest cards. With no profile set, Jarvis asks once,
fluently, what the user follows and offers to remember it, rather than re-asking daily.

For **contested, political, or conflict** topics (ADR-020), the news-synthesis path
applies a firm framing rule regardless of source: attribute claims to their source rather
than asserting them as fact (preserving the hedging present in good reporting),
present contested points even-handedly, and avoid sensationalized graphic detail in the
spoken summary. This rule composes with source-quality weighting (ADR-016) — quality
selects *which* sources; this governs *how* their claims are voiced.

## 11e. Personal utilities: timers, lists, calendar, outbound mail (FR-33..36)

- **Timers/alarms/reminders (ADR-023).** A dedicated lightweight module, entirely in the
  deterministic grammar: zero LLM, offline-capable, Postgres-persisted (restart-safe,
  missed alarms announced on restart with a notice). Firing = audible alert on a playback
  path independent of the TTS pipeline (an alarm must sound even if voice services are
  down) + TTS announcement when available + a live countdown/reminder card. Named,
  enumerable, voice dismiss/snooze. Boundary with FR-17: anything needing policy
  re-evaluation or model reasoning at fire time is an automation; "make a noise at T" is
  a timer.
- **Lists & notes (ADR-024).** Named lists with grammar-driven add/remove/check-off/read,
  a list card, quick notes as single-item captures, promotion to artifacts when a list
  becomes a document.
- **Calendar (ADR-025).** CalDAV adapter: reads R0 → agenda card; create/modify R2 with
  the exact event in the approval; calendar content is sensitivity-labeled personal
  context under the standard context-assembly visibility rules.
- **Outbound mail (ADR-026).** One SMTP adapter completes the canonical send_message R2
  flow: verbatim recipient/subject/body in the approval card, idempotency key per send.
  Inbox reading deferred to v2.
- **Math & unit conversions in the grammar (catalog F4/F5).** Arithmetic, percentages,
  and unit conversions are answered by the deterministic grammar (value card + spoken
  answer) — never a Claude round-trip for "15% of 230".
- **Corrections (catalog M1).** "No, I meant the kitchen" re-executes the prior command
  with the correction applied: for reversible R1 actions, compensate (undo) then redo;
  for R2 actions, the corrected version is a *new* approval — a stale grant never covers
  corrected arguments (this already falls out of grant args-hashing; stated here so the
  UX is deliberate, not incidental).
- **Spotify device aliases (catalog B5).** `[integrations.spotify] device_aliases` maps
  room names to Connect device IDs so "in the kitchen" targets the right speaker.

## 12. Deployment topology (single-PC v1)

| Unit | Form | Privileges / notes |
|---|---|---|
| `jarvisd` | systemd service (or container) | No root; binds loopback by default; owns DB access and policy. |
| `jarvis-agent` | user systemd service | Runs in the graphical session; limited Hyprland/audio/window capabilities. |
| Angular assets | served by `jarvisd` | No direct tool network access; authenticated WS/REST only. |
| PostgreSQL + pgvector | container | Local volume; never exposed publicly. |
| Ollama | native or container | GPU if available; loopback/private network only. |
| Voice services | containers | Wyoming exposed only on private network. |
| Home Assistant | existing instance | Dedicated token, allowlisted capabilities. |
| Tool/browser/coding workers | per-trust containers | Read-only mounts default; CPU/mem/time/network limits. |

Startup order: Postgres migrations are applied by ops (`sqlx migrate run`) before
`jarvisd` starts. **Implementation note (M0/M1):** `jarvisd` itself starts even if
Postgres is unreachable at boot — it uses a lazy connection pool and reports the
database as a degraded adapter on the health endpoint rather than failing to start
(`crates/jarvisd/src/main.rs`, `jarvis-infra::db::connect_lazy`); a restart re-runs
pairing bootstrap once the database is reachable. UI + agent connect; health page shows
adapter states → local model/voice/HA adapters register asynchronously and update
routing eligibility → scheduled work runs only when its required capabilities are
healthy.

## 13. Whole-house evolution

The core stays authoritative for policy, memory, artifacts. Room satellites are thin:
audio capture/playback, presence, LEDs/display, narrow local fallbacks. HA may own Wyoming
voice satellites. Node pairing (FR-19) adds device identity + presentation capabilities
with challenge-response pairing, per-device keys, mTLS or signed tokens, revocation.
Introduce WebRTC/LiveKit only for clients crossing unreliable networks or needing echo
cancellation/video/telephony. Introduce NATS JetStream behind the existing
`EventPublisher` port only when a second machine needs durable messaging.

## 14. Observability (NFR-14)

Every interaction gets a trace ID. Spans: context assembly, model queue/first-token/total,
policy, approval wait, tool execution, artifact build, client delivery. Metrics: active
runs, fallback count, model error rate, tokens/s, STT/TTS latency, tool p95, approval
rate, cancellation success, `jarvisd` RSS (NFR-15). Audit events are separate from logs
with stronger retention/integrity (hash-chained; see `06` §7). A diagnostics bundle
excludes secrets and full sensitive prompts by default.
