# M8c gate report — the seam

**Status: NOT READY FOR SIGN-OFF — for one reason, and it is not a code reason.** The exit
evidence is a fresh-machine install demonstrated end to end, and that has not been done on a
fresh machine.

Prepared 2026-08-15 against `main` (`e69bd83`), covering F8.8–F8.11.
**Updated 2026-08-17:** **D1, D2 and D3 are all closed** (voice section + config-write API,
durable monthly spend, bundle back under budget), and the hands-free claim it inherited from
M8a's B1 is closed too. What is left is a human at a clean machine.

---

## 1. Sub-gate exit evidence

> **M8c exit evidence**: *a fresh machine reaches a working, hands-free house from a documented
> install, administrable from the UI by the person who lives in it — and golden 12 proves it end
> to end.*

| # | Claim | Result |
|---|---|---|
| 1 | Administrable from the UI | **PASS** |
| 2 | A documented install | **PARTIAL — never run on a fresh machine** |
| 3 | Golden 12 proves it end to end | **PARTIAL** |
| 4 | *Hands-free* house | **PASS (code) — M8a B1 closed** |
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

### 4 — Hands-free — **PASS (code)**

**M8a's B1 is closed** (2026-08-17): the openWakeWord engine is implemented and tested over real
inference. See the M8a report §1.4 — including the finding that openWakeWord publishes **no
model for "Andy"**, which is an owner decision rather than a code gap. Until such a model is
provisioned a node falls back to push-to-talk and says so, and the settings surface names it.

Standing in a kitchen and being answered there remains off-machine evidence.

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

- ~~**D1 — no voice section in settings.**~~ **CLOSED 2026-08-17**, by owner direction to build
  the full config-write API rather than a read-only surface. Settings → Voice now shows the wake
  word (chosen from **provisioned models**, never free text — a word with no model is a node
  that has gone deaf), the ElevenLabs toggle, and the spend against its ceiling.

  Because this **relocates ADR-033 §2's consent gate** from a config file to an HTTP toggle,
  ADR-033 is amended rather than quietly reinterpreted. The conditions that keep it a gate:
  `ui` scope only (a satellite must not grant or withdraw the household's consent, nor read its
  spend); audited in the same transaction as the change; **refused unless it could be honoured**
  (a key reference *and* a voice *and* a local fallback — the same condition `main.rs` refuses
  to start under); the API key never moves from the keyring; and **withdrawal is immediate**,
  because the adapter reads the gate per utterance rather than at construction.

  Granting asks once, withdrawing does not ask at all — deliberately asymmetric.

- ~~**D2 — the ElevenLabs budget is per-process.**~~ **CLOSED 2026-08-17.** It is now durable
  and monthly, keyed `YYYY-MM`. The old behaviour made "monthly budget" untrue in the direction
  that matters: a daemon restarted daily had **no ceiling at all**. Storage returns the running
  total on each reservation, so two concurrent utterances cannot both pass the same figure, and
  the rollover is a new key rather than a scheduled job. A ledger that cannot be read spends
  locally — an unknown ceiling is not permission.

- ~~**D3 — the web bundle is over its 500 kB budget.**~~ **CLOSED 2026-08-17**, with a fix
  rather than a budget relaxation. The whole overage was MapLibre's 70 kB control stylesheet
  `@import`ed into `styles.scss` — charged to every page load, including every page with no map
  on it. It is now fetched with the map chunk it belongs to.

  ```
  initial total   502.51 kB  ->  432.60 kB     (67 kB headroom)
  styles bundle    73.30 kB  ->    3.38 kB
  ```

## 4. Open risks

- **No subagent review passes** on any of M8. F8.11 adds the voice pipeline's **only**
  `DataEgress::External` path — the clearest `security-auditor` candidate in M8c.
- `jarvisd`'s `keyring` still resolves to an in-memory **mock** store (see the M8a report). F8.9's
  production config tells operators to put secrets in the keyring; **that instruction is not
  currently true for jarvisd.** This should be fixed before anyone follows the install guide.

## 5. Recommendation

**Do not sign M8c yet — but everything still open is off-machine.**

The three code deviations this report was written around (D1, D2, D3) are closed, and so is the
`jarvisd` keyring defect it depended on: the daemon now resolves `keyring:` references through
a real Secret Service backend instead of keyring 3's silent in-memory mock, with a regression
that asserts the credential type. The install guide's advice is finally true of the daemon.

What is left, and none of it is code:

1. **The first-run script, run on an actual clean machine, watched by a human** — including a
   `keyring:` lookup under the production service account. Whether that account can reach a
   Secret Service session is deployment reachability, and no code-only run can claim it.
2. **The NFR-04 measurement** (D-M5-3, open since M5) on reference hardware, and the same for
   ADR-032's false-accept rate (M8a D5) over a corpus recorded in the rooms the nodes live in.
3. **The wake-word model decision** for "Andy" (M8a §1.4) — fund a training run or choose from
   the published set. A config change either way.

Item 1 *is* this sub-gate's exit evidence, so it cannot be delegated to a report. That is not a
gap in the work; it is what a gate is for.
