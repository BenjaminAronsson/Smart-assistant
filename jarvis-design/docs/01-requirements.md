# 01 — Scope, assumptions and requirements

This is the driving document. Every design element in `02`–`06` and every milestone in
`08` traces back to a requirement here (traceability table in §6).

## 1. Baseline assumptions

| ID | Assumption |
|---|---|
| A-01 | One trusted owner operates v1 on a Linux desktop. |
| A-02 | The desktop runs Hyprland; other compositors are supportable via the desktop-agent adapter. |
| A-03 | The owner has a Claude Pro/Max subscription and can authenticate Claude Code CLI, but does not have Anthropic API billing enabled. |
| A-04 | No hardware capable of useful local *reasoning* models exists in v1. CPU-only embedding models are available. Fallback when Claude is unavailable is deterministic degradation, not a second LLM (see ADR-011). |
| A-05 | Home Assistant is available or installable before smart-home integration. |
| A-06 | First release is personal use only — no external distribution or multi-tenancy. |
| A-07 | The developer controls both server and clients, so contracts evolve under versioned schemas. |
| A-08 | Implementation is performed primarily by Claude Code under human review; the design must be enforceable by tooling (compiler, tests, lints), not convention alone. |

A-08 is new in v2 and shapes the stack choice and the testing strategy.

## 2. Functional requirements

Priorities: **M** must, **S** should, **C** could (v1 horizon).

| ID | Pri | Requirement |
|---|---|---|
| FR-01 | M | Accept text input and stream incremental responses. |
| FR-02 | M | Create, resume, branch, archive, and search sessions. |
| FR-03 | M | Route each run to a configured model profile without leaking provider details into domain logic. |
| FR-04 | M | Invoke typed tools with schema-validated JSON inputs and structured results. |
| FR-05 | M | Classify action risk (R0–R4) and obtain explicit approval for configured risk levels. |
| FR-06 | M | Cancel an active run; propagate cancellation to model, tool, and audio operations. |
| FR-07 | M | Persist model runs, tool calls, approvals, errors, and outcomes in an append-only audit trail. |
| FR-08 | M | Create and version artifacts with provenance, hash, media type, and renderer metadata. |
| FR-09 | M | Render chat, run timeline, approval prompts, and artifacts in a web-based desktop shell. |
| FR-10 | M | Place/focus Jarvis windows on named monitors/workspaces through a desktop agent. |
| FR-11 | M | Support a Claude Code CLI model profile as sole reasoning provider; additional profiles (API, local) pluggable behind the same port. |
| FR-12 | M | Degrade gracefully to deterministic mode when Claude quota, auth, or network is unavailable: UI/history/R0 tools/rule-based home intents keep working; LLM-needing runs queue with a visible waiting state. |
| FR-13 | S | Push-to-talk speech input with partial transcripts, streaming speech output, barge-in. |
| FR-14 | S | Read Home Assistant state and execute allowlisted services/intents. |
| FR-15 | S | Run browser tasks in an isolated profile with screenshots and step audit. |
| FR-16 | S | Store explicit user facts and task summaries as reviewable memory items. |
| FR-17 | S | Schedule local automations with policy re-checked at execution time; triggers may reference HA presence/zone entities (e.g. "when I leave"). |
| FR-18 | S | Generate small local web applications from validated templates; open them sandboxed. |
| FR-19 | C | Pair remote room/display nodes with scoped device capabilities. |
| FR-20 | C | Support chat channels through separate adapters or an OpenClaw bridge. |
| FR-21 | S | Control Spotify playback on the owner's account: search, play/pause/skip, queue, volume (capped), device targeting via Spotify Connect. An artist-only resolution starts that artist's context (shuffled top tracks), no clarification needed for the common case. "Play playlist X" resolves against the user's own saved playlists first, public search only as fallback (ADR-022). |
| FR-22 | S | Cast/open YouTube and generic web video in a dedicated media window on a chosen display; universal local playback control (play/pause/next/volume) for whatever is playing via MPRIS. |

