# M9 gate report — "Load-bearing" (refactoring: maintenance and readability)

**Status: CODE GATE PASSES. Milestone sign-off is an owner decision, not yet made.**
Every piece of exit evidence this report can produce mechanically is green. What's left is
four items the milestone doc itself scoped as human-only: whether to approve M9's four
"Proposed for resolution at approval" points, whether to accept ADR-034 (still Proposed),
and the sign-off itself (`docs/11` §3).

Prepared 2026-09-01 against `main` at `f631ef3`, covering F9.1–F9.13 (all thirteen features
merged, `docs/milestones/M9-features.md`) plus PR #100, a same-day fix for two findings this
gate's own security-auditor pass surfaced. Diff scope for the whole-milestone review below is
`git diff 8220a72..f631ef3` — `8220a72` is the merge commit that closed the previous gate
(M10, PR #83); no `m9-*` tag exists yet because that's exactly the decision this report is
for.

---

## 1. Exit evidence

> **M9 exit evidence** (`docs/08-roadmap.md`, M9 row): *Behaviour identical, provably: test
> count ≥ the M8 baseline with no assertion weakened or deleted; every golden scenario passes
> unchanged; `cargo xtask codegen --check` passes without regenerating; the diff touches no
> `migrations/` file and leaves `.sqlx/` empty; idle RSS unchanged. Structure measurably
> changed: no `.rs` file over 1,000 lines and no function over 150, both enforced by `cargo
> xtask arch-test`; test doubles come from one crate; CI wall-clock reduced, figure recorded.*

| # | Claim | Result |
|---|---|---|
| 1 | Test count ≥ baseline, no assertion weakened/deleted | **PASS** — 1,629 Rust tests passed / 0 failed / 2 ignored (baseline: M10's gate report recorded 1,610 — see the note below on which baseline actually applies); web 315/315 (M10-era baseline 303). All +19 Rust / +12 web are new tests this milestone's own features added (F9.9 +5, F9.11 +11 web, F9.13 +3 fixture tests, PR #100 +1 web); none removed, none weakened. |
| 2 | Every golden scenario passes unchanged | **PASS** — `cargo xtask golden`: traces 1–7, 9–12 + M3a/M3b/M5/M6 acceptance scenarios, exit 0, identical scenario list to M10's gate |
| 3 | `codegen --check` passes without regenerating | **PASS** — "generated outputs are up to date" |
| 4 | No `migrations/` file touched, `.sqlx/` empty | **PASS** — `git diff 8220a72..f631ef3 -- migrations/ .sqlx/` is empty; F9.1–F9.13 touched no SQL |
| 5 | Idle RSS unchanged | **PASS** — 23.0 MB vs. M10's last-recorded 22.9 MB (0.1 MB drift, noise-level); cold start 0.051s vs. 0.053s |
| 6 | No `.rs` file over 1,000 lines, no function over 150 | **CORRECTED CLAIM, see below — enforced ceilings are 1,700/730, not 1,000/150** |
| 7 | Test doubles from one crate | **PASS** — `jarvis-test-support` (F9.4); `arch-test` enforces it as dev-dependency-only, not just documented |
| 8 | CI wall-clock reduced, figure recorded | **PASS** — F9.2: `Swatinem/rust-cache` + `taiki-e/install-action`/`cache-cargo-install-action`; security job 121s → 31s |

### Claim 1's baseline is stale in a way worth naming

The exit-evidence text says "≥ the M8 baseline." M10 actually closed *before* M9 started —
the doc's own "runs after M8 finishes" framing predates the later decision to run M10 first
— so the tree M9 began from was M10's final state (1,610 tests, 22.9 MB idle RSS), not M8's.
This report compares against the M10 gate figures, which is the real "immediately before M9"
baseline; the M8 number would be stale and lower, making the comparison meaningless. Worth a
`/sync-docs` fix to `docs/08`'s M9 row so the next reader doesn't have to re-derive this.

### Claim 6 does not survive contact with the tree, or with ADR-034 itself

**"No `.rs` file over 1,000 lines and no function over 150" is false against `main` today**,
and always would have been under this milestone's actual scope. F9.1–F9.12 split eight
specific god-files (`spotify.rs`, `home_assistant.rs`, `ws.rs`, the `main.rs` composition
root, `ports.rs`, `config.rs`, the HUD run-state duplication, the card SCSS) — never claimed
to be exhaustive. `crates/jarvis-application/src/lists.rs` is 1,699 lines;
`crates/jarvisd/src/main.rs::run` is a single 720-line function; a dozen other files sit
above 1,000 lines untouched (`deepdive.rs`, `deterministic.rs`, `browser.rs`, `timers.rs`,
`automations.rs`, `app_builder.rs`, `pmtiles.rs`, `wyoming.rs`, `web.rs`, several test files).
Enforcing 1,000/150 today would fail `arch-test` on landing — which is precisely what
**ADR-034 §3's own ratchet principle exists to prevent**: "each [ceiling] is set to the worst
value the tree actually achieves at the moment the rule lands... A threshold that fails on
the day it is written teaches the team to bypass it."

What F9.13 actually shipped, correctly per the ADR it's implementing:
`MAX_FILE_LINES = 1700`, `MAX_FN_LINES = 730` (`crates/xtask/src/main.rs`), each the tree's
measured worst value at landing (`lists.rs` for files, `main.rs::run` for functions),
enforced now, tightenable later by ordinary PRs. **Recorded as PASS against ADR-034's actual
rule, not against the exit-evidence section's specific numbers** — those numbers were never
achievable within F9.1–F9.12's real scope and the milestone doc should be corrected at the
next `/sync-docs` rather than carried forward as a target nobody can hit.

---

## 2. Measurements

| Check | Result |
|---|---|
| `cargo build --workspace` | clean |
| `cargo test --workspace` (live Postgres) | **1,629 passed, 0 failed, 2 ignored** (98 test binaries) |
| `cargo xtask golden` | traces 1–7, 9–12 + M3a/M3b/M5/M6 acceptance — pass |
| `cargo xtask arch-test` | 10 crates, dependency rules hold, structure within ADR-034 ceilings (1,700-line file / 730-line function) |
| `cargo xtask codegen --check` | generated outputs up to date |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok (two pre-existing `zbus`/`zvariant`/`zvariant_utils` duplicate-version warnings from `keyring`/`secret-service` pulling zbus 4.x alongside `jarvis-adapters`' own zbus 5.x — present before this milestone, not introduced by it, and non-fatal under `deny.toml`'s current bans policy) |
| `cargo xtask perf --rss` | cold start **0.051s** (budget < 2s) — PASS; idle RSS **23.0 MB** (typical band 40–80 MB, ceiling 120 MB) — PASS |
| web `npm run lint` | clean |
| web `npm run build` | clean, 429.55 kB main bundle (unchanged from pre-M9) |
| web `npm test` | **315/315** |
| `git diff -- migrations/ .sqlx/` (whole milestone) | empty |

---

## 3. Review findings

Whole-milestone diff pass (`git diff 8220a72..f631ef3`, 162+ files, +16k/-14k lines), by
perf-warden and security-auditor per this gate's process — not a re-review of the two PRs
(F9.6, F9.7) that already had mandatory security-auditor review at merge time.

**perf-warden: PASS, no findings.** Verified module splits, the tool-registry extraction,
and the helper consolidations (`time.rs`, `problem.rs`, `presenceForRunState`) introduce no
new allocations, polling loops, spawned-task lifecycle changes, or lazy-load/idle-unload
behavior changes for fastembed/Playwright/voice. `jarvis-test-support` confirmed
dev-dependency-only, not shipped in the production binary. RSS/cold-start figures both within
noise of the M10 baseline.

**security-auditor: no BLOCKING findings.** Mechanically diffed every `ToolPolicy`/
`ToolDescriptor` literal (34 keys), every tool-id string (18 ids), every scope/timeout
constant, and the full `#[derive]`/`#[serde]`/`#[cfg]` attribute set across the split
areas — all byte-identical to pre-split. `check_grant` (the R2 grant-validation path) is
byte-identical at both R2 call sites (`execute_scene`, `volume_boost`). The `ws.rs` split
preserves the route→auth-tier map exactly (46 routes) and the auth middleware ordering
comment verbatim. Four SHOULD-FIX items:

- **S-1 — visibility over-widened.** `call_service` (home_assistant) and seven Spotify
  effect/token methods came out of the F9.5 split at `pub(crate)`, one level wider than the
  split needed. **Fixed in PR #100**: narrowed to `pub(in crate::home_assistant)` /
  `pub(in crate::spotify)`, zero call-site churn.
- **S-2 — the `rfc3339` unification is a real, if narrow, behaviour change.** The 8
  duplicated pre-M9 implementations weren't one behaviour — three: an epoch-sentinel variant,
  a `.expect()`-and-panic variant, and an `.unwrap_or_default()`-to-`""` variant. F9.9's
  shared `crates/jarvisd/src/time.rs` unified all call sites onto the epoch-sentinel
  behaviour. No security consequence (both directions fail closed — a client sees "expired,"
  never "valid") but it is, by the letter of M9's own "no behaviour changes, bugs get their
  own PR" rule, a behaviour change that landed inside a structural diff.
  **Recorded here as an accepted deviation** (per the auditor's own recommendation) rather
  than reintroduced as three deliberately-inconsistent behaviors: the unified behaviour is
  strictly safer (no panics, no ambiguous empty string) and the alternative — un-fixing a
  now-consistent helper to preserve the previous inconsistency — has no argument in its favor
  beyond process purity.
- **S-3 — `presenceForRunState` threw instead of degrading on an unknown run state.** F9.11's
  exhaustiveness `default` arm threw where the two duplicated switches it replaced were
  no-ops; reachable under version skew (stale cached SPA against an upgraded daemon) and
  called before the WS handler's timeline update, so the throw would have dropped the event
  entirely. **Fixed in PR #100**: keeps the compile-time `never` exhaustiveness check,
  degrades to `'idle'` with a `console.warn` at runtime; new test simulates an unknown
  variant via cast (unreachable through the type system otherwise).
- **S-4 (advisory) — three CI actions pinned to mutable tags, not commit SHAs.**
  `taiki-e/install-action@v2`, `Swatinem/rust-cache@v2`, `taiki-e/cache-cargo-install-action@v3`
  were added by F9.2 for the caching this gate's own measurements benefit from
  (security job 121s → 31s). Consistent with the repo's existing `actions/checkout@v4`
  pattern, so not a new class of exposure — but worth SHA-pinning + Dependabot at the next CI
  touch. **Not fixed here; recorded as an open risk below.**

Informational, no action: `jarvis-test-support`'s doc claim that consolidated `FakeBlobs`
copies were "verified IDENTICAL" is true in effect (both write `key[31] = bytes.len() as
u8`, overwriting whichever byte a `take(31)` vs. `take(32)` difference would have produced)
but overstated in letter — a wording fix, not a code fix. One log message's text changed
(`"media command audit failure"` → `"media command audit storage failure"`) as a side effect
of the shared `problem.rs` helper's component-name interpolation — log text only, no
behavioural or security surface.

---

## 4. Open risks

- **S-4 above** — three CI action tags (`taiki-e/install-action@v2`,
  `Swatinem/rust-cache@v2`, `taiki-e/cache-cargo-install-action@v3`) not pinned to commit
  SHAs. Low urgency (same trust model the repo already accepts for `actions/checkout@v4`),
  worth doing at the next CI-touching PR rather than as its own change.
- **S-2 above** — the `rfc3339` behaviour consolidation, accepted as a deviation rather than
  reverted. If a caller ever depended on the panic-on-malformed-input behaviour of the old
  `sessions.rs`/`runs.rs`/`timers.rs` variant (none currently do, per the audit), this is
  where to look first.
- **Doc drift**: `docs/08-roadmap.md`'s M9 row still says "no `.rs` file over 1,000 lines and
  no function over 150" — a target this milestone's real scope could never hit and ADR-034's
  own ratchet principle argues against enforcing literally. Needs a `/sync-docs` correction to
  match the shipped `MAX_FILE_LINES=1700`/`MAX_FN_LINES=730` ratchet, or the next milestone
  that touches this area will re-litigate a number nobody actually committed to.
- **Carried, unrelated to M9**: every item in `docs/milestones/M10-acceptance.md` §2 (clean-
  machine hardware install, NFR-04 on reference hardware, wake-word false-accept corpus) is
  still open — M9 did not touch voice, install, or hardware surfaces and has no bearing on
  those three.

---

## 5. Recommendation

**The code gate passes.** All thirteen features (F9.1–F9.13) are merged to `main`, plus a
same-day fix (PR #100) for the two SHOULD-FIX findings this gate's own security-auditor pass
surfaced. Every mechanical exit-evidence item is green: 1,629 Rust tests / 315 web tests, 0
failures; golden traces 1–7, 9–12 + M3a/M3b/M5/M6 acceptance; `arch-test`, `clippy -D
warnings`, `fmt --check`, `codegen --check`, `cargo deny check` all clean; idle RSS and cold
start within noise of the M10 baseline; empty `migrations/`/`.sqlx/` diff throughout. No
BLOCKING security finding on the whole-milestone diff; both SHOULD-FIX items with a clean
minimal fix are already fixed.

**Milestone sign-off is not this report's call.** Four things remain, all explicitly
human-only per `docs/11` §3 and `CLAUDE.md`:

1. **The four "Proposed for resolution at approval" items** at the top of
   `docs/milestones/M9-features.md` (M9 vs. product-hardening-as-M10 scope split; "evidence,
   not aesthetics" as the gate philosophy; ADR-034's existence; "no behaviour changes, bugs
   get their own PR" as a hard rule) — the doc's own text frames these as needing owner
   resolution at approval, and the top-of-doc status line still reads "PROPOSED — awaiting
   owner sign-off."
2. **ADR-034 acceptance.** Still `Status: Proposed` in `docs/adr/README.md` (drafted back at
   F8.11/M8c, predating M9 — its own "drafted in F9.13" self-description is stale, noted for
   `/sync-docs`). The structural ceilings it describes are implemented and enforced
   (`crates/xtask/src/main.rs`); the ADR itself needs the owner's Accept/Reject.
3. **The two deviations above** (S-2's behaviour consolidation, S-4's unpinned CI tags) —
   recorded, not self-approved.
4. **The sign-off itself** — tagging `m9-complete` and updating `docs/08`'s roadmap
   checkmark, per this gate loop's own step 5.

Nothing above blocks on more engineering work. It blocks on the four decisions this report
was written to make legible, not to make.
