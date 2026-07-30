# M3a "Artifacts, desktop agent, workers, media" — Gate Report

**Status: AWAITING HUMAN SIGN-OFF** · Prepared 2026-07-30 · Milestone loop docs/11 §2

M3a is feature-complete on `main`: **F3a.1–F3a.8 all merged**. M3 was split by owner
decision (2026-07-22, `docs/milestones/M3-features.md`) into **M3a** — which carries
**all five M3 exit-evidence bullets** — and **M3b** (the docs/12 HUD face, deep dive,
personal utilities), gated separately. docs/08 §1 still records M3 as one milestone;
`/sync-docs` should reflect the split after this gate.

Scope since the M2 sign-off commit (`eace634`): **22 commits, 84 files,
+15 503 / −115 lines.** New shipped dependencies, all outside the pure crates:
`zbus` 5 in `jarvis-adapters` (`default-features = false` + `tokio`, so no second
executor stack) and the `jarvis-agent` binary's own runtime deps
(tokio + tokio-tungstenite + futures-util + tracing). `jarvis-domain` and
`jarvis-application` `Cargo.toml` are **byte-identical to the M2 gate** — the
artifact/media/display work added pure types and ports only.

---

## 1. Exit evidence (docs/08 §1) → result

Every item below is runnable via `cargo xtask golden` and documented, with its
manual real-hardware counterpart, in **`docs/milestones/M3a-acceptance.md`**.