| FR-23 | S | User-selectable background (none / abstract / photo) with the adaptive glass-contrast system so all HUD content stays legible over any wallpaper. |
| FR-24 | M | HUD result-panel lifecycle: new query shelves current panels (max 4 shelf entries, restorable), per-panel and clear-all dismissal, 2-hour auto-expiry; pending approvals exempt from shelving and TTL. |
| FR-25 | M | General web search + page-fetch tool (R0, read-only) as the default open-domain knowledge source: current facts, entity images, weather, and place/restaurant lookups when no dedicated integration exists (HA, Spotify, MPRIS). Images always carry a visible source link — no separate image-search API (ADR-014). Source-quality weighting prefers authoritative domains; genuinely ambiguous queries get one fluent spoken clarifying question, never a multi-option picker (ADR-016). |
| FR-26 | M | Location provider (paired-device GPS → configured home coordinate → IP geolocation, in that order) supplying coordinates to location-dependent `web.search` queries ("nearby", "near me") (ADR-015). |
| FR-27 | S | Deep-dive support: follow-ups that continue a thread extend the canvas instead of shelving it; sources and gallery card types with per-item attribution; "read the source" hands off to the browser worker; threads past a follow-up threshold can be promoted to a durable Research Notes artifact (ADR-017). |
| FR-28 | M | HA area/device-class commands resolve to the concrete allowlisted entity set; execution is per-entity with an honest spoken result on partial failure (ADR-018). |
| FR-29 | S | User news-interest profile (topics/sources) resolving topicless "what's the news" into concrete headline queries; with no profile, ask once and offer to remember (ADR-019). |
| FR-30 | M | Contested/political/conflict news is summarized with claims attributed to sources, even-handed framing, and no sensationalized graphic detail (ADR-020). |
| FR-31 | S | Product/recommendation card type; recommendations are ranked only by fit and source quality and are never monetized (no affiliate/sponsored placement) (ADR-021). |
| FR-32 | S | "What is this song/what's playing" answered from MPRIS metadata as a spoken answer plus a now-playing card (title/artist/album/art if available) — a first-class query, not just the passive media bar (ADR-022). |
| FR-33 | M | Timers, alarms, and one-shot reminders: deterministic zero-LLM set/query/cancel, persisted across restart, audible alert + TTS announcement + live countdown card, voice dismiss/snooze; missed alarms announced on restart (ADR-023). |
| FR-34 | S | Named lists and quick notes: add/remove/check-off/read via deterministic grammar, list card with tap/voice check-off, promotable to artifacts (ADR-024). |
| FR-35 | S | Calendar via CalDAV: reads R0 (agenda card + spoken summary), event create/modify R2 with exact event in the approval; calendar data sensitivity-labeled personal context (ADR-025). |
| FR-36 | S | Outbound messages via one SMTP adapter completing the R2 send_message approval flow (verbatim recipient/subject/body in approval, idempotent send). Inbox/message reading deferred to v2 (ADR-026). |

> **Explicit exclusion:** Netflix has no public API; no search, browse, or deep
> integration is in scope (ADR-012). Generic MPRIS control and window focus incidentally
> apply to anything playing in the media window — that is hand-off, not integration.
>
> **FR-25 note:** this closes a real gap found in review — the HUD design examples
> ("who is this", restaurant search, weather) all implied data sources that were never
> specified as tools. Claude CLI's built-in web tools are deliberately disabled for the
> reasoning profile (ADR-004); FR-25 is the sanctioned replacement path, not a
> reopening of that boundary — it is one more catalogued, policy-governed tool like any
> other, not ambient model access to the internet.

## 3. Non-functional requirements

