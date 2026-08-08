# M5 — repeatable acceptance scenarios (F5.8)

The M5 exit evidence (docs/08 §1, M5 row) as **runnable** scenarios. §2 runs from one
command. §3 states plainly what this repo **cannot** demonstrate and why. §4 records the
deviations the gate must decide on. This file is the checklist the M5 `/gate` walks; it
follows `M3a-acceptance.md` / `M3b-acceptance.md`.

docs/08 §1, M5 row, verbatim:

> Full voice round trip within NFR-04; safely control one allowlisted HA entity; "pause
> the music" works with zero LLM calls; play a searched Spotify track on a chosen device;
> "play ABBA" starts shuffled top tracks with no unnecessary clarification, "play playlist
> X" resolves the user's own library first, "what's playing" answers with a now-playing
> card (FR-32); a plural area command ("turn on the living room lamps") resolves to
> multiple entities and reports partial failure honestly (FR-28); golden 9.

## 1. Prerequisites

```bash
docker compose -f infra/compose/dev.yml up -d postgres
export DATABASE_URL=postgres://jarvis:jarvis-dev-only@127.0.0.1:5432/jarvis
```

Nothing else. **No live Wyoming, no live Home Assistant, no live Spotify, no MPRIS
session bus, no model quota** — every one of those is a fixture at the transport seam,
per CLAUDE.md's "fixture-driven tests over live-provider calls, always". Everything
above the seam (the orchestrator state machine, `policy::evaluate`, the approval gate,
the grant store, the hash-chained audit log, and each adapter's own client and executor)
is production code running for real against live Postgres.

## 2. One command

```bash
cargo xtask golden
```

runs the M1/M2 traces, golden 7, the M3a and M3b acceptance sets, **golden 9**, and the
nine M5 acceptance scenarios below. A filter that matches nothing is a failure, not a
pass — a renamed test cannot silently turn a gate scenario into a no-op.

To run only the M5 half:

```bash
cargo test -p jarvisd --test m5_acceptance
```

### 2.1 Exit evidence → scenario

All nine scenarios live in `crates/jarvisd/tests/m5_acceptance.rs`.

| # | Exit evidence (docs/08 §1) | Scenario | What it actually proves |
|---|---|---|---|
| 1 | Full voice round trip **within NFR-04** | `evidence1_a_full_voice_round_trip_answers_aloud_what_it_heard` | Push-to-talk PCM over the authenticated WS → the real `jarvis_adapters::wyoming` client → a final transcript → a run started through the **same** `RunApi::start_turn` a typed message takes → the streamed answer → clause-segmented TTS back as `voice.speak.start` / binary PCM / `voice.speak.stop{reason:"completed"}`. The durable half: the transcript is committed as an ordinary **user message** on the timeline, which is what proves voice took no shortcut (invariant 1). **The NFR-04 latency figure is NOT claimed — see §3.** |
| 2 | Safely control one allowlisted HA entity | `evidence2_an_allowlisted_light_is_driven_through_policy_and_audit` | The R1 half, through the real authorization path: `PolicyReview` → auto-authorize → execute → audit. `home.set_light` on the allowlisted lamp drives HA exactly once; `policy.auto_authorized` and `tool.executed` are both on the hash chain and the chain verifies; no grant is minted (R1 does not need one). The **"safely"** half is the second drive: a light that is *not* allowlisted is refused and the transport never even **reads** it — the allowlist bites before any HA I/O, so a proposal cannot be used to probe the house. |
| 2 | …and the broad-effect tier | `evidence2_a_broad_home_action_needs_approval_and_a_single_use_grant` | `home.execute_scene` (R2) parks at `WaitingApproval`; the approval id is read back from the **persisted `approval.requested` card** (the same place a real client reads it) and resolved through the real `JarvisApprovalGate`; exactly one grant row is minted for `home.execute_scene` and is marked **consumed**; the scene runs once; `approval.requested`, `approval.resolved`, `policy.approval_requested`, `grant.minted` and `tool.executed` are all on the chain, and the whole chain verifies. |
| 3 | "pause the music" with **zero LLM calls** | `evidence3_pause_the_music_drives_the_player_with_zero_model_calls` | `FakeModel::opened()` is `false` — the assertion the roadmap bullet actually makes. "The right text came back" and "it cost no quota" are different claims; a regression that quietly delegated to the provider would still look correct. The recognized verb reaches the MPRIS player as `Pause` and nothing else, still via `PolicyReview` (recognition is not authorization), and the effect is audited. **Also pins deviation D-M5-1 — see §4.** |
| 4 | Play a searched track on a chosen device | `evidence4_a_searched_track_plays_on_the_chosen_device` | Two drives through the registry: R0 `spotify.search` finds the track, then R1 `spotify.play` starts **that** URI with `device_id=kitchendeviceid0001` on the request — the device the caller named, not wherever playback happened to be. No approval card: playing a track is reversible R1. |
| 5 | "play ABBA" → shuffled top tracks, no clarification | `evidence5_play_abba_starts_shuffled_top_tracks_without_asking` | The exact call sequence is `GET /search`, `PUT /me/player/shuffle?state=true`, `PUT /me/player/play` with the **artist's** `context_uri` — ADR-022 (1). The observation folded back to the model says "shuffled" and contains **no `?`**: the common case asks nothing. |
| 6 | "play playlist X" resolves the owner's library first | `evidence6_play_playlist_resolves_the_owners_library_before_public_search` | A saved playlist and a public one share the name. The call sequence is `GET /me/playlists`, `PUT /me/player/play` — the public catalogue is **not consulted at all** — the owner's URI is what plays, and the answer says "your playlist" (ADR-022 (2)). |
| 7 | "what's playing" → a now-playing card (FR-32) | `evidence7_whats_playing_answers_with_a_card_and_zero_model_calls` | `FakeModel::opened()` is `false`, and the card really reached the HUD canvas: exactly one `Now playing` canvas, `action = Extend` (an aside never shelves work the owner did not put down), carrying the track facts. Driven with **`tools: None`** — the query is an observation, so it must be answerable with no tool authority whatsoever. Nothing is written to the audit chain, because nothing happened in the world. |
| 8 | A plural area command reports partial failure honestly (FR-28) | `evidence8_a_plural_area_command_reports_partial_failure_honestly` | Three lights in `living_room`, **one seeded to fail**, plus an allowlisted light in another area. All three living-room lamps are attempted (the failure does not abort the rest); the kitchen lamp is never touched; and the result reads "2 of 3", names the survivors (`Left lamp`, `Right lamp`) **and** names the failure (`Corner lamp … did not respond`), and does **not** say "all 3". An all-succeed path would prove nothing about this bullet, which is why the failure is seeded. |
| 9 | golden 9 | `cargo xtask golden` | See §2.2. |

The observation asserted in #5, #6 and #8 is read out of the **replan prompt** (the
untrusted-tool-result block the orchestrator folds back into the next turn), not out of
the streamed text. The streamed text is the *scripted* model answer, so asserting on it
would prove nothing about the executor.

