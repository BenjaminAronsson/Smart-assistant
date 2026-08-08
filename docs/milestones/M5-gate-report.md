# M5 "Voice, home & media" — Gate Report

**Status: ✅ SIGNED OFF 2026-08-09 (owner-approved).** Prepared 2026-08-08. Milestone loop
docs/11 §2. All eight features are implemented, tested and merged to `main`; all gate runs
are green (§2); both security passes are complete with **no BLOCKING findings** (§4.2);
the repeatable exit-evidence scenarios live in `docs/milestones/M5-acceptance.md`.

**Owner decisions, 2026-08-09 (all three as recommended):** D-M5-3 accepted as a
carryforward — the NFR-04 figure and the STT model-size choice close on reference
hardware, per docs/02 §9's own "do not block the milestone on hardware". D-M5-2 kept
as-is: barge-in stops the speech, not the run — a user who interrupts wants silence, not
to lose an answer still readable on the HUD. D-M5-4 accepted as a carryforward, to be
scheduled **before any further physical-effect tools land**.

Scope since `m4-complete`: **15 commits** on `main`.

---

## 1. Exit evidence (docs/08 §1, M5 row) → result

Every scenario below is runnable via `cargo xtask golden`, fixture-driven per CLAUDE.md
("fixture-driven tests over live-provider calls, always"): **no live Wyoming, Home
Assistant, Spotify, MPRIS bus, or model quota**. Everything above the transport seam —
the orchestrator, `policy::evaluate`, the approval gate, the grant store, the hash-chained
audit log, and each adapter's own client and executor — is production code running for
real against live Postgres. Full mapping in `M5-acceptance.md` §2.1.

| # | Exit-evidence item | Result | Evidence |
|---|---|---|---|
| 1 | **Full voice round trip within NFR-04** | ⚠️ **Functionally met; numerically NOT met** | The round trip works and is locked in (`evidence1_…`): PTT PCM over the authenticated WS → the real Wyoming client → transcript → a run started through the **same** `RunApi::start_turn` a typed message uses → answer → clause-segmented TTS back. The transcript is committed as an ordinary user message, which is what proves voice took no shortcut. **The NFR-04 latency figure is not claimed** — see D-M5-3. |
| 2 | **Safely control one allowlisted HA entity** | ✅ MET | Three scenarios. R1: policy → auto-authorize → execute → audit chain verifies. The *"safely"* half: a non-allowlisted light is refused and the transport never even **reads** it, so a proposal cannot be used to probe the house. R2 (`home.execute_scene`): parks at `WaitingApproval`, approval id read back from the persisted card, resolved through the real gate, exactly one grant minted **and consumed**. |
| 3 | **"pause the music" with zero LLM calls** | ✅ MET | `FakeModel::opened() == false` — the assertion the bullet actually makes, not "the right text came back". Reaches the player as `Pause` and **exactly** `Pause`, still via `PolicyReview` (recognition is not authorization), audited, run `Completed`. |
| 4 | **Play a searched Spotify track on a chosen device** | ✅ MET | R0 search feeds R1 play; the request carries the searched URI **and** the named `device_id` — the device the caller chose, not wherever playback happened to be. |
| 5 | **"play ABBA" → shuffled top tracks, no clarification** | ✅ MET | Exact call sequence `GET /search` → `shuffle?state=true` → play with the **artist's** `context_uri` (ADR-022). The observation contains no `?`: the common case asks nothing. |
| 6 | **"play playlist X" resolves the owner's library first** | ✅ MET | A saved and a public playlist share a name; `GET /me/playlists` then play, and the public catalogue is **never consulted**. |
| 7 | **"what's playing" → now-playing card (FR-32)** | ✅ MET | Zero model calls, and driven with **`tools: None`** — an observation must be answerable with no tool authority at all. Exactly one card, `action = Extend`. Nothing on the audit chain, because nothing happened in the world. |
| 8 | **Plural area command reports partial failure honestly (FR-28)** | ✅ MET | Three lights, **one seeded to fail**. All three attempted (a failure does not abort the rest), a light in another area untouched, result reads "2 of 3", names the survivors *and* the failure, and does not say "all 3". An all-succeed path would have proven nothing, which is why the failure is seeded. |
| 9 | **Golden 9** | ✅ MET | Registered in `cargo xtask golden`; six sub-scenarios (9a–9f) proving TTS, model and tool cancellation at the three seams that own them. See D-M5-2 for an honest scope note. |