| # | Exit-evidence item | Result | Evidence |
|---|---|---|---|
| 1 | Create/reopen an artifact after restart | ✅ MET | `jarvisd::artifacts_api::artifact_reopens_through_a_fresh_app_instance` (create → rebuild app state on the same DB → `GET` manifest + blob, content address re-verifies) and `jarvis-infra::artifacts::artifact_reopens_after_a_simulated_restart` (persistence half, live Postgres). Golden 7 additionally reopens its patch artifact through a fresh `PgArtifactStore`. |
| 2 | Place a canvas on a selected monitor | ⚠️ MET (agent-fake at the compositor hop — D-M3a-5) | `jarvisd::display_api::open_places_canvas_on_the_requested_monitor`; unknown/malformed monitor and "no monitor resolves" fail **closed**, and no dispatch happens when the audit write fails. Real-Hyprland steps: acceptance doc §3. |
| 3 | Audited browser flow | ⚠️ MET (fixture worker; no browser binary in CI — D-M3a-2/D-M3a-5) | `jarvis-adapters::browser::tests::every_action_records_append_only_audit` — every typed action writes append-only audit **before** its effect; `a_step_that_cannot_be_audited_fails_closed`; `page_content_that_looks_like_a_tool_call_is_inert_text`. Real Playwright steps: acceptance doc §3. |
| 4 | Pause whatever is playing from the media bar | ⚠️ MET (MPRIS fake at the D-Bus hop — D-M3a-4/D-M3a-5) | `jarvisd::media_api::pause_from_the_media_bar_pauses_the_active_player` + `a_command_is_audited_before_it_is_applied`, driven through the media bar's own REST route. Real MPRIS steps: acceptance doc §3. |
| 5 | **Golden 7** — a coding task creates a patch artifact in a disposable worktree; no direct deployment | ✅ MET | `crates/jarvisd/tests/golden7_coding_patch.rs`: the **real** `tools/coding-worker` in a **real** disposable `git worktree`, live Postgres + the real CAS. Pins the immutable v1 `CodeText`/`text/x-diff` artifact, `artifact.created` in the same transaction (chain verified), reopen through a fresh store, **and the no-deployment property**: unchanged HEAD, clean tree, no applied file, worktree removed. A hostile worker adding `applied`/`deploy`/`tool_call`/`auto_authorized` changes nothing (invariant #1) and its summary is still sanitized (invariant #5). |

---

## 2. Gate runs (on merged `main`, commit `7d97adf`)

| Gate | Result |
|---|---|
| Full workspace suite (`cargo test --workspace`, incl. `#[sqlx::test]` vs live Postgres) | ✅ **599 passed, 0 failed** |
| `cargo xtask golden` | ✅ traces **1–7** + **4 M3a acceptance scenarios** pass |
| `cargo xtask arch-test` (docs/06 §8 gate 1) | ✅ 9 crates, dependency rules hold |
| Adversarial suite (docs/06 §8 gate 2) | ✅ golden 6 (malicious page) + golden 7's hostile worker + browser "page cannot inject a tool call" |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ clean |
| `cargo fmt --check` | ✅ clean |
| `cargo xtask codegen --check` | ✅ generated schema + TS up to date |
| `cargo sqlx prepare --check --workspace` (live schema) | ✅ exit 0 (warns only about *potentially unused* cached queries) |
| `cargo deny check` (docs/06 §8 gate 6) | ✅ **advisories ok, bans ok, licenses ok, sources ok** — run for real this time (M2 could not; D2 there) |
| web (`npm run lint`, `npm test`, `npm run build`) | ✅ lint clean, **24 tests pass**, production build succeeds |
| Perf / RSS (docs/01 §4.1, 8 GB profile) | ✅ measured — see §3 |

### 3. Perf measurement (in lieu of the still-absent `xtask perf` harness)

Release build, `jarvisd` with the dev config against compose Postgres, media/web/MCP
integrations at their defaults (all **off**):

| Measurement | Value | Budget (docs/01 §4.1, 8 GB) |
|---|---|---|
| `jarvisd` idle RSS | **10.2 MB** | 40–80 MB idle |
| `jarvisd` RSS after serving a request | **11.2 MB** | ≤120 MB peak |
| `jarvisd` release binary | 21.7 MB | — |
| `jarvis-agent` release binary | 0.46 MB | — |

**Perf review (inline, see D-M3a-7):** M3a adds **no default-on resident component**.
`[integrations.media] enabled = false` by default, so no session-bus connection, no
watcher task and no media tools exist at runtime; `zbus` is built without its async-io
executor stack so it reuses the tokio runtime jarvisd already has. The MPRIS watcher is
event-driven and debounced (never polling) and bounded at `MAX_PLAYERS = 16`. Both
workers are per-task child processes with bounded round trips, not residents.
`jarvis-agent` is a separate 0.46 MB binary that only runs on a desktop host. Idle RSS is
unchanged from M2 (≈11 MB), which is the number that matters for the ultrabook target.

---

## 4. Security review (whole-milestone diff since `eace634`)

Structural checks, verified on the merged tree rather than asserted:

- **Gate 1 (purity).** `jarvis-domain`/`jarvis-application` `Cargo.toml` unchanged since
  the M2 gate; `arch-test` green across 9 crates. The artifact domain types, media value
  types and the `ArtifactStore`/`BlobStore`/`MediaController`/`MediaStateSink`/
  `MediaWindowSink` ports are pure; `zbus`, the CAS and sqlx stay in adapters/infra.
- **Gate 2 (no authority from text).** The only **production** `executor.execute` call
  site in the tree is still `crates/jarvis-application/src/orchestrator.rs:649`, after
  `policy::evaluate` (+ grant validation and the presence belt for R2). Every
  `.execute(` hit in `browser.rs`/`coding.rs`/`media_mpris.rs` is inside `mod tests`.
  Worker replies are `serde` structs with fixed fields, so an untrusted worker cannot
  declare a tool, a risk level or a deployment — golden 7 now proves that behaviourally
  as well as structurally.
- **Gate 3 (exact effects).** Host-owned `ToolPolicy` everywhere: the browser host's
  policy-table overlay refuses an action without host policy, `coding_patch_policy()` is
  R1 *data output* with no apply path a grant could authorize, and media splits
  transport/volume-within-cap (R1) from `media.volume_boost` (R2, player-bound so the
  grant's argument hash binds the target).
- **Gate 5 (secrets).** `#![deny(unsafe_code)]` in all nine crates including
  `jarvis-agent` (no `unsafe` block was needed). Worker stderr is never forwarded (it
  inherits credentials); the coding instruction travels in the environment, never argv or
  a shell string; the media window is incognito with no credentials; build provenance is
  **host-attested**, never worker-reported.
- **Gate 6 (supply chain).** `cargo deny check` clean on all four checks.
- **Invariant 6 (append-only audit).** Artifact manifest + `artifact.created` are one
  transaction (verified by `verify_chain` in golden 7 and in the infra suite); browser
  steps and media commands audit **before** the effect and fail closed if the audit
  cannot be written.

Per-feature `security-auditor` and `rust-reviewer` runs happened inside F3a.1–F3a.7 and
their findings (including three BLOCKING fixes: the browser transport desync, the MPRIS
multi-byte-URL panic, and the unregistered `media.*` error codes) are recorded in
`docs/milestones/M3-features.md`. **F3a.8 and this gate pass were reviewed inline by the
primary model instead of the subagents — see D-M3a-7.**

---

## 5. Deviations (require accept/reject)

Recorded during implementation, in full in `docs/milestones/M3-features.md`:

- **D-M3a-1 (F3a.3)** — artifact surface shipped **read-only**; the `artifact.created`
  WS event and a client `POST` create were deferred (artifacts are run outputs, never
  client-uploaded; the event waits for its first producer). The durable half landed with
  F3a.6 as intended. → *Recommend ACCEPT.*
- **CF-M3a-A (F3a.3)** — blob download buffers the whole blob (no streaming/size cap).
  Fine for markdown notes and patches; the real trigger is M6 `Bundle`. → *Recommend
  ACCEPT as a tracked carry-forward.*
- **D-M3a-2 (F3a.5)** — browser host + real Playwright worker shipped; jarvisd
  tool-stack wiring, keyring credential resolution and the container launch profile
  deferred; CI runs a fake worker. **ADR-027 (isolation: container = contract,
  process+profile-dir = dev/CI fallback) is still *Proposed* and needs owner
  acceptance at this gate.** → *Recommend ACCEPT + accept ADR-027.*
- **D-M3a-3 (F3a.6)** — coding host + worker shipped patch-only; the *replayable* WS
  `artifact.created` and orchestrator/`ToolStack` wiring deferred to the run-loop slice.
  → *Recommend ACCEPT.*
- **D-M3a-4 (F3a.7)** — media shipped end-to-end through the owner-driven REST path; the
  model-facing `media.playback` tool is registered but not yet reachable (no orchestrator
  tool-stack wiring); Spotify/now-playing/voice transport stay M5 by scope. Owner also
  needs to confirm the two-tool volume split. → *Recommend ACCEPT.*
- **D-M3a-5 (F3a.8)** — golden 7 scripts the coding step (deterministic, quota-free)
  and CI substitutes the last OS hop for items #2/#3/#4 (no Hyprland, no browser binary,
  no session bus). Everything below that hop — policy, audit-before-effect, sanitization,
  fail-closed — is exercised for real, and `M3a-acceptance.md` §3 gives the manual
  real-hardware verification for each. → *Recommend ACCEPT: this is the same
  fixture-driven discipline CLAUDE.md mandates.*
- **D-M3a-6 (F3a.8)** — `cargo xtask golden` now requires the compose test env (live
  Postgres, `node`, `git`), matching docs/07 §2 and the CI `integration` job. A scenario
  whose filter matches nothing is now a failure, so gate evidence cannot silently become
  a no-op. → *Recommend ACCEPT.*
- **D-M3a-7 (this gate + F3a.8) — reviews for the final slice were performed inline by
  the primary (strong) model rather than by the `rust-reviewer`/`security-auditor`/
  `perf-warden` subagents.** The session's harness policy is not to spawn subagents
  without an explicit request, so F3a.8's diff (a test harness, an xtask runner and
  docs — no production code path, and therefore outside the security-auditor's stated
  trigger) and the §3/§4 gate passes were reviewed against the same checklists by the
  primary model, with the structural claims verified by command rather than assertion.
  F3a.1–F3a.7 all had their mandated subagent reviews. → *Owner decision: accept, or
  re-run the three subagents against the M3a diff before sign-off.*

