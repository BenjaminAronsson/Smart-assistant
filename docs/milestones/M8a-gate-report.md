# M8a gate report — hands-free core

**Status: NOT READY FOR SIGN-OFF.** One exit-evidence item cannot be demonstrated, and the
reason is a feature that was deliberately not built rather than a test that failed.

Prepared 2026-08-15 against `main` (`e9118ae`), covering F8.1–F8.5 since `m7-complete`.

Read §3 first. A gate is never "passed with exceptions" silently (docs/11 §2), and this one
should not be passed at all yet.

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
| 4 | **Say the wake word and be answered** | **FAIL — not implemented** |
| 5 | The satellite does not trigger itself | **PASS** |
| 6 | Answer and alerts return to the room that spoke | **PARTIAL** |

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

### 4 — Say the wake word and be answered — **FAIL**

**The openWakeWord ONNX binding is not implemented.** A node gets `NeverWakes` and says so at
startup. It still pairs, shows its screen, speaks and answers push-to-talk; it does not answer
to its name.

Everything around it is done and tested — the port, the pipeline, the barge-in path, the
listening state, ADR-032 — and the only place an engine is chosen is `open_wake_gate()` in
`crates/jarvis-agent/src/main.rs`.

Why it was not built, stated plainly: the model assets **are** downloadable, but ADR-032
(consequence 3) forbids vendoring them, so neither a local run nor CI could exercise the
inference. Shipping unverified wake-word inference to close a checkbox would have been worse
than the gap. The feature-list tests *"a recorded clip fires once and only once"* and
*"silence and household noise do not"* are **unsatisfied**.

**Wake word:** `"Andy"` (owner's choice, 2026-08-15; ADR-032 §1, configurable via
`JARVIS_AGENT_WAKE_WORD`).

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

**Not done:** the *answer* path. A run's spoken response does not yet return to its origin node,
so *"two nodes, each gets only its own answers"* is unproven.

---

## 2. Measurements

Run on the dev host (not the reference 8 GB machine — see §3).

| Metric | Measured | Budget | Result |
|---|---|---|---|
| Cold start to healthy | **0.051 s** | < 2 s (NFR-15) | PASS |
| jarvisd idle RSS | **22.1 MB** | 40–80 MB typical, 120 MB ceiling | PASS |
| `jarvis-agent` release binary | **10.63 MB** | — | noted |
| Workspace tests | **1488 pass**, 81 binaries, 0 fail | — | PASS |
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

**NFR-04 (voice round trip) is NOT measured.** D-M5-3, open since M5, stays open — it needs the
Wyoming services on reference hardware.

---

## 3. Why this gate should not be signed yet

**Blocking:**

- **B1 — evidence item 4 is not implemented.** The sub-gate's headline claim is *"say the wake
  word at a satellite and be answered aloud by that satellite"*, and a node does not answer to
  its name. This is not a measurement shortfall; it is missing feature code.

**Also outstanding, not blocking on their own:**

- **D1 — the answer path is not routed to the origin node** (evidence 6). The alert path is done.
- **D2 — NFR-04 has still never been measured** (D-M5-3, since M5).
- **D3 — measurements are from the dev host**, not the 8 GB reference machine. docs/01 §4.1's
  numbers are the 8 GB profile's, so these are indicative, not conforming.
- **D4 — no subagent review passes were run.** The whole of M8 was built in a session configured
  not to dispatch subagents, so `rust-reviewer`, `security-auditor` and `perf-warden` did not see
  any of it. The diffs that most warrant a security pass: `jarvis-agent/src/pinning.rs` (the
  trust decision), `jarvis-agent/src/http.rs` (hand-rolled HTTP), and `jarvisd/src/ws.rs`
  `delivers_to` (who hears what). **This is a gap in process, not just in coverage.**
- **D5 — the false-accept budget is unmeasured.** ADR-032 consequence 2 requires a measured rate
  over a household-noise corpus. `WakeGate` counts detections and exposes them; nothing has been
  measured because nothing detects yet.

**Found and fixed during M8a, worth recording:**

- **`keyring` is compiled with no backend feature anywhere in the tree** — keyring 3 then
  silently uses its in-memory *mock* store. `jarvisd`'s `keyring:` secret references therefore
  do not persist and are not an OS keyring (invariant 5). Fixed for `jarvis-agent`;
  **`jarvisd` is still in that state** and should be fixed before F8.9's install story is relied
  on.
- **CI's gitleaks scan was already failing on `main`** on the canonical ULID used across ~878
  test fixtures. Fixed with a justified `.gitleaks.toml` allowlist.

---

## 4. Recommendation

**Do not sign M8a.** Two honest options:

1. **Implement F8.3's engine binding**, provision the model assets per ADR-032 (download with a
   pinned checksum), measure the false-accept rate on a household-noise corpus, and re-run this
   gate. This is what the sub-gate was scoped to prove.
2. **Accept B1 as a recorded deviation** and sign M8a for what it does prove — the transport,
   the privacy property, echo control and room attribution — with the wake word explicitly
   carried to M8c or M9. Defensible only if the wake word stops being M8a's headline claim,
   which means amending the exit evidence rather than quietly reinterpreting it.

Option 1 is the honest one. Option 2 is available, and it is the owner's call — but it changes
what M8a means, and docs/11 §3 makes that a human decision.