**8 of 9 fully met; item 1 is functionally met and numerically unmet (D-M5-3).**

---

## 2. Gate runs

All against live Postgres/pgvector.

| Gate | Result |
|---|---|
| `cargo fmt --check` | ✅ clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ clean |
| `cargo test --workspace` | ✅ **1131 passed, 0 failed** |
| `cargo xtask arch-test` | ✅ 9 crates, dependency rules hold |
| `cargo xtask codegen --check` | ✅ generated outputs up to date |
| `cargo xtask golden` | ✅ **33 named scenarios** (was 17 at M4) |
| `cargo xtask perf --rss` | ✅ cold start **0.053 s** (budget < 2 s); idle RSS **21.3 MB** (budget 40–80 MB typical, 120 MB ceiling) |
| `cargo xtask perf --voice` | ✅ daemon-side overhead: transcript leg p95 **8.3 ms**, first-audio leg p95 **10.1 ms** — **excludes model time**, see D-M5-3 |
| web: `ng lint` / `ng build` / `ng test` | ✅ clean / clean / **247 passed** |

Note: the sqlx `#[sqlx::test]` harness shows a pre-existing ~30 s run-to-run alternation
from pool contention (child pools under a master pool capped at 20, 30 s acquire timeout).
It reproduces on unmodified `m4-complete`, affects every jarvisd test binary, and is not
an M5 defect. The *failures* it used to cause were fixed in `de35e09` (each harness's
outbox dispatcher now gets its own pool rather than holding a `PgListener` connection
from the shared one).

---

## 3. What M5 shipped

Voice: provider-neutral VAD/STT/TTS ports + a real Wyoming protocol client; browser
push-to-talk; the full round trip with clause-segmented TTS and barge-in; a `voice.error`
event so a dead speech service is distinguishable from silence.

Home Assistant: a curated tool layer only — `get_state` (R0), `set_light` (R1),
`set_area_lights` (R1, area→entity with honest partial failure), `execute_scene` /
`run_script` (R2) — never the whole HA service namespace.

Spotify: search / play / play_playlist / queue_add / volume (R1) plus a separate
`volume_boost` (R2). No library-mutation authority exists at all, by construction.

Deterministic grammar: recognized commands now emit a `ToolProposal` and take the
identical policy → approval → grant → execute → audit path a model-proposed call takes;
"what's playing" answers as text with zero model calls.

---

## 4. Review passes

### 4.1 perf-warden equivalent

`cargo xtask perf --rss`: 21.3 MB idle, 0.053 s cold start — an order of magnitude inside
the docs/01 §4.1 budget, and essentially unchanged from M4 (20.4 MB) despite the voice,
HA and Spotify surfaces, because all three are opt-in and none is resident when
unconfigured. `perf --voice` was added this milestone for the daemon's share of NFR-04.

### 4.2 security-auditor

Two passes over the milestone. **Neither found a BLOCKING issue.**

**Pass 1** (first half of the milestone): no BLOCKING, five SHOULD-FIX — **all five fixed
and committed**: the voice socket wedge and its unbounded teardown, an orphaned synthesis
task, unvalidated `stream_id`/audio-format fields, the Spotify grant expiry/resource
check, and a wrong-device volume undo.

**Pass 2** (whole milestone, `m4-complete..HEAD`): **no BLOCKING**, four SHOULD-FIX.
Three are fixed (S1, S3, S4 below); one is carried with an owner decision (S2 → D-M5-4).

- **S1 — a partial physical effect could be reported as a clean failure. FIXED.**
  `home.set_area_lights` shared one 10 s wrapper timeout across a fan-out that makes two
  HA round trips per entity for up to 16 entities. On a slow HA the wrapper dropped the
  whole future mid-loop: lights already switched stayed switched, the "2 of 3" sentence
  was discarded, and the audit recorded `tool.failed` — telling the owner nothing happened
  while half the room was lit. Exactly what FR-28's partial reporting exists to prevent,
  and it made the append-only audit misleading about a real physical side effect.
- **S3 — the secret-rejection error still echoed a prefix of a pasted secret. FIXED.**
  The earlier fix (`d882408`) closed the no-colon case; a password *containing* a colon
  (`Summer:2026!`) still leaked `Summer` to journald.
