# M8a gate report — hands-free core

**Status: READY FOR SIGN-OFF ON THE CODE, pending two off-machine measurements.**

Prepared 2026-08-15 against `main` (`e9118ae`), covering F8.1–F8.5 since `m7-complete`.

**Updated 2026-08-17.** The original status was *NOT READY*: **B1** (the wake-word engine was
not implemented) and **D1** (the answer path did not return to the origin node) are both now
**closed**, and every claim in §1 passes. What remains is measurement that only exists on
reference hardware — see §3.

Read §3 first. A gate is never "passed with exceptions" silently (docs/11 §2).

---

## 1. Sub-gate exit evidence

> **M8a exit evidence** (docs/milestones/M8-features.md): *say the wake word at a satellite and
> be answered aloud by that satellite, with no browser involved and nothing streamed before the
> word fired.*

| # | Claim | Result |
|---|---|---|
| 1 | A node pairs itself, pins the daemon, and reconnects | **PASS** |
| 2 | A satellite can capture and play audio | **PASS** |
| 3 | **Nothing is streamed before the word fires** | **PASS** |
| 4 | **Say the wake word and be answered** | **PASS (code); the room is off-machine)** |
| 5 | The satellite does not trigger itself | **PASS** |
| 6 | Answer and alerts return to the room that spoke | **PASS** |

### 1 — A node pairs, pins, and reconnects (F8.1, PR #44)

`jarvis-agent pair` runs the ADR-031 ceremony against a **real rustls listener** with a
per-run rcgen certificate. Evidence in `crates/jarvis-agent/tests/pairing_tls.rs`:

- pairing succeeds and the daemon verifies the node's signature (possession proven, not assumed);
- **the pinned fingerprint is the certificate actually served** — the node captures the served
  leaf and refuses to store anything unless `sha256(DER)` matches what the response reported;
- a fingerprint that does not describe the served certificate is refused **and stores nothing**;
- TLS with no reported fingerprint is refused rather than left unpinned;
- after pairing, an impostor with its own valid certificate is refused **at the handshake**.

Revocation is terminal (`tests/node_session.rs`): a 1008 close or a 401/403 handshake ends the
process with exit code 3, and the node does **not** reconnect even once. An ordinary close does
reconnect, so a daemon restart needs no human.

**`JARVIS_AGENT_TOKEN` is deleted.** No code path reads a node credential from the environment.

### 2 — Capture and playback (F8.2, PR #45)

cpal at the one wire format docs/05 §1 fixes (PCM 16-bit LE, 16 kHz, mono). Playback is driven
by the real `voice.speak.*` contract: a format that is not the agreed one is **refused** rather
than played at the wrong rate; a cancelled utterance flushes immediately while a completed one
drains; a stop for another utterance cannot silence the current one.

Mute is enforced **at the source** (`FrameAccumulator`), and audio buffered *before* a mute is
discarded rather than released on unmute. A missing device is non-fatal: a node with no
microphone still connects and says so.

### 3 — Nothing streams before the word fires (F8.3, PR #46)

The privacy property M8's decision 3 turns on, asserted twice:

- at the gate (`wake.rs`): every frame is discarded before a detection, and the pre-roll buffer
  is bounded so an idle node accumulates nothing;
- **at the socket** (`tests/node_audio.rs`): 50 frames of microphone audio produced, **zero
  bytes sent**.

And the converse, so the claim is not satisfied by a node that never streams at all: a detection
opens exactly one stream, bracketed by a real `voice.stream.start`, carrying 500 ms of pre-roll.

ADR-032 §3 states as a *decision* that the daemon cannot ask a node to stream continuously —
there is no protocol frame for it and no code path to it.

### 4 — Say the wake word and be answered — **PASS (code)**

**Closed 2026-08-17.** The openWakeWord engine is implemented and selected in
`open_wake_gate()`, the one place ADR-032 §4 allows an engine to be chosen.

The original report declined to build it on the grounds that inference could not be exercised
here. **Two of its premises turned out to be wrong**, and both are worth recording because the
same reasoning would otherwise recur:

* **ONNX Runtime was already in the tree** — `ort` 2.0.0-rc.13, pulled in by `fastembed` for
  embeddings since M4. Pinning to that version means the workspace resolves one runtime, and
  `Cargo.lock` gained no new package.
* **openWakeWord ships its own test recordings**, 16 kHz mono PCM — which is exactly the
  node's wire format. So the engine can be tested against its author's fixtures rather than
  against ours.

F8.3's two named acceptances now hold, over real inference:

| Acceptance | Evidence |
|---|---|
| a recorded clip fires **once and only once** | two words, two recordings |
| silence does not fire | 10 s |
| household speech that is not the word does not fire | `hey_jane.wav` — a person saying a phrase with the same rhythm and opening syllable, a far harder negative than white noise |
| a node does not answer to a **different** word | cross-model |
| a tampered feature extractor is refused | pinned SHA-256 |

