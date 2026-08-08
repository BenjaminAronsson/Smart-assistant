# M5 Voice, home & media — feature list

Status: **✅ COMPLETE — signed off 2026-08-09** (tag `m5-complete`; deviations D-M5-2/3/4
accepted). All eight features merged to `main`; see `docs/milestones/M5-gate-report.md`.

Original status line follows.

Status: **PROPOSED — awaiting owner approval.** Decomposed 2026-08-06 (milestone loop,
docs/11 §2), on the session's active model (Sonnet 5) — **CLAUDE.md's model-strategy
section reserves milestone decomposition for the strongest available model (Fable 5, or
Opus)**; this draft was flagged as a deviation from that policy when produced. Nothing
below has been implemented; do not begin any feature until the owner approves this list
(or asks for a re-decomposition on a stronger model first).

M4 signed off 2026-08-06 (`docs/milestones/M4-gate-report.md`, tag `m4-complete`, merged
to `main`). This is the first M5 decomposition attempt.

Milestone scope (docs/08 §1): push-to-talk, VAD/STT/TTS via Wyoming, barge-in, HA
state/actions with an allowlist, Spotify adapter + voice transport commands routed
through the deterministic grammar M4 built. **Cast-a-link (the web-video half of
FR-21/22) and the MPRIS control-plane adapter itself are already done — M3a
(`m3-f3a.7-plan` memory, PR #28)** — M5's media work is specifically the Spotify Web API
adapter and the "what's playing" query (FR-32), not the media window/cast flow.
Wake-word and room attribution are explicitly **not** in scope (docs/02 §9: "push-to-talk
→ barge-in → wake word → room attribution" is the milestone order; docs/08 §6 defers
wake-word engine selection to post-M5).

Exit evidence (docs/08 §1, M5 row): **(1)** full voice round trip within NFR-04; **(2)**
safely control one allowlisted HA entity; **(3)** "pause the music" works with zero LLM
calls; **(4)** play a searched Spotify track on a chosen device; **(5)** "play ABBA"
starts shuffled top tracks with no unnecessary clarification; **(6)** "play playlist X"
resolves the user's own library first; **(7)** "what's playing" answers with a
now-playing card (FR-32); **(8)** a plural area command ("turn on the living room lamps")
resolves to multiple entities and reports partial failure honestly (FR-28); **(9)**
golden 9.

Each feature is a vertical slice sized for one session and runs the `/feature` loop
(spec → threat note → contracts/tests first → implement → review → DoD → small PR).
"Read" names the exact spec sections for that session (token discipline, CLAUDE.md).

**Model discipline (CLAUDE.md §"Model strategy").** Anything touching
`jarvis-domain`/`jarvis-application` (voice-session state, barge-in cancellation wiring
into the orchestrator, HA domain types, Spotify domain types, the deterministic-grammar
extension for transport commands) is **strong-model** work. New out-of-process adapter
plumbing with a fixed external protocol (the Wyoming client, the HA REST/WebSocket
client, Spotify's OAuth/API client) is more tightly constrained by an external spec and
**may** run on Sonnet — owner decides per session, same latitude as M3's adapter work.

**Invariants that bite in M5.** Invariant 1 (text never grants authority) now extends to
**voice transcripts**: a final STT transcript is untrusted input exactly like typed text
— it must route through the same deterministic-grammar-first / policy-gated path M4
built, never a shortcut from "the microphone heard it" to a tool call. HA control is a
genuinely new *physical* side-effect class (lights, scenes, scripts) — every HA tool
needs real `ToolPolicy` risk tiers from its first commit (curated tool layer only, per
docs/02 §10 — never the whole HA service namespace). Invariant 4 (cancellable) matters
acutely for barge-in: TTS playback must stop within the interrupt-response budget, not at
the next clause boundary. Invariant 5 (no secrets in logs) extends to the Spotify OAuth
refresh token and the HA long-lived access token — both keyring-resolved at the adapter
boundary, same pattern as the M4 SMTP/CalDAV credentials.

**Reference-hardware decision points explicitly deferred to this milestone (docs/08
§6):** exact STT model size (`base` vs `small` int8) and whether the CPU-only NFR-04
budget (0.8s transcript) needs relaxing or a smaller model — decide once there is a real
reference machine to measure on, not in the abstract; record the choice at the M5 gate,
per docs/02 §9's own instruction ("do not block the milestone on hardware").

---

## Phase A — Voice pipeline foundation (exit evidence #1)

- [x] **F5.1 — Wyoming client ports + VAD/STT/TTS adapters (application + adapters)** · *strong model (ports) / adapter plumbing may be Sonnet*
  `jarvis-application::ports`: provider-neutral `VoiceCapture`/`Transcriber`/`Speaker`
  traits (or one `VoicePipeline` port, TBD in the feature spec) so the domain never knows
  it's Wyoming. `jarvis-adapters`: a Wyoming protocol client (out-of-process services per
  docs/02 §9 — Silero VAD, faster-whisper/whisper.cpp STT, Piper TTS), each independently
  swappable. Push-to-talk capture: resolve during the feature spec whether audio capture
  is a `jarvis-agent` responsibility (OS-level mic access, matching its existing narrow
  allowlisted-command role, docs/02 §8) or a browser-side `getUserMedia` stream into
  jarvisd over WS — this is an open design question this list does not presume the answer
  to. Partial + final transcript events flow to the orchestrator like any other run input.
  Refs: FR-13, docs/02 §9, ADR-007, ADR-011. Read: docs/02 §9, §8 (if capture is agent-side),
  §12 (deployment topology); skill `provider-adapter` (adapter-boundary conventions
  transfer even though this isn't a `ModelProvider`). Deps: none (first M5 slice).
  security-auditor (new OS-level mic-access surface if agent-side) + rust-reviewer
  mandatory. **If audio capture surfaces an irreversible OS/protocol choice (e.g. PipeWire
  vs. PulseAudio vs. ALSA direct), stop and draft an ADR** — none of ADR-007/011 commit to
  the capture-side transport, only the STT/TTS/VAD service shape.

- [x] **F5.2 — Barge-in + full voice round trip wired into the orchestrator; NFR-04 latency measurement** · *strong model*
  Wire VAD end-of-turn → STT final transcript → the existing M4 deterministic-grammar-first
  routing → (LLM run if unresolved) → TTS response, with barge-in (new audio interrupts and
  cancels in-flight TTS playback via the existing `CancellationToken` plumbing, invariant 4
  — no new cancellation mechanism). **Exit-evidence #1:** a full voice round trip measured
  against the NFR-04 budget on the actual reference hardware; if the CPU-only budget
  (faster-whisper `base`/`small` int8 transcript latency) misses 0.8s, this session
  records the model-size/budget decision (docs/08 §6) rather than blocking. Consider
  reusing the `cargo xtask perf` pattern (built at the M4 gate) for a repeatable latency
  measurement rather than an ad hoc one-off. Refs: FR-13, NFR-04, docs/02 §9. Read: docs/02
  §4 (orchestrator/cancellation), §9; skill `state-machine`. Deps: F5.1.

## Phase B — Home Assistant integration (exit evidence #2, #8)

- [x] **F5.3 — HA adapter + curated tool layer + one allowlisted entity control (adapters + application)** · *strong model*
  `jarvis-adapters`: HA REST/WebSocket client, dedicated least-privilege long-lived token
  (keyring-resolved), entity/area metadata caching (HA remains authoritative — cache is
  advisory, never the source of truth on a stale read). Curated tools only —
  `home.get_state`, `home.set_light`, `home.execute_scene`, `home.run_script` — never the
  whole HA service namespace (docs/02 §10). Real `ToolPolicy` risk tiers from the first
  commit: reads R0, a single named-entity mutation R1 with an allowlist, scenes/scripts
  R2 (broader blast radius, approval with a diff). **Exit-evidence #2:** safely control one
  allowlisted entity end to end (approval where required → grant → execute → audit).
  Refs: FR-14, docs/02 §10, ADR-006. Read: docs/02 §10; docs/06 §3 (risk tiers) — re-read,
  don't assume M4's tiers transfer unchanged; skill `policy-grants`. Deps: none (parallel
  to Phase A). security-auditor mandatory (new physical-effect tool class) + rust-reviewer.

- [x] **F5.4 — HA area→entity resolution + honest partial-failure reporting (application)** · *strong model*
  Area/device-class commands ("turn on the living room lamps") resolve to the concrete
  allowlisted entity **set**, not a single entity; execution is per-entity, and a partial
  failure (2 of 3 lamps succeeded) is reported honestly in the spoken/card result, never
  silently swallowed or falsely reported as full success. Refs: FR-28, ADR-018. Read:
  ADR-018 in full, docs/02 §10. Deps: F5.3. rust-reviewer mandatory (this is exactly the
  kind of multi-entity partial-failure state a transition-table test should pin down).

## Phase C — Voice-routed commands, Spotify, now-playing (exit evidence #3, #4, #5, #6, #7)

- [x] **F5.5 — Voice/text transport commands through the deterministic grammar (application)** · *strong model*
  Extend M4's deterministic grammar (`crates/jarvis-application/src/home.rs` and
  sibling modules) to recognize media-transport phrasing ("pause the music", "skip",
  "turn off the kitchen lights") and route it to the **already-existing** MPRIS
  (`media.playback`, M3a) and HA (F5.3) tools — zero LLM calls for the recognized case,
  same policy/grant path as any other tool call (no shortcut from "recognized speech" to
  execution, invariant 1). **Exit-evidence #3:** "pause the music" works with zero LLM
  calls, from both text and voice input. Refs: FR-13 (routing), the M4 grammar work. Read:
  `crates/jarvis-application/src/home.rs` + `deterministic.rs` (current state, this
  milestone's own code) docs/03 §4 "quota-first routing". Deps: F5.2 (voice input must
  exist to route from), F5.3 (HA tools to route to) — MPRIS routing alone could start
  once F5.2 lands, without waiting on F5.3.

- [x] **F5.6 — Spotify adapter: search/play/queue/volume-cap + artist/playlist resolution (adapters + application)** · *adapter plumbing may be Sonnet; policy/domain types strong model*
  `jarvis-adapters::spotify`: OAuth authorization-code + PKCE, refresh token in the
  keyring (never logged, docs/06 §5 pattern from M4's SMTP/CalDAV credentials). Tools:
  `spotify.search`, `spotify.play` (uri + Connect device), `spotify.play_playlist { name }`,
  `spotify.queue_add`. Premium-required detection surfaced, not assumed. Playback R1 with
  a config volume cap (reuse the M3a media-tool volume-cap pattern — it needed two tools
  because `policy::evaluate` ignores arguments, per the `m3-f3a.7-plan` memory; the same
  constraint applies here); playlist mutation R2. Artist-only resolution defaults to
  shuffled-top-tracks with no clarification; `play_playlist` resolves the user's own saved
  playlists first, public search only as fallback (ADR-022). **Exit-evidence #4/#5/#6.**
  Refs: FR-21, ADR-012, ADR-022. Read: ADR-012, ADR-022 in full, docs/02 §11a; skill
  `media-integration`. Deps: none structurally, but natural to sequence after F5.5 so
  voice-routed play commands have a grammar path to land in.

- [x] **F5.7 — "What's playing" now-playing query + card (application + contracts + jarvisd)** · *may be Sonnet (tightly spec'd by ADR-022 + docs/12)*
  A first-class query (not just the passive media bar) answered from the same MPRIS
  metadata: spoken answer + now-playing card (title/artist/album, art if available).
  Multi-player ambiguity asks via the ADR-016 fluent single-question pattern (already used
  in M2's contested-topic/clarifying-question work) — never a picker. **Exit-evidence #7.**
  Refs: FR-32, ADR-022, docs/12 §2.3. Read: ADR-022, ADR-016 (the fluent-question pattern
  to reuse), docs/12 §2.3; skill `media-integration`. Deps: F5.6 (Spotify's now-playing
  metadata may also feed this, if a Spotify session is the active player — confirm in the
  feature spec whether MPRIS alone suffices or Spotify API state is also needed).

## Phase D — Integration lock-in

- [x] **F5.8 — Golden 9 + acceptance scenarios + latency trace lock-in** · *may be Sonnet*
  `cargo xtask golden` scenario 9 (docs/07-testing.md §2) covering the full voice round
  trip, HA area command with partial failure, and a Spotify play command, against fixture
  adapters (per CLAUDE.md: fixture-driven tests over live-provider calls, always). Latency
  traces captured and locked in before declaring the milestone done (docs/08 §2 discipline
  — this is the same principle M0–M4 followed at golden 1–7). Records the reference-hardware
  STT model-size decision (§ above) in the gate report rather than leaving it implicit.
  Refs: docs/07-testing.md §2, docs/08 §2. Deps: F5.2, F5.4, F5.5, F5.6, F5.7 (all prior
  phases feed this).