- **S4 — the D-M5-1 safety argument rested on an undocumented invariant. FIXED.**
  `report()` echoing the executor's result is safe only because the grammar's verdict is
  identical on turn 1 and on the replan. That holds today, but a *dynamic*
  `LightTargetResolver` (config reload, HA-backed lookup, cache warm-up) returning `None`
  then `Some` would cause fetched web content to be spoken verbatim as the assistant's own
  answer. Now stated on the trait and pinned by a flip-flop-resolver regression test.
- **S2 → carried as D-M5-4** (§5): tool-execution audit rows carry no argument binding.

**What pass 2 verified clean**, with reasoning, since it is the substance of this gate:
`orchestrator.rs:712` is still the *only* production call site of `ToolExecutor::execute`,
so a grammar-produced `ToolProposal` takes the identical policy → approval → grant →
execute → audit path — recognizing speech grants nothing. Injection can neither
manufacture, widen, **nor suppress** a proposal: classification sees only the pre-marker
slice (the owner's own words on every turn, since both memory and tool frames are
appended *after* it), and "has a tool already run" is answered by `prior_tool_result`,
which grep confirms has exactly one construction site and one write site, making it
unforgeable by content. `ConfiguredLightTargets` resolves only *downward* into the
allowlist — there is no code path that constructs an entity id from an utterance. The
area fan-out filters the allowlist by HA metadata rather than enumerating HA and checking
afterwards, so a non-allowlisted entity is structurally unreachable; the bound and the
zero-match case are refused before any mutation; and the per-entity undo is composed from
each entity's own pre-read, so a light already on is restored to on. Both grant fixes are
strictly *stronger* (Spotify gained expiry + resource; HA lost nothing), and the entity
stays bound via `normalized_args_sha256`. Secrets: neither adapter config derives `Debug`,
error variants are fixed strings, HA refuses non-HTTPS and forbids redirects (token
re-send to another origin). Crash recovery was checked specifically for this milestone:
`prior_tool_result` is in-memory, so recovered runs re-drive with `device = None`, leaving
the tool stack unwired and failing at `PolicyReview` — a deterministic command **cannot
re-fire a physical effect across a restart**.

### 4.3 Defects found and fixed during the milestone

Recorded because each passed review or tests before being caught, and the pattern matters
more than the individual bugs:

1. **Every approved R2 Home Assistant action would have been denied in production.** The
   orchestrator mints a grant's `target_resource` from the *tool id*; the executor
   compared it against an *entity* string; `ResourcePattern::matches` without a `*` is
   exact equality. The owner approves, `PgGrantStore` burns the single-use grant, and the
   action still does not happen — a retry needs a fresh approval.
2. **The same class of bug in Spotify** (checked a resource the orchestrator never mints).
   Both adapters' tests passed only because their fixtures built grants *the adapter's*
   way rather than the orchestrator's. Both now assert that a grant minted exactly as the
   orchestrator mints it is **accepted** — the test that was missing in both.
3. **The config secret-rejection error leaked the secret it was rejecting.** It promised
   "the rejected value is withheld from this message" while echoing "everything before
   the first `:`" — which is the *entire value* when there is no colon, as in most raw
   tokens (JWTs, API keys). Invariant 5, in the guard designed to protect invariant 5.
4. **A deterministic command re-fired its effect on every replan turn** (D-M5-1) — one
   "turn on the lamp" would have driven eight real service calls at a physical lamp.
   Fixed structurally (`ModelRequest::prior_tool_result`, written only by the
   orchestrator) rather than by sniffing the prompt for a marker that attacker-influenced
   memory can itself contain.

The common thread in 1, 2 and 4: **a component's self-consistent fixtures can hide a total
mismatch with its real caller.** Worth carrying into future gates when a coverage claim
looks reassuring.

---

## 5. Deviations requiring an owner decision