| ID / quality | Measurable expectation |
|---|---|
| NFR-01 Security | Default-deny tool permissions; no unrestricted host shell; secrets never enter prompts unless explicitly scoped. |
| NFR-02 Privacy | Session and memory data remain local by default; provider-bound context is visible and policy-filtered. |
| NFR-03 Latency | Immediate UI ack (<100 ms); first useful text <1.5 s for simple local actions, <3 s for cloud reasoning (excluding provider outages). |
| NFR-04 Voice | Final transcript <0.8 s after end-of-speech; first audio <1.2 s after response text begins, on reference hardware. |
| NFR-05 Reliability | Process restart loses no committed sessions, approvals, artifacts, or scheduled tasks. |
| NFR-06 Offline | Core UI, history, R0/R1 local tools, Home Assistant control (rule-based intents), and memory retrieval continue without internet; LLM-needing runs queue visibly (degraded mode per ADR-011). |
| NFR-07 Auditability | Every side effect links to user input, plan step, policy decision, model run, and tool result. |
| NFR-08 Maintainability | Domain crates do not reference provider SDKs, web frameworks, or OS-specific implementations. |
| NFR-09 Extensibility | New model/tool adapters can be added without modifying the orchestration state machine. |
| NFR-10 Portability | Core runs as a systemd service or container; client contracts are transport/version tolerant. |
| NFR-11 Accessibility | Keyboard-first operation, visible focus, captions/transcripts, non-voice alternatives. |
| NFR-12 Resource control | Per-run token, time, tool-call, process, memory, and artifact-size budgets. |
| NFR-13 Recovery | Idempotency and compensating actions for retries; clients resync after event gaps. |
| NFR-14 Observability | OpenTelemetry traces/metrics plus immutable high-value audit events. |
| NFR-15 Footprint | `jarvisd` idle RSS <100 MB excluding model servers; cold start to healthy <2 s (new in v2 — always-on daemon on a personal machine must be invisible in `htop`). |

> **Latency targets are engineering budgets, not promises.** Establish a reference machine
> and fail the release gate when p50/p95 budgets regress.

## 4. Hardware and platform sizing

| Profile | Suggested baseline | Enables |
|---|---|---|
| **Ultrabook v1 (validated target)** | ~3-year-old U-class laptop CPU (4P+ cores), **8 GB RAM design floor** (16 GB comfortable), iGPU, 30 GB disk | Everything through M4; M5 voice with relaxed STT budget (§4.1). No local reasoning model — already excluded by ADR-011, which is precisely why this profile works. Because the host may downgrade to 8 GB, the low-power rules in `09` §5 (worker serialization, embeddings idle-unload, OTel collector off, zram) are **defaults, not opt-ins**; peak budget on 8 GB is ~2 GB above desktop baseline, enforced by the perf gate. |
| Minimum prototype | 4+ cores, 16 GB RAM, 30 GB disk, decent mic/headphones | Claude CLI, text UI, PostgreSQL, basic tools, CPU embeddings. |
| Recommended desktop | 8+ cores, 32 GB RAM, 100 GB disk, GPU with ~12 GB VRAM | Headroom + future local reasoning model when ADR-011 revisit triggers fire. |
| Whole-house later | Always-on host, wired network, room satellites with echo-controlled audio | Continuous availability, distributed voice/display endpoints. |

### 4.1 Ultrabook resource budget (normative for NFR-15 verification)

Steady-state RSS above desktop baseline, enforced as a performance-gate assertion:

| Component | Idle | Peak |
|---|---|---|
| `jarvisd` | 40–80 MB | ≤120 MB |
| PostgreSQL + pgvector (small tuning, `09` §6) | ~150 MB | ~250 MB |
| 2× Chromium app-mode clients | 400–700 MB | ~900 MB |
| Claude CLI (single-flight, transient) | 0 | 250–450 MB |
| fastembed embeddings (lazy-load, idle-unload) | 0 | ~300 MB |
| Playwright worker (transient) | 0 | 400–600 MB |
| Voice VAD/STT/TTS (resident when enabled) | — | 250–400 MB |
| **Total** | **~0.7–1.0 GB** | **~2.5–3 GB** |

Rules that make the budget hold: single-flight CLI (never stacked model processes);
embeddings lazy-loaded and unloaded after idle; Playwright and voice never required
concurrently with a coding-profile CLI run (scheduler serializes); OTel collector optional
on low-power hosts (`09` §6). CPU is ~0% idle; the only sustained burst is STT
(~1–2 s per utterance). Expected M5 deviation on U-class CPUs: final-transcript latency
~1–1.5 s with faster-whisper `base` int8 — either accept a relaxed NFR-04 or select
`tiny` int8; record the choice at the M5 gate.

## 5. Acceptance definition for v1

1. A fresh install starts core, database, UI, and configured model adapters from
   documented commands.
2. The user can ask a question, see streamed output, invoke a safe tool, approve a
   consequential tool, cancel a run, and review the complete trace.
3. Claude CLI failure automatically produces a clear state and uses/offers the local
   fallback without corrupting the session.
4. A generated artifact is versioned, rendered, reopened after restart, and linked to its
   source run.