### 2.2 Golden 9

docs/07 §2 item 9 is *"Voice response interrupted; TTS, model, and tool cancellation all
correct."* Its three halves are proved at the three seams that own them rather than
re-simulated in one place:

| Sub-scenario | What it proves | Where |
|---|---|---|
| 9a `barge_in_cancels_synthesis_and_stops_the_audio` | The user speaking again cancels the in-flight utterance: `voice.speak.stop{reason:"cancelled"}` is reported (it does not merely fall silent) and **no further audio frame** arrives for it | `crates/jarvisd/tests/voice_round_trip.rs` |
| 9b `a_second_voice_turn_reports_the_utterance_it_supersedes` | A superseding turn ends the previous utterance through `cancel_speech` rather than orphaning its task and its speech-service connection | `crates/jarvisd/tests/voice_round_trip.rs` |
| 9c `cancellation_mid_model_reaches_cancelled_without_orphan` | Model cancellation mid-stream reaches `Cancelled` and drops the provider stream | `crates/jarvis-application/src/orchestrator_tests.rs` |
| 9d `cancellation_ends_the_stream_promptly` | The Wyoming client honours its `CancellationToken` promptly | `crates/jarvis-adapters/src/wyoming.rs` |
| 9e `cancellation_during_a_request_returns_promptly` | An in-flight home tool cancels promptly | `crates/jarvis-adapters/src/home_assistant.rs` |
| 9f `a_pre_cancelled_run_never_reaches_the_transport` | A cancelled media tool never reaches the network | `crates/jarvis-adapters/src/spotify.rs` |

**Scope note, honestly stated:** barge-in cancels **synthesis**. It does not cancel the
in-flight *run*. `start_voice_turn`/`cancel_speech` reach the speech token only, so if a
run is still streaming when the user interrupts, that run continues and its text is
simply no longer spoken. The model- and tool-cancellation halves of docs/07 §2 item 9
are therefore proved at their own seams (9c–9f), not through the voice interrupt. This
is behaviour, not a test gap — decide at the gate whether barge-in should also cancel
the run (see §4, D-M5-2).

## 3. What this repo cannot demonstrate — evidence #1's NFR-04 number

**The NFR-04 latency budget is NOT met by anything in this repo, and nothing here should
be read as claiming it is.**

