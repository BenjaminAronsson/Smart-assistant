# M10 acceptance — "a product, not a demo"

Exit evidence for M10. Executable where it can be; honest about where it cannot.

The milestone's promise was an operational lifecycle — **install, talk, break, restore,
upgrade, roll back** — plus the surfaces that make a house administrable by the person
living in it. Golden 10 holds the parts a script can hold. The rest is a person at a
machine, and this document names those rather than implying a script covered them.

---

## 1. Executable evidence

Run all of it with:

```bash
cargo test --workspace          # 1610 tests
cargo xtask golden              # golden 1–7, 9–12 + M3a/M3b/M5/M6 acceptance
cargo xtask arch-test
cargo xtask codegen --check
(cd web && npm test && npm run lint)
```

| # | claim | evidence |
|---|---|---|
| 1 | It runs, and answers to its name | `docs/TRY-IT.md` — every command executed in order, output shown |
| 2 | A backup describes a whole house | `jarvis-infra/tests/backup_restore.rs` |
| 3 | A restore returns a **working** house | `golden10_the_house_survives` — schedulable and writable after restore |
| 4 | A restore missing its blobs fails loudly | `backup_restore.rs` — refuses rather than half-working |
| 5 | Rollback is restore, and it is executed | `update_rollback.rs` |
| 6 | There are no `down` migrations, and the docs say so | `rollback_is_restore_because_there_are_no_down_migrations` — fails the build if one is added |
| 7 | Health reports what this daemon can actually do | `report_capabilities`, verified against a live daemon |
| 8 | A diagnostics bundle is safe to send | `diagnostics_bundle.rs` — a seeded credential, transcript and message body appear nowhere |
| 9 | The policy view matches the engine's decisions | `policy_api.rs` — compared against fresh `evaluate` calls |
| 10 | A release verifies its own signature | `release_signing.rs` |
| 11 | A stale advisory scan is refused | `a_valid_signature_over_a_stale_advisory_scan_is_refused` |
| 12 | The surface is accessible | `a11y.spec.ts`, `contrast.spec.ts` |
| 13 | An owner who loses their token can get back in | `pairing_api.rs` (F10.9) |
| 14 | There is something to install, and it installs on a host that has never had Jarvis | CI job `install-artifact` — cuts a signed release, verifies it, installs it into a pristine `ubuntu:24.04` with no Rust, no Node and no source tree |
| 15 | The signature covers everything an owner runs, as a closed set | `release_signing.rs` — a file smuggled into `compose/postgres-init/` is refused even though everything listed matches |
| 16 | The installer will not install a release that failed its own check | `install.rs::install_refuses_a_release_that_does_not_verify` |
| 17 | The database password never reaches argv, a child process, or a log | `install.rs::backup_never_puts_the_database_password_in_argv`, `host_env.rs` — every spawn site in the daemon scrubs it |

Where a claim is load-bearing, the test was **mutation-checked** — deliberately broken to
confirm it fails. Recorded per feature in the PRs; the pattern was adopted after F10.2's
first restore tests passed with `pg_restore` replaced by `echo`.

---

## 2. Human evidence — not delegable, and not yet done

These are the honest gaps. None is blocked; all need a person and hardware.

### 2.1 A genuine clean-machine install — **build half done, hardware half outstanding**

Originally recorded here as entirely owner-only. That was too quick: the *prerequisites and
build* half does not need the owner's hardware, only a machine with nothing on it — and a
clean container is exactly that.

**Done (2026-08-20).** A `debian:13` container with no toolchain, no Node, no build cache,
source obtained by `git clone` so nothing came with it. It found a real defect:

| stopped at | cause | fixed |
|---|---|---|
| ``error: linker `cc` not found`` | `build-essential` was not in the prerequisite list | TRY-IT.md §0 |
| `jarvis-agent` build failure | `libasound2-dev` not listed (ALSA, for cpal) | TRY-IT.md §0 |
| `git clone` / rustup impossible | `curl`, `git`, `ca-certificates` not listed | TRY-IT.md §0 |
| `ng build` refuses | apt's Node is v20; Angular needs ≥ 22.22 | TRY-IT.md §0 names it |

The list was also **narrowed** by testing, not guessed: `jarvisd` needs only
`build-essential`; `pkg-config`, `libssl-dev` and `cmake` are not required, because TLS is
rustls throughout and nothing links OpenSSL.

**Still outstanding, and genuinely owner-only.** A container has no sound card, no
microphone, no compositor and no session bus. What remains unverified on a clean machine is
everything past the build: pairing a real browser, a real audio device opening, the wake
word firing in a room, and `room-node` under Hyprland. That needs a reimaged laptop, and
`docs/TRY-IT.md` followed literally on it.

*Method for the rest:* fresh VM or reimaged laptop, TRY-IT.md followed literally, nothing
skipped. What matters is not whether it works but **where it stops**.

### 2.2 NFR-04 on the reference hardware

Measured on the dev host: **432.7 ms** transcript, **91.7 ms** first audio (budgets 800 ms
/ 1200 ms). Re-measured during M10 against live Wyoming: 621 ms / 119 ms.