Two implementation details were load-bearing and silent when wrong: the melspectrogram must
run over each 1280-sample chunk **plus 480 samples of the previous one** (1280+480 yields
exactly 8 mel frames = 80 ms at the model's 10 ms hop; the bare chunk yields 5 and drops 30 ms
of every chunk), and openWakeWord is trained on **raw int16 magnitudes as floats** — normalising
to [-1, 1] costs ~40 dB and the models never fire.

Assets are provisioned, never vendored (ADR-032 consequence 3):
`infra/install/fetch-wake-assets.sh` verifies pinned SHA-256s and installs nothing unless every
file matches. CI gains a `wake-word-engine` job — nothing else in the workflow compiles the
engine, so without it the wake word would rot between satellite image builds with no red check.

**Wake word: `"hey jarvis"`** (ADR-032 §1, owner 2026-08-17). **Resolved during this gate.**

Implementing the engine surfaced that openWakeWord publishes models for six words only —
`alexa`, `hey jarvis`, `hey mycroft`, `hey rhasspy`, `timer`, `weather` — and the previous
default, `"Andy"`, was not among them. The shipped default would therefore have been a house
that could not hear its own name until somebody ran a GPU training job, and nothing in the tree
said so.

The owner chose `hey jarvis`, which is published, so it works the moment the assets are
provisioned. The swap cost one paragraph of ADR-032 — which is itself the evidence for §4's
claim that the word is configuration rather than code.

Two tests keep the class of problem closed rather than just today's instance:
`the_default_wake_word_has_a_pre_trained_model` checks the default against the published set,
and `the_default_wake_word_loads_from_provisioned_assets` loads it against exactly the files the
installer provisions — the test the previous default would have failed. jarvisd carries the
matching guard (`the_default_wake_word_is_one_the_default_assets_provide`), because the daemon
serves this word to nodes and a disagreement would have a node answering to one word while the
shell reported another.

A bespoke word remains available at the cost of a training run, and a node configured for a word
it has no model for still says so at startup and falls back to push-to-talk.

**"Answered aloud, in that room" is still an off-machine claim.** The code path is complete and
tested end to end; a human saying the word in a kitchen is what a gate is for.

### 5 — The satellite does not trigger itself (F8.4, PR #47)

Asserted under the **worst case on purpose**: a detector that fires on *any* loud audio, echo
cancellation switched **off**, and the node's own speaker at full volume in its microphone.
Without suppression this is an infinite loop. It opens no stream and sends no audio.

`EchoCanceller` (NLMS, 2048 taps) reduces residual energy below 30% of a synthetic echo, which
is what makes barge-in-by-voice possible while the assistant is talking. `HalfDuplex` is the
floor that does not depend on convergence: with no AEC the node degrades to push-to-talk rather
than looping.

### 6 — Answer and alerts return to the room that spoke (F8.5, PRs #48/#50) — **PARTIAL**

**Done:** a timer remembers the room it was set in (durable — asserted through a fresh store
over the same database), and the alert is delivered to **that node and no other**. The node
synthesises the tone locally, so no audio crosses the wire and a room still rings when the
voice pipeline is down. The fallback is intact: no room, or nobody listening in it, rings on the
daemon's host — a revoked or unplugged node cannot swallow an alarm.

**Closed 2026-08-17 — the answer path.** The cause was one line of the fan-out rule: a run's
text deltas ride the **Session** channel, whose rule is `ui`, which a `voice-node`/`room-node`
deliberately never holds (F7.1). So the node that asked the question was the one socket in the
house that could not hear the answer — and a node cannot speak what it is not sent.

`start_voice_turn` now records the run it started as an id the socket owns, and
`delivers_to_owner_of` lets that run's spoken answer past the Session rule. Ownership is
per-socket and in memory, so it does not survive a reconnect and `replay_since` needed no
matching change.

The exemption is keyed on **two** things, and the second is the one that matters: ownership,
**plus an allowlist of exactly the four event types** `feed_speech` consumes. Ownership alone
would have been a standing invitation — `approval.requested` is a Session event about a
specific run carrying the exact effect, the real arguments, and an approval id that is a
decision oracle. It fails to match today only because its `runId` happens to sit nested under
`card`, which is the shape of a DTO and not a security boundary.

F8.5's named acceptance `two_nodes_each_get_only_their_own_answers` now passes, alongside
`owning_a_run_does_not_hand_a_node_the_rest_of_that_run` — the test that keeps this from
becoming a hole.

---

## 2. Measurements

Run on the dev host (not the reference 8 GB machine — see §3).

