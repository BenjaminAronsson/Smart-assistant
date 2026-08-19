# M8c gate report — the seam

**Status: ✅ SIGNED OFF — owner, 2026-08-19, with the clean-machine install recorded as an
accepted deviation.**

Be precise about what that means, because it is the one place this milestone's evidence is
weaker than its wording. On 2026-08-19 the full stack was brought up and driven end to end from
a written runbook (`docs/TRY-IT.md`, every command executed in order): Postgres, Wyoming, the
daemon, the shell served by the daemon, an owner paired, a satellite paired with its credentials
in the **OS keyring**, and the wake word heard over the air. What that is **not** is a *fresh
machine* — it is a developer workstation with the stack already built. The owner signed on that
evidence; a genuine clean-machine install remains untested and is the first thing F10.2/F10.3
build on.

That run was not ceremonial: it found four defects, including one that made a woken node stream
continuously and never wake again, and one where the shell told a never-paired browser that the
daemon was down.

---

*Original status: NOT READY FOR SIGN-OFF — the exit evidence is a fresh-machine install
demonstrated end to end, and that had not been done on a fresh machine.*

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

### 2 — A documented install (F8.9, PR #55) — **PARTIAL, and a real bug found**

**Update 2026-08-18: the script was run end to end for the first time, and it found a defect
that would have failed the owner's clean-machine run.**

`first-run.sh` gates its "a paired device" step on `"paired":true` in the health response.
**`HealthResponse` never had a `paired` field.** The check could therefore never pass — on any
machine, however correctly installed — and the script exited 1 every time. Nobody noticed
because nobody had run it: F8.9 shipped the script and the gate deferred running it to "a fresh
machine, watched by a human".

That is precisely the failure mode this sub-gate's exit evidence exists to catch, and it is
worth recording that *writing* the install story and *running* it are different acts.

Fixed by giving the health endpoint the field the script always assumed: a bare `paired`
boolean, deliberately disclosing nothing else (not how many devices, not which, not their
classes), and **failing closed** — an unreadable identity store reads as "no owner", because a
check that reported success on a broken database would be worse than one that failed.
Regression test in `tests/health.rs`; the two contract wire-shape tests were updated, which is
them doing their job.

With the fix, against a live daemon, real Postgres and both real Wyoming services:

```
== database        ok: postgres is running
== migrations      ok: migration state readable   (18, 19, 20 installed)
== daemon health   ok: jarvisd answers on http://127.0.0.1:8741
== a paired device ok: an owner device is paired
== voice services  ok: wyoming service on 10300 / 10200
== a microphone    ok: an input device exists
first-run check passed.
```

**This is not the clean-machine run.** It is a developer workstation with the stack already
built, so it does not demonstrate "a fresh machine reaches a working house from a documented
install". What it does demonstrate is F8.9's named acceptance — *a scripted install reaches a
healthy daemon and a paired device* — and that the script now reports the truth when it does.
The owner's run on real hardware remains this sub-gate's exit evidence.

### Original assessment (2026-08-15)

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
model for the then-default "Andy"**, which the owner resolved during the gate by moving ADR-032
§1 to `hey jarvis` — a published word, so a fresh install answers to its name with no training
run. A node configured for any word without a model still says so and falls back to
push-to-talk.

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
| Workspace tests (as prepared, 2026-08-15) | **1488 pass**, 0 fail |
| Workspace tests (all six M8 branches merged, 2026-08-17) | **1520 pass**, 0 fail |
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

- ~~**No subagent review passes**~~ **CLOSED 2026-08-18** (M8a report, D4). Again the instinct
  was right about where to look: on the `DataEgress::External` path the audit found consent
  **failing open** — a transient settings-read error at startup fell back to the config file, so
  an owner who withdrew consent in the shell could have it silently reinstated by a slow
  Postgres. Fixed to fail closed.

  **S3 remains open and is the one worth reading before signing:** every spoken run answer is
  labelled `Normal`, so a run that used a mail or calendar tool and reads the result back can be
  spoken by ElevenLabs. The routing constraint ADR-033 §4 describes is correct machinery with
  nothing driving it. Fixing it needs a signal the socket does not have (`RunUpdate` carries no
  tool variant), so it is an application-layer change, not a patch. Mitigation today: the feature
  is off unless explicitly consented to.
- **Resolved after this report:** `jarvisd` now enables keyring's real Linux Secret Service
  backend (`async-secret-service` + pure-Rust crypto) and has a regression that rejects the mock
  credential type. Lookups run on awaited blocking workers, as required by keyring's Tokio
  integration. Whether the production service account can reach its Secret Service session is
  still part of the clean-machine install evidence below; this code-only run does not claim it.

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
   The wake-word decision that stood here is closed — ADR-032 §1 is now `hey jarvis`.

Item 1 *is* this sub-gate's exit evidence, so it cannot be delegated to a report. That is not a
gap in the work; it is what a gate is for.