Both are a fast desktop. **The budget in docs/01 §4.1 is the 8 GB ultrabook**, and nothing
has run there.

Deliberately not approximated. Constraining CPU on this host would produce a number, and a
number that looks like a measurement but was taken on different silicon is worse than an
admitted gap — the figures are dominated by Whisper and Piper, so throttling the harness
without throttling them would measure nothing at all, and throttling them would be a
guess at hardware nobody has profiled.

*Method:* on the reference machine, with the voice compose stack running:

```bash
JARVIS_NFR04_REAL=1 cargo test -p jarvisd --test voice_latency_real -- --nocapture
```

It prints both figures against their budgets and fails if either is exceeded.

### 2.3 False-accept corpus for the wake word — **harness ready, corpus outstanding**

ADR-032 consequence 2. A wake word that fires while you are watching television is not a
working wake word.

**The harness exists and is verified working** (2026-08-20):
`the_false_accept_rate_over_a_noise_corpus_is_within_budget` in
`crates/jarvis-agent/tests/wake_onnx.rs`, budget one accept per hour. Exercised against
real non-matching speech — the `alexa`, `hey mycroft` and `hey jane` clips — with the
`hey jarvis` model:

```
false-accept measurement: word="hey_jarvis" clips=3 hours=0.001 accepts=0 rate=0.000/hour
```

**That number is not the measurement, and must not be quoted as one.** Three clips is about
four seconds; a per-hour rate computed from four seconds of audio is arithmetic, not
evidence. What it does establish is that the harness runs end to end, the corpus plumbing
works, and the engine does not fire on three other wake words.

#### A preliminary real-room reading — over budget, and it needs settling

An agent left listening in a real room during M10 testing produced the first field data,
and it is not reassuring:

```
word="alexa"   102 detections over 13.58 h   =  7.44 accepts/hour   (budget 1.00)
```

Median gap between detections two minutes; longest quiet stretch 3.7 hours. The engine's
hysteresis is working (it re-arms below half the firing threshold, and the gate holds a
stream open after a hit), so these are separate re-fires rather than one utterance counted
many times.

**This is a signal, not the measurement**, and the difference matters:

* the word was `alexa`, not the shipped default `hey jarvis` — different models have
  different false-accept characteristics, and nothing here transfers between them;
* there is **no ground truth**. Some detections were a test clip being played
  deliberately during that session, and nobody was recording who said what. The rate is an
  upper bound on false accepts, not a count of them;
* one room, one microphone, one day.

It is recorded because a 7× overshoot is worth knowing about before sign-off, and because
it is exactly the kind of result that never surfaces if the gate is quietly ticked. If the
controlled run reproduces anything near this for `hey jarvis`, the threshold in
`WakeSensitivity` is the first thing to look at — ADR-032's budget was chosen as the point
at which people stop leaving a satellite switched on, and 7 an hour is well past it.

*What remains is recording, not building.* Capture hours of ordinary household audio at
16 kHz mono — television, conversation, kitchen noise — into a directory, then:

```bash
JARVIS_WAKE_NOISE_CORPUS=/path/to/corpus JARVIS_AGENT_WAKE_WORD=hey_jarvis \
  cargo test -p jarvis-agent --features wake-word-onnx --test wake_onnx the_false_accept_rate -- --nocapture
```

It fails the build if the rate exceeds one per hour, so the result is a gate, not a note.

### 2.4 The two-binary upgrade

`update_rollback.rs` proves migrations apply forward with live data and that the documented
rollback returns a working house. It does **not** run the *old binary* against a rolled-back
database, because that needs two builds of two versions — a release-process step, not a unit
of the test suite. First real upgrade between tagged versions is the moment to check it.

### 2.5 Screen-reader feel and photo-background contrast

`docs/12` §8.1. Structure is tested; announcement order and verbosity in a real reader are
not reducible to assertions, and no fixture can model an arbitrary photo behind the scrim.

---

## 3. Known state at sign-off

**Deliberately opt-in, and near-empty by default.** A fresh install registers **two** tools
and cannot search the web, control lights or play anything until those are configured. This
surprised the owner during M10 testing — it read as three unrelated faults (no research, no
cards, nothing spoken) and was one configuration gap plus one browser bug. Health now
reports it and `docs/TRY-IT.md` says it before you hit it.

**Fixed during testing, worth recording:** the browser never resumed its playback
`AudioContext`, so a daemon that was demonstrably synthesizing audio produced a silent
browser. A suspended context accepts `start()` and plays nothing — no error, no warning. It
failed exactly like working code, which is why it survived a passing suite on both sides of
the socket.

**Carried forward:** nothing new. D-M4-1 was dropped by owner decision at the M8 sign-off.

---

## 4. Sign-off

Human-only (docs/11 §3). Not signed off. §1 is complete and reproducible; **§2 is
outstanding and is the owner's to run.** Ticking this milestone before §2.1–§2.3 would be
recording a milestone as met on evidence nobody gathered.