| Metric | Measured | Budget | Result |
|---|---|---|---|
| Cold start to healthy | **0.051 s** | < 2 s (NFR-15) | PASS |
| jarvisd idle RSS (M8a, 2026-08-15) | **22.1 MB** | 40–80 MB typical, 120 MB ceiling | PASS |
| jarvisd idle RSS (all of M8 merged, 2026-08-18) | **22.8 MB** | 40–80 MB typical, 120 MB ceiling | PASS |
| Cold start to healthy (2026-08-18) | **0.052 s** | < 2 s (NFR-15) | PASS |
| `jarvis-agent` release binary, no engine | **10.63 MB** | — | noted |
| `jarvis-agent` release binary, `wake-word-onnx` | **37.18 MB** | — | **noted — see below** |
| Workspace tests, as first prepared (2026-08-15) | **1488 pass**, 81 binaries, 0 fail | — | PASS |
| Workspace tests, all six branches merged (2026-08-17) | **1520 pass**, 0 fail | — | PASS |
| `jarvis-agent` tests | **90 pass** (was 3 at M7) | — | PASS |
| `cargo xtask arch-test` | 9 crates, rules hold | — | PASS |
| `cargo clippy -D warnings` | clean | — | PASS |
| `cargo fmt --check` | clean | — | PASS |
| `cargo deny check` | advisories/bans/licences/sources ok | — | PASS |
| gitleaks | no leaks | — | PASS |
| `cargo xtask golden` | all traces, exit 0 | — | PASS |

The agent binary grew from 0.46 MB (M3a) to 10.63 MB: cpal, rustls, ed25519, keyring and the
audio pipeline. It is a per-node cost on a device that exists to do this, not a daemon cost, and
jarvisd's idle RSS is unchanged.

**With the wake-word engine compiled in it is 37.18 MB** — ONNX Runtime links statically, so
there is no separate dylib to ship, and this is the whole of it. Recorded rather than waved
through: it is **+26.6 MB per satellite image**, it is the direct cost of ADR-032's decision 2
(detection on the node, so audio never leaves until the word fires), and it is charged per
*node* rather than to the daemon — `jarvisd` does not link `ort` at all through this path.

ADR-032's first consequence budgeted "~20–30 MB resident for the model and its feature
extractors" per node; this figure is **binary size**, not RSS, and the two should not be
confused. **Resident memory with the engine running has not been measured** — that belongs with
the reference-hardware measurements in §3, and it is the number the 8 GB profile (docs/01 §4.1)
actually constrains.

**NFR-04 is now measured (D-M5-3, open since M5 — first figures ever produced).**

`cargo xtask perf --voice-real` drives the production pipeline against **real** faster-whisper
`base-int8` and Piper from `infra/compose/voice.yml`, using a **real recorded utterance** rather
than silence — silence would let an STT model return almost immediately and report the pipeline
with its expensive part skipped.

| NFR-04 figure | Median | Worst of 5 | Budget | Result |
|---|---|---|---|---|
| Final transcript after end of speech | **432.7 ms** | 460.3 ms | 800 ms | **PASS** |
| First audio after the response text begins | **91.7 ms** | 92.3 ms | 1200 ms | **PASS** |

The first sample of each run is discarded: it pays for the model warming up, and reporting it
as a latency figure would describe a state the house is in exactly once. A second run agreed
(523.4 ms / 116.8 ms median), so the figures are reproducible rather than a single lucky pass.

**⚠️ Measured on the dev host, NOT the 8 GB reference profile** — an i7-11850H, 16 threads,
31 GB. NFR-04 is specified on the reference machine (docs/01 §4.1), so this is evidence that
the pipeline is the right order of magnitude and that nothing in it is pathologically slow. It
is **not** evidence that the budget holds on the target, and the headroom here (transcript at
54% of budget on a machine several times the reference) is exactly the margin that could
disappear. **The reference-hardware run is still owner evidence** — but it is now a matter of
re-running one command on that machine rather than of building a harness.

---

## 3. Why this gate should not be signed yet

**Blocking: none as of 2026-08-17.**

- ~~**B1 — evidence item 4 is not implemented.**~~ **CLOSED.** The engine is implemented,
  selected in `open_wake_gate()`, and tested over real inference against openWakeWord's own
  recordings. See §1.4 — including the finding that **no pre-trained model existed for the
  then-default "Andy"**, which the owner resolved during this gate by moving to `hey jarvis`.

**Outstanding, none blocking on their own:**

- ~~**D1 — the answer path is not routed to the origin node.**~~ **CLOSED.** See §1.6.
- **D2 — NFR-04 has still never been measured** (D-M5-3, since M5).
- **D3 — measurements are from the dev host**, not the 8 GB reference machine. docs/01 §4.1's
  numbers are the 8 GB profile's, so these are indicative, not conforming.