**D-M5-3 — evidence #1's NFR-04 latency figure is unmet, not approximated.**
NFR-04 budgets an end-to-end voice round trip (docs/01 §4.1: 0.8 s to transcript, 1.2 s to
first audio). That figure is dominated by **model** time — faster-whisper and Piper — on
the **reference machine**. This host has neither; every voice scenario runs against
fixture Wyoming services that answer instantly, so a latency measured against them
measures the harness, not the pipeline. What is repeatable here is the daemon's own share:
transcript leg p95 8.3 ms, first-audio leg p95 10.1 ms, both far inside their overhead
budgets, with model time entirely on top. **To close this properly**, on the reference
machine with real Silero/faster-whisper/Piper: record end-to-end transcript and
first-audio latency, and record the STT model-size decision (`base` vs `small` int8) that
docs/08 §6 explicitly defers to this milestone, including whether the CPU-only 0.8 s
budget needs relaxing. Requested disposition: **accept as a carryforward** to be closed on
reference hardware, in the same spirit as M4's D-M4-1 — or reject and hold the gate until
hardware exists.

**D-M5-2 — barge-in cancels synthesis but not the in-flight run.** Speaking again stops
the utterance (reported as `cancelled`, not silence) but the run that produced it keeps
streaming; its text simply stops being spoken. The model- and tool-cancellation halves of
docs/07 §2 item 9 are proved at their own seams (9c–9f) rather than through the voice
interrupt. This is behaviour, not a test gap. Requested disposition: **owner decides**
whether interrupting speech should also cancel the run. Arguments both ways: cancelling
matches "stop means stop" and saves quota; not cancelling means a long answer the user
interrupted is still completed and persisted, which is what they would want if they only
wanted the *speaking* to stop.

**D-M5-4 — tool-execution audit rows carry no argument binding.** `tool_audit_event`
(`orchestrator.rs:814-828`) records `target: "tool:{id}"` with an empty payload. This is
deliberate M2 design — docs/06 §5 keeps arguments out of audit payloads — but F5.4
amplifies it: `home.set_area_lights` can drive up to 16 physical devices, and the durable
record cannot distinguish "the living room" from "the whole floor". The append-only log
therefore cannot answer *"what was actuated"* after the fact, which is most of what
invariant 6 exists for. The audit's suggested fix is to record the already-computed,
non-sensitive `normalized_args_sha256` (it binds the row to the exact approved arguments
without echoing them), plus the matched entity count for the area tool. **Not done here**
because it changes the audit payload for *every* tool, which is a cross-cutting
design decision touching a docs/06 rule rather than an M5 defect. Requested disposition:
**accept as a carryforward**, ideally scheduled before any further physical-effect tools
land.

**D-M5-1 is NOT in this list — it was fixed, not accepted.** Repeatedly actuating physical
hardware is not something to carry as paperwork. Neither is **S1** (a partial physical
effect reported as a clean failure), for the same reason.

---

## 6. Open risks carried forward

- **`broadcast_voice_transcript` fans live microphone transcripts to every connected
  socket**, not just the capturing one. Correct for today's single-owner model (all paired
  devices are the owner's), but when the planned scope differentiation lands
  (`voice-capture`, `display-agent`), transcript delivery should become scope- or
  socket-targeted. Flagged by the first security pass; carry to M7.
- **`InMemorySmtpIdempotencyStore` (M4) remains process-local** — unchanged this
  milestone, still backed by the single-use grant as the real defence.
- **HA is HTTPS-only.** Deliberate (a long-lived bearer token rides every request), but
  many HA installs are plain HTTP on the LAN, so deployment needs TLS in front of HA. This
  will surface the first time the integration is enabled for real.
- **OAuth enrollment is absent for both HA and Spotify** — tokens are consumed as resolved
  secret references; minting the first one is an out-of-band step.

---

## 7. Recommendation

All eight features are complete, 8 of 9 exit-evidence items are fully met, every gate run
is green, and both security passes found **no BLOCKING issues**. Of the nine SHOULD-FIX
items raised across both passes, **eight are fixed**; the ninth (D-M5-4) is a cross-cutting
audit-payload design decision, not an M5 defect.

Every defect found during the milestone that could produce a **wrong physical effect or a
dishonest report of one** was fixed rather than carried — the eight-fold re-firing
(D-M5-1), the R2 home grants that silently denied every approved action, and the fan-out
timeout that reported a half-lit room as a clean failure.

**APPROVED WITH DEVIATIONS** (D-M5-2, D-M5-3, D-M5-4) — owner sign-off 2026-08-09.

Proceeding: tag `m5-complete`, update the docs/08 §1 roadmap row, push. The
reference-hardware NFR-04 measurement and the D-M5-4 audit binding are carried forward;
the latter should be scheduled before the next physical-effect tool.
