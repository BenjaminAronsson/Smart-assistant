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
cargo test --workspace          # 1563 tests
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

Where a claim is load-bearing, the test was **mutation-checked** — deliberately broken to
confirm it fails. Recorded per feature in the PRs; the pattern was adopted after F10.2's
first restore tests passed with `pg_restore` replaced by `echo`.

---

## 2. Human evidence — not delegable, and not yet done

These are the honest gaps. None is blocked; all need a person and hardware.

### 2.1 A genuine clean-machine install

`docs/TRY-IT.md` is verified, but it was verified on a machine that already had a Rust
toolchain, a container runtime, a warm build cache and a working audio stack. **A test
running inside that cache cannot simulate its absence.** What is unproven is the first
twenty minutes on a machine that has never seen this project.

*Method:* a fresh VM or a reimaged laptop, `docs/TRY-IT.md` followed literally, nothing
skipped. What matters is not whether it works but **where it stops** — a missing package,
an unstated assumption, a permission.

### 2.2 NFR-04 on the reference hardware

Measured on the dev host: **432.7 ms** transcript, **91.7 ms** first audio (budgets 800 ms
/ 1200 ms). Re-measured during M10 against live Wyoming: 621 ms / 119 ms.

Both are a fast desktop. **The budget in docs/01 §4.1 is the 8 GB ultrabook**, and nothing
has run there. `cargo test -p jarvisd --test voice_latency_real` with `JARVIS_NFR04_REAL=1`
is the harness; it needs the reference machine.

### 2.3 False-accept corpus for the wake word

ADR-032 consequence 2. A wake word that fires while you are watching television is not a
working wake word, and no fixture measures that. Needs hours of real room audio on real
hardware.

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
