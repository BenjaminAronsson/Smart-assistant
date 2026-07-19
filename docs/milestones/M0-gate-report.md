# M0 Foundation — Gate Report

**Status: SIGNED OFF (owner, 2026-07-19).** Deviations confirmed: CI platform = GitHub
Actions; Angular pinned to 22. Tag `m0-complete` is applied to the commit whose CI run is
green (see E4). M1 is now unblocked.

- **Milestone:** M0 Foundation (docs/08 §1)
- **Branch / PR:** `claude/milestone-cny833` / PR #3
- **Date:** 2026-07-19
- **Head commit at gate:** `e62be34`
- **Gate driver model:** Opus 4.8 (strong-model work per CLAUDE.md)

---

## 1. Exit evidence (docs/08 §1)

> "`jarvisd` starts; health page works; one persisted session round-trips; CI green end to end."

| # | Evidence item | Result | Measurement / proof |
|---|---|---|---|
| E1 | `jarvisd` starts | **PASS** | Release binary boots and serves health in **21 ms** (cold start to first healthy response; NFR-15 budget < 2 s). |
| E2 | Health page works | **PASS** | `GET /api/v1/diagnostics/health` returns `{status, version, adapters}`; live-probes Postgres per request — verified `ok`→`degraded`→`ok` across a stopped/started DB container. Angular shell renders it (5 Karma specs incl. HttpTestingController contract checks). |
| E3 | One persisted session round-trips | **PASS** | `infra/ci/smoke.sh`: pair → create (201) → idempotent replay (200, same id) → unauthenticated list (401) → **jarvisd restart** → session survives (NFR-05); audit chain shows `device.paired` + `session.created`. Green locally end to end. |
| E4 | CI green end to end | **PASS** | Run #3 (head `1fd3fb0`) completed **success across all five jobs** — validate, test, build, security, **integration** (which runs the `infra/ci/smoke.sh` session round-trip). Run #1 had failed at `test` on a CI-config bug I introduced (§4), fixed in `e62be34`. The Angular-22 head (`5720dd2`) re-confirms the same pipeline green. |

---

## 2. Gate suite results (docs/11 §2 step 2)

All run locally at head `e62be34` against the compose Postgres.

| Gate | Result | Detail |
|---|---|---|
| `cargo test --workspace` | **PASS** | **170 tests**, 0 failures (unit + contract + 14 DB-backed `#[sqlx::test]` on isolated throwaway databases). |
| `cargo xtask arch-test` | **PASS** | 8 crates, dependency-direction rules hold (NFR-08, security gate 1). |
| `cargo xtask golden` | **PASS (empty)** | 0 scenarios — harness slot only; scenarios 1–3 land in M1 (docs/08 §3). |
| `cargo fmt --check` | **PASS** | clean. |
| `cargo clippy --workspace --all-targets -D warnings` | **PASS** | clean. |
| `cargo xtask codegen --check` | **PASS** | committed TypeScript matches contracts (no drift). |
| `cargo sqlx prepare --check --workspace` | **PASS** | committed `.sqlx` matches the live schema. |
| `cargo deny check` | **PASS** | advisories ok, bans ok, licenses ok, sources ok (security gate 6). |

---

## 3. Security & performance reviews (docs/11 §2 step 3)

### security-auditor — whole-M0 holistic pass → **M0 invariant posture: PASS**
No blocking findings. All six invariants confirmed across the assembled system:
- **Inv 1** (text ≠ authority): no tool-execution machinery exists at all (no orchestrator/executor/policy/grant); the only reachable writes are the two intended paths (device pair, session create).
- **Inv 3** (domain purity): `jarvis-domain`/`jarvis-application` depend only on allowed crates; arch-test green.
- **Inv 4** (cancellable): bounded 15 s graceful-shutdown drain; no unbounded blocking.
- **Inv 5** (no secrets in logs): full config→`Redacted`→pool and pairing-token→sha256 paths traced; no value reaches logs/spans/wire/DB; `TraceLayer` logs method+path only.
- **Inv 6** (append-only audit): both writers (session create, device pair) write the audit event in the same transaction; DB triggers block UPDATE/DELETE/TRUNCATE.
- Applicable release gates (docs/06 §8): gate 1 (arch-test) PASS, gate 5 (diagnostics/no-secrets) PASS, gate 6 (cargo-deny) PASS. Gates 2–4 (adversarial/tool-profile/escape) are N/A until tools exist (M2).