---

## 6. Open risks / carry-forwards into M3b and beyond

1. **No orchestrator wiring for the M3a tools yet** (browser, coding, media). Each is
   proven at its own boundary, but no *run* reaches them; that slice (with the run's
   actor/correlation replacing placeholder `system` actors, and the replayable
   `artifact.created` WS event) is the first thing M3b's deep-dive work will need.
2. **`cargo xtask perf --rss` still does not exist** (M2's D2). Idle RSS is measured by
   hand each gate. The first milestone that adds a resident component — M4 (fastembed)
   or M5 (voice) — must build the harness first; M3a did not need it.
3. **CF-M3a-A** blob streaming before large-artifact producers (M6).
4. **Real-hardware verification is manual** for compositor/browser/MPRIS (D-M3a-5).
   A desktop smoke checklist exists in `M3a-acceptance.md` §3; nothing automates it.
5. **ADR-027 is Proposed.** Until accepted, the isolation contract for both workers is
   informally "container in production, process in dev".

---

## 7. Recommendation

All five exit-evidence items are demonstrated and repeatable from one command; every CI
gate including `cargo deny` is green; the resident-memory budget holds with room to
spare. **Recommend sign-off on M3a**, accepting D-M3a-1 … D-M3a-6 and ADR-027, with an
explicit owner decision on D-M3a-7 (inline vs subagent review for the final slice).

On approval: tag `m3a-complete`, tick the roadmap, and `/sync-docs` the M3a/M3b split
into docs/08 §1 before starting F3b.1.