NFR-04 budgets an end-to-end voice round trip (docs/01 §4.1: 0.8 s to transcript, 1.2 s
to first audio). That number is dominated by **model** time — faster-whisper doing STT
and Piper doing TTS — on the **reference machine**. This repo has neither: every voice
scenario above runs against fixture Wyoming services that answer instantly. A latency
measured against them is a measurement of the test harness, not of the pipeline.

What *is* repeatable here is the daemon's own share of that budget:

```bash
cargo xtask perf --voice
```

which says so itself ("measured with FIXTURE Wyoming services — the STT/TTS MODEL TIME
IS EXCLUDED"). Current result on this dev host:

| Leg | p50 | p95 | Overhead budget | NFR-04 budget it sits inside |
|---|---|---|---|---|
| `voice.stream.stop` → final transcript | 3.2 ms | 7.7 ms | p95 < 150 ms — PASS | 0.8 s end to end, faster-whisper time on top |
| first `text.delta` → first audio frame | 4.2 ms | 9.4 ms | p95 < 300 ms — PASS | 1.2 s end to end, Piper time on top |

So the daemon contributes single-digit milliseconds to each leg and the remaining budget
is entirely the speech models'. **To close evidence #1 properly** someone must, on the
reference machine, with real Silero VAD / faster-whisper / Piper services running:

1. run the round trip and record transcript and first-audio latency end to end;
2. record the STT model-size decision (`base` vs `small` int8) that docs/08 §6 defers to
   this milestone, and whether the CPU-only 0.8 s budget needs relaxing;
3. put both in the M5 gate report.

Until that happens, evidence #1 is met **functionally** (the round trip works, and is
locked in by scenario #1) and **unmet numerically**. That is a gate deviation for the
owner to accept or reject — not something to approximate.

## 4. Deviations and findings for the gate

### D-M5-1 — a deterministic command re-fires its effect on every replan turn

**Found by F5.8; not fixed by F5.8.** `DeterministicFirstProvider` classifies the slice
of the prompt *before* the first `[Untrusted …]` marker. On a **replan** — the turn after
the tool it proposed has executed — that slice is still the user's original utterance, so
the grammar recognizes it again and emits the same `ToolProposal` again. The loop runs
once per model turn until `max_model_turns` (8) trips.

Observed for one spoken "pause the music": **eight** `Pause` calls, and a run that ends
`Failed` on budget although the effect happened. The home route is the same code path,
so "turn on the desk lamp" is eight real `light.turn_on` service calls.

The zero-LLM property is **not** affected (the inner provider is never opened), so
evidence bullet #3's literal claim holds; what does not hold is "works".

Why F5.8 did not fix it: the fix changes `jarvis-application` semantics, and two existing
tests pin the current behaviour directly —
`deterministic::tests::untrusted_context_cannot_widen_a_transport_command` and
`deterministic::tests::a_home_command_that_only_matches_before_a_sanitized_tool_result_is_not_widened`
both assert that a prompt carrying a tool-result block **re-proposes** the command (their
*intent* is injection defence: appended text must not widen the verb or add arguments —
which a fix preserves and strengthens, but their literal assertion is the loop). Changing
application-layer semantics and rewriting injection-defence assertions is not integration
lock-in work; per CLAUDE.md it is strong-model work with a transition-table test and a
rust-reviewer pass.

A guard was prototyped and reverted: skip the command routes when the prompt already
carries a `[Untrusted tool result …]` frame. The prototype's naive form (echo the tool
result as the answer) is **wrong** — it would emit untrusted tool text as assistant
speech, which is exactly what those two tests exist to prevent. A correct fix must end
the turn without echoing that block (a host-authored acknowledgement, or no text at all).

The defect is pinned by acceptance scenario #3, which asserts the repeat as *current
behaviour* with instructions to tighten it on fix, so it cannot be lost between this gate
and the next milestone.

### D-M5-2 — barge-in cancels synthesis but not the in-flight run

See §2.2's scope note. Behaviour, not a test gap; a decision for the owner.

### D-M5-3 — evidence #1's NFR-04 figure is unmet

See §3.

## 5. Notes

- Every scenario is deterministic and quota-free: no scenario in this file opens a real
  model provider, so the whole set can be run as often as wanted without touching the
  shared subscription quota.
- The fixtures are the **transport** trait of each adapter (`HomeAssistantTransport`,
  `SpotifyTransport`, the Wyoming TCP services, `MediaController`), which is the
  outermost hop in every case. Allowlist enforcement, policy tiers, grant binding, error
  classification, result text and audit writes are all production code.
- These scenarios are additive to the M1/M2/M3 traces; `cargo xtask golden` remains the
  single gate entry point.