- ~~**D4 — no subagent review passes were run.**~~ **CLOSED 2026-08-18.** All three ran over
  `m7-complete..HEAD`. It was the right call to insist on: between them they found **two blocking
  defects and a false claim in these reports**, none of which any test was failing on.

  | Pass | Outcome |
  |---|---|
  | `security-auditor` | 2 blocking, 7 should-fix — **all fixed** except S3 (recorded, see below) |
  | `rust-reviewer` | 1 blocking (**D-M4-1 is not closed** — see the M8b report), 7 should-fix |
  | `perf-warden` | no blocking budget violations |

  **What the security pass found:** an absent room could **swallow an alarm** (the timer path
  trusted a broadcast send, which succeeds whenever *any* socket exists — so an unplugged kitchen
  node plus an open browser meant nobody heard it, contradicting ADR-023 and this report's own
  §1.6 claim); and automations wrote **no audit rows at all** for enable, disable, delete or
  firing (invariant 6, on the one surface that acts unattended). Both fixed with regression
  tests. Also fixed: consent failing open on a startup read error, a stream/run id namespace
  confusion, a resolved API key inside a `#[derive(Debug)]`, a wake word that could escape the
  asset directory, CRLF in the hand-rolled request head, and a `DELETE` route that could never
  succeed for any automation that had fired.

  **Two reviewers disagreed and were checked rather than averaged.** `perf-warden` reported the
  wake inference as running "on the audio thread (no Tokio worker)"; `rust-reviewer` reported it
  as blocking a Tokio worker. Inspection settles it for `rust-reviewer`: `gate.accept` runs
  synchronously inside `handle_captured_frame`, which is awaited from a `select!` arm in the
  socket task, so that task cannot poll the socket or the shutdown token while three ONNX models
  run. Recorded as an open should-fix.

  **Still open from these passes** (none blocking, each recorded rather than quietly carried):
  the AEC adapts against near-silence with no double-talk detector (**measured**: ~56%
  attenuation of a near-end voice, and a ~200 ms clipped burst that the wake gate then scores);
  the AEC costs **997 µs per 20 ms frame (~5% of an i7 core, sustained)** and never skips even on
  an all-zero reference; automations execute tools with a `CancellationToken` nothing can cancel;
  `jarvis-agent` never handles SIGTERM, so `systemctl stop` skips its drain path; and S3 — every
  spoken answer is labelled `Normal`, so a run that read a message aloud can reach the
  third-party voice.
- **D5 — the false-accept budget is still unmeasured, but is now measurable.** ADR-032
  consequence 2 requires a *measured* rate over a household-noise corpus rather than an
  assurance. The harness exists and reports accepts/hour
  (`the_false_accept_rate_over_a_noise_corpus_is_within_budget`, budget 1/hour); it skips until
  `JARVIS_WAKE_NOISE_CORPUS` names a directory of recordings.

  The corpus is deliberately **not** in this repository: a false-accept rate measured over audio
  chosen by whoever tuned the threshold measures nothing. It has to be recorded in the rooms the
  nodes will live in. **This is gate-bench work for the owner**, and it is the last piece of
  ADR-032 that this report cannot produce.

**Found and fixed during M8a, worth recording:**

- **`keyring` was compiled with no backend feature anywhere in the tree** — keyring 3 then
  silently uses its in-memory *mock* store, so `jarvisd`'s `keyring:` secret references did not
  persist and were not an OS keyring (invariant 5), while F8.9's install guide told operators to
  put secrets there. Fixed for `jarvis-agent` during M8a and **for `jarvisd` on 2026-08-17**,
  with a regression test that asserts the concrete credential type — a config-parser test cannot
  catch this, because `keyring:` parses identically under either backend.
- **CI's gitleaks scan was already failing on `main`** on the canonical ULID used across ~878
  test fixtures. Fixed with a justified `.gitleaks.toml` allowlist.

---

## 4. Recommendation

**The code is ready to sign; two things are not code.**

Every claim in §1 now passes, and the two items that made the original report say *do not sign*
— the missing wake-word engine and the unrouted answer path — are closed with tests rather than
with argument.

What this report **cannot** produce, and what a gate exists for:

1. **Say the word in a kitchen and be answered there.** The path is complete and tested end to
   end; a human standing in a room is the evidence.
2. **The false-accept rate over a household-noise corpus** (D5) and **NFR-04's round trip** (D2,
   open since M5), both on the 8 GB reference machine rather than this dev host (D3).

The wake-word question this report originally left open (§1.4) is **closed**: the owner moved
ADR-032 §1 to `hey jarvis`, a published word, so a fresh install answers to its name with no
training run.

**D4 stands and should be read as a process gap, not a coverage one:** no `rust-reviewer`,
`security-auditor` or `perf-warden` pass has seen any of M8, including the diffs added since
this report was first written — `delivers_to_owner_of` (who hears what) and the settings
surface's consent gate are the two that most warrant a security pass.
