# M8c gate report — the seam

**Status: NOT READY FOR SIGN-OFF.** The exit evidence is a fresh-machine install demonstrated
end to end, and that has not been done on a fresh machine.

Prepared 2026-08-15 against `main` (`e69bd83`), covering F8.8–F8.11.

---

## 1. Sub-gate exit evidence

> **M8c exit evidence**: *a fresh machine reaches a working, hands-free house from a documented
> install, administrable from the UI by the person who lives in it — and golden 12 proves it end
> to end.*

| # | Claim | Result |
|---|---|---|
| 1 | Administrable from the UI | **PASS (with D1)** |
| 2 | A documented install | **PARTIAL — never run on a fresh machine** |
| 3 | Golden 12 proves it end to end | **PARTIAL** |
| 4 | *Hands-free* house | **FAIL — inherits M8a B1** |
| — | ElevenLabs (F8.11, added by owner direction) | **PASS** |

### 1 — Administrable from the UI (F8.8, PR #54)

A lazy `/settings` route: devices with class, scopes, last-seen and whether the class may run
tools at all — **rendered from what the server sent, never inferred**. Pair a node (show the
code, watch it appear, clear the code once it does). Revoke **asks once**, inline rather than in
a focus-trapping modal, and the revoked device **stays listed, marked revoked** — a row that
vanishes looks like the revoke failed, and *"did that actually turn off?"* is the question the
page exists to answer. Automations enable/disable plus history, where a **refusal reads as
"Refused"**.

Keyboard-first (NFR-11) with a test that walks every button and fails on a click handler bolted
to a `div`. 265 web tests pass.

### 2 — A documented install (F8.9, PR #55) — **PARTIAL**

Everything exists: Wyoming STT/TTS compose (loopback, memory-capped), systemd units for both
binaries, TLS generation, an annotated production `jarvisd.toml`, and a first-run **check**
rather than an installer that hides what it did.

**The STT model size is decided** (docs/08 §6, open since M5): faster-whisper **`base` int8**.
`small` is ~2.5× the resident memory and ~2× the latency for a word-error-rate gain that does
not change the outcome of the sentences this system actually hears; NFR-04's round trip is the
binding constraint on the 8 GB profile.

Config now refuses the half-configured states people actually reach — most importantly
ElevenLabs enabled with **no local voice to fall back to**, which would make an internet outage
a mute house.

**But the claim is "a fresh machine reaches a working house", and no fresh machine has run it.**
The script checks; nobody has watched it pass on a clean box. That is gate evidence a human
observes.

### 3 — Golden 12 (F8.10, PR #57) — **PARTIAL**

Registered in `cargo xtask golden`, exit 0. Proves the daemon-side halves against real Postgres
and the production fan-out: a timer rings in its own room and survives a restart; an automation
fires on its own and records durably; a **revoked** node's automation is denied and reaches
nothing; a revoked node is not sent its own timer alert. The node-side halves run in
`jarvis-agent`'s suite, deliberately — a claim about the node proved in the daemon's tests would
prove only that a fake said so.

It does **not** prove the wake word firing in a real kitchen, and says so in the file rather
than implying otherwise.

**The first real NFR-04 measurement (D-M5-3, open since M5) is not here.** It needs Wyoming on
reference hardware; `cargo xtask perf --voice` measures only the daemon's own share.

### 4 — Hands-free — **FAIL**

Inherits M8a's B1: no wake-word engine, so the house is not hands-free. Push-to-talk works.

### ElevenLabs (F8.11, PR #53) — **PASS**

Added to M8 by owner direction, superseding the approval's deferral. All five conditions of that
deferral were implemented as its acceptance criteria, because the timing moved and the
safeguards did not:

- **opt-in is the consent gate**, off by default;
- **local fallback always** — jarvisd *refuses to start* with ElevenLabs enabled and no local
  voice configured, because there would be nothing to fall back to;
- **sensitivity is a hard routing constraint**, labelled by the producer and never inferred from
  text, checked *before* the budget so a private message is never even priced;
- **a character budget** reserved before the request, refunded on failure, exhaustion falling
  back rather than failing the turn;
- **the API key is a keyring reference**.

**ADR-033** records the rejections as invariant calls rather than timing ones: not replacing
Piper, not the wake word, not STT, and never their Agents platform (it takes over the loop —
invariants 1 and 2 in one step).

---

## 2. Measurements

| Metric | Result |
|---|---|
| Web tests | **265 pass** (10 new) |
| Web bundle, initial | **502.51 kB** — over the 500 kB budget by 2.51 kB |
| Workspace tests | **1488 pass**, 0 fail |
| lint / build / arch-test / clippy / fmt / codegen | clean |

**The bundle was already over budget on `main` (502.29 kB) before M8c.** F8.8 added 0.22 kB —
after a finding worth keeping: Angular's `DatePipe` costs **+12 kB in the *initial* bundle even
from a lazy route**, because it drags the i18n machinery in. Replaced with `Intl.DateTimeFormat`.

---

## 3. Deviations requested

- **D1 — no voice section in settings.** Wake word, audio devices, and the **ElevenLabs toggle
  and spend counter** are not in the UI: they are daemon config and there is no config-write API.
  F8.9 gave `jarvisd.toml` its real shape, which is the prerequisite; exposing it is a further
  step. ADR-033's durable monthly counter needs the same.
- **D2 — the ElevenLabs budget is per-process** and resets on restart. The ceiling makes runaway
  spend impossible; it does not bill accurately. Recorded in ADR-033.
- **D3 — the web bundle is over its 500 kB budget** (pre-existing). A budget relaxation or a fix
  is a human decision (docs/11 §3); flagging rather than assuming.

## 4. Open risks

- **No subagent review passes** on any of M8. F8.11 adds the voice pipeline's **only**
  `DataEgress::External` path — the clearest `security-auditor` candidate in M8c.
- **Resolved after this report:** `jarvisd` now enables keyring's real Linux Secret Service
  backend (`async-secret-service` + pure-Rust crypto) and has a regression that rejects the mock
  credential type. Lookups run on awaited blocking workers, as required by keyring's Tokio
  integration. Whether the production service account can reach its Secret Service session is
  still part of the clean-machine install evidence below; this code-only run does not claim it.

## 5. Recommendation

**Do not sign M8c yet.** It needs, in order:

1. the first-run script run on an actual clean machine, watched by a human, including a
   `keyring:` lookup under the production service account;
2. the NFR-04 measurement (D-M5-3, open since M5) on reference hardware;
3. M8a's B1 resolved, or the "hands-free" claim explicitly amended.

Items 1 and 2 are inherently off-machine — they are what a gate is *for*. The code-level keyring
backend defect that previously preceded them is fixed; deployment reachability is not yet
demonstrated.