5. No tool can run outside its granted scope in the automated security suite.
6. The system controls at least one allowlisted Home Assistant entity and performs one
   push-to-talk voice round trip (M5 gate).

## 6. Requirements traceability

| Requirement | Design area | Milestone | Verification |
|---|---|---|---|
| FR-01/02/06 | Conversation, gateway, orchestrator (`02` §4–5) | M1 | Streaming/cancel/restart E2E |
| FR-03/11/12 | Model gateway + adapters (`02` §5, `05` §4) | M1/M4 | Provider-failure golden traces |
| FR-04/05/07 | Tools, policy, approvals, audit (`06`) | M2 | R0/R2/adversarial tool tests |
| FR-08 | Artifacts (`02` §6, `04` §4) | M3 | Version/create/reopen/restart E2E |
| FR-09/10 | Angular shell + desktop agent (`02` §8) | M1/M3 | Multi-surface + Hyprland contract tests |
| FR-13 | Voice pipeline (`02` §9) | M5 | Latency/barge-in golden trace |
| FR-14 | Home Assistant adapter (`02` §10) | M5 | Allowlist/entity-resolution tests |
| FR-15 | Browser worker (`02` §8) | M3 | Isolated profile + injection tests |
| FR-16 | Memory (`02` §7) | M4 | Provenance/forget/retrieval tests |
| FR-17 | Automations (`02` §11) | M5+ | Policy re-eval / missed-run tests |
| FR-18 | Generated-app sandbox (`06` §6) | M6 | CSP/capability-denial/escape tests |
| FR-21 | Spotify adapter (`02` §11a, ADR-012) | M5 | OAuth/scope/volume-cap/device tests |
| FR-23 | HUD backgrounds + glass tokens (`12` §5) | M3 | Contrast audit on worst-case wallpapers |
| FR-24 | Panel lifecycle (`12` §4) | M1/M3 | Shelve/restore/dismiss/TTL/approval-exempt tests |
| FR-22 | MPRIS + media window (`02` §11a) | M3/M5 | MPRIS contract + cast flow tests |
| FR-25 | Web search + fetch tool (`02` §11b, ADR-014/016) | M2 | Injection/untrusted-content + attribution + ambiguity-clarification tests |
| FR-26 | Location provider (`02` §11c, ADR-015) | M2 | Location resolution order + sensitivity-labeling tests |
| FR-27 | Deep-dive continuity + cards + promotion (`12` §2.3/§2.5, ADR-017) | M3 | Continuation-vs-new-topic + gallery-attribution + promotion tests |
| FR-28 | HA area→entity resolution (`02` §10, ADR-018) | M5 | Multi-entity expand + partial-failure spoken-result tests |
| FR-29 | News-interest profile (`02` §11d, ADR-019) | M4 | Profile-resolution + ask-once-and-remember tests |
| FR-30 | Contested-news framing (`02` §11d, ADR-020) | M2 | Attribution/even-handedness synthesis tests |
| FR-31 | Product card + no-monetization invariant (`12` §2.3, ADR-021) | M3 | Card render + affiliate-absence audit |
| FR-32 | Now-playing query + card (`12` §2.3, ADR-022) | M5 | MPRIS-metadata-to-card + spoken-answer tests |
| FR-33 | Timers module (`02` §11e, ADR-023) | M3 | Set/fire/persist/missed-alarm + zero-LLM tests |
| FR-34 | Lists/notes (`02` §11e, ADR-024) | M3 | Grammar add/check-off + card + promotion tests |
| FR-35 | CalDAV adapter (`02` §11e, ADR-025) | M4 | Two-provider read/create + sensitivity tests |
| FR-36 | SMTP send (`02` §11e, ADR-026) | M4 | End-to-end approval→send + idempotency tests |
| FR-19 | Device/node identity (`06` §2) | M7 | Pair/revoke/reconnect tests |
| NFR-01/02/07/12 | Policy, identity, audit, budgets | All | Security release suite |
| NFR-03/04/14/15 | Tracing + performance harness | M1 onward | p50/p95 + footprint regression gates |
| NFR-05/06/13 | Persistence, fallback, idempotency | M1/M2 | Chaos and offline tests |
| NFR-08/09 | Crate boundaries, ports & adapters | All | `cargo xtask arch-test`, `cargo deny` |