Advisory (M2, not an M0 blocker): session routes require a valid device token but do not yet assert a specific **scope** — enforce scopes before any R2+ tool route. Nits: pairing code on the loopback-only health body (deliberate, docs/05 §6) and non-constant-time code compare (mitigated by pre-hashing) — both fine while loopback is the sole binding.

### perf-warden — resource budget (docs/01 §4.1, 8 GB profile) → **PASS on all lines**

| Budget line | Measured (release) | Budget | Result |
|---|---|---|---|
| Cold start to healthy (NFR-15) | 21 ms | < 2 s | **PASS** (~95× headroom) |
| `jarvisd` idle RSS | 8.6 MB | 40–80 MB | **PASS** (~12% of budget) |
| `jarvisd` peak RSS (post-load) | 8.8 MB | ≤ 120 MB | **PASS** |

Zero polling loops (health probes on demand), one justified process-lifetime spawn (signal listener), OTel collector off by default. Advisory: the adapter-health map grows as providers land in M1+ — re-measure at each gate; ample headroom.

---

## 4. Failures & how they were handled

- **CI `test` job failed (run #1).** `cargo test` failed to **compile** `jarvis-infra` (`E0282`, 21 errors) because I set `SQLX_OFFLINE=false` on that step, forcing the `sqlx::query!` macros to validate against the live DB at compile time — but the job never migrated it. Reproduced locally (un-migrated DB + `SQLX_OFFLINE=false` = exact failure; `SQLX_OFFLINE=true` from the committed `.sqlx` cache compiles clean and the `#[sqlx::test]` suite still passes because it migrates its own throwaway DBs at runtime). **Fixed** in `e62be34`: tests compile offline; the `prepare --check` step (which legitimately needs a live schema) now migrates first. Re-run in progress.

No code defects were found at the gate — the only failure was CI configuration.

---

## 5. Deviations requested (owner decision)

1. **CI platform: Azure DevOps → GitHub Actions.** The spec (docs/03 §6, CLAUDE.md) named Azure DevOps, but the project lives on GitHub. Corrected to `.github/workflows/ci.yml` at the owner's direction; docs updated in the same commit. The stage/gate contract is unchanged. **Confirm this is the intended platform.**
2. **Angular version — RESOLVED.** Owner requested Angular 22; upgraded 20→21→22 via `ng update` (Angular 22.0.7, TypeScript 6, angular-eslint 22, typescript-eslint 8.64). Requires Node ≥22.22.3 — CI and `.nvmrc` now on Node 24. web lint/test/build green.

## 6. Open risks carried into M1

- CI-green (E4) is the one exit-evidence item not yet confirmed at report time — pending run #2.
- Scope enforcement on authenticated routes must land before M2 tool routes (security advisory).
- Recorded implementation deferrals (see `M0-features.md`): INSERT-only audit DB role (prod provisioning), malformed-JSON problem-body extractor (M1), keyring secret resolution (packaging), run-trace correlation ids (M1).

---

## Recommendation

All exit evidence is demonstrated **except E4 (CI green), which is pending the re-run of
the config fix** — validate/build/security already passed on run #1, and the test-job fix
is reproduced-and-verified locally. Security and performance posture both PASS with no
blocking findings. **Recommend sign-off once CI run #2 reports green**, with the two
deviations in §5 confirmed. This report will be updated with the run #2 result before it
is presented as final.
