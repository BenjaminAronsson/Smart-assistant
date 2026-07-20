# M1 Text vertical slice — Gate Report

**Status: AWAITING HUMAN SIGN-OFF.** All exit evidence demonstrated; gate suite green.
Two reviews returned no blocking findings against the invariants. Four non-blocking
findings and two deviations are recorded below (§3, §4) for the gate owner's decision —
this gate is **not** passed silently with exceptions. Do not tag `m1-complete` or check
the roadmap until sign-off.

- **Milestone:** M1 Text vertical slice (docs/08 §1)
- **Branch:** `claude/milestone-cny833-e3omzt` (PR #4)
- **Date:** 2026-07-20
- **Head commit at gate:** `94877bd`
- **Previous gate tag:** `m0-complete` (M0 signed off 2026-07-19)
- **Gate driver model:** Opus 4.8 (strong-model work per CLAUDE.md)

---

## 1. Exit evidence (docs/08 §1)

> "Question streams; quota/auth/network failure switches to a visible degraded mode and
> queues the run; run survives restart; golden traces 1–3 pass."

| # | Evidence item | Result | Measurement / proof |
|---|---|---|---|
| E1 | Question streams | **PASS** | `crates/jarvisd/tests/ws_stream.rs` (1 test, DB-backed): a submitted message drives a run whose token deltas stream live over `/ws/v1`; a reconnect resyncs persisted history without replaying transient deltas. Angular client (`web/src/app/conversation.ts`) accumulates `text.delta` into a live bubble. **Note:** the conversation UI's live streaming was broken until this session (the WS handler guarded on an impossible `type`); fixed in `c5208eb`, web gates green. |
| E2 | Failure → visible degraded mode + queues the run | **PASS** | `crates/jarvisd/src/runs.rs::degraded_run_queues_then_completes_on_recovery` drives the real `RunEngine` through quota-exhausted → queued → recovered → completed, asserting: providers endpoint reports the specific reason (`quota_exhausted`, not the generic fallback), no assistant message committed while queued, the retry re-queued as `Received`, answer committed exactly once on recovery. UI shows provider status + queued indicator (`conversation.html`). |
| E3 | Run survives restart | **PASS (with a scoped limitation — see D2)** | `crates/jarvis-infra/tests/persistence.rs` (14 tests) proves checkpoint save/load + `load_unfinished`; `main::recover_unfinished_runs` re-drives non-terminal runs from the top (idempotent in M1). **Limitation:** a run *parked in the degraded queue* at the instant of restart is **not** recovered — the queue is in-memory and the durable row is terminal `Failed` (D2, NIT 2). |
| E4 | Golden traces 1–3 pass | **PASS** | `cargo xtask golden` → "Golden traces 1–3 passed" (docs/07 §2: simple question; complex/multi-subscriber; quota-exhausted → degraded queue → recovery). |

---

## 2. Gate suite results (docs/11 §2 step 2)

Run locally at head `94877bd` against a Postgres 16 instance stood up for the gate
(pgvector not required until M4; M1 migrations `0001`–`0008` apply clean).

| Gate | Result | Detail |
|---|---|---|
| `cargo test --workspace` | **PASS** | **271 tests, 0 failures** — unit + contract + **49 DB-backed** `#[sqlx::test]` on isolated throwaway DBs (`persistence` 14, `orchestration` 8, `outbox_dispatch` 3, `event_log` 2, `ws_stream` 1, `runs_api` 7, `sessions_api` 6, `auth` 12, plus contract/unit). |
| `cargo xtask golden` | **PASS** | Traces 1–3 registered and passing (was an empty slot at M0). |
| `cargo xtask arch-test` | **PASS** | 8 crates, dependency-direction rules hold (NFR-08, security gate 1). |
| `cargo clippy --workspace --all-targets -D warnings` | **PASS** | clean. (Was failing with 8 warnings before this session — fixed.) |
| `cargo fmt --check` | **PASS** | clean. |
| `cargo xtask codegen --check` | **PASS** | committed TypeScript matches contracts (no drift). |
| `cargo sqlx prepare --check --workspace` | **PASS** | exit 0; committed `.sqlx` matches the live schema. |
| web `npm run lint` | **PASS** | clean. (Was 11 eslint errors before this session — fixed.) |
| web `npm test` | **PASS** | 5/5 Karma specs (headless Chromium). |
| web `npm run build` | **PASS** | no warnings (component-style budget trimmed under 4 kB). |
| CI (PR #4, head `94877bd`) | **4/5 green, 1 in-flight** | validate, security, build, test = success; `integration` (Postgres-backed) in progress at report time — mirrors the local `cargo test --workspace` run above. |

`cargo xtask perf --rss` + latency: **NOT RUN — harness not implemented** (deviation D1).

---

## 3. Security & performance reviews (docs/11 §2 step 3)

Both reviews cover the whole-M1 diff `8c775f9..HEAD` (M0 merge → gate head).

### security-auditor — invariant posture: **PASS (no blocking findings)**

All six invariants confirmed CLEAN, with paths traced:
- **Inv 1** (text ≠ authority): `submit_message`/`cancel_run` only persist/start/cancel; the loop advances solely in `Orchestrator::drive`; `RunState` match is exhaustive (no `_`); M2 executor states return `UnwiredInM1` and fail the run; the WS hub ignores all inbound frames. No model-output→execution edge.
- **Inv 5** (secrets in prompt/argv): Claude CLI adapter sends the prompt over **stdin**, never argv; `stderr` nulled; no key in argv or the stdin JSON; prompt never logged.
- **Inv 6** (append-only audit): `message.created` and every `run.*` event inserted in the same transaction as the domain write; the dispatcher's only mutation is `dispatched_at` delivery metadata.
- **Auth**: all new endpoints (`messages`, `timeline`, `runs/{id}`, `cancel`, `providers`, `/ws/v1`) are behind the bearer sub-router; only health + pairing are unauthenticated (loopback by design).
- **Angular WS**: frames parsed in try/catch, acted on via a known-`type` allow-list, rendered through auto-escaped interpolation; no `innerHTML`/`bypassSecurity`/`DomSanitizer`.
- **Providers endpoint**: `classify()` reduces any error to a fixed reason code; raw adapter text discarded.

Findings (none block M1; all recorded for tracking):

- **SHOULD-FIX 1 — raw adapter error text reaches the run outcome detail (Inv 5 / docs/06 §5, defence-in-depth).** `orchestrator.rs:150-153` `user_detail()` for `ModelError::Unavailable(msg)` returns `format!("provider unavailable: {}", msg)`, embedding the adapter's raw string — contradicting the function's own documented contract ("never the adapter's own error text"). This detail is persisted and emitted on the WS / timeline / `GET /runs/{id}` **before** the host strips the prefix. Today the leaked content is OS/io error text on an authenticated single-owner surface (not a credential) → **not blocking now**. It **escalates to blocking the moment a second provider adapter can surface secret-bearing text in an `Unavailable` message** (e.g. an HTTP adapter echoing a URL/response/token). **Must be fixed before M2 introduces any new adapter.** Fix has a subtlety: `health::classify` matches `"<code>:"` prefixes, so simply substituting the bare code would break reason classification — the fix must keep the host's `unavailable_reason`→`classify` path working (candidate: reduce to a code and align `classify` to match the code form).
- **NIT 2 — degraded queue is transient; a queued run emits a contradictory terminal event.** On provider-unavailable the run is already persisted `Failed` + `run.completed(failed)` emitted, then a *fresh* run is re-queued in an **in-memory** `RunQueue`. So (a) a run parked at restart is lost (durable row is terminal `Failed`; `load_unfinished` skips it), and (b) on recovery the same runId emits a second `run.started`…`run.completed(completed)`, leaving two contradictory terminal events in the append-only log. This **qualifies the F1.9 "queued runs never recovering" fix**: that fix makes *in-process* recovery work (previously it looped forever), but it does **not** make the queue survive restart. Candidate: a durable `run.queued` state/record instead of writing terminal `Failed` for a run that will be retried (F1.7 depth / M2).
- **NIT 3 — unbounded line read from the provider subprocess.** `claude_cli.rs:190-192` `read_line` has no per-line cap; a hostile/malfunctioning provider emitting a long newline-less line grows memory within the 120 s idle window (docs/06 §5 resource DoS). Bound the read.
- **NIT 4 — unbounded transient-delta accumulation in the browser.** `conversation.ts:159` grows `streamingText` without a cap; bounded in practice by turn budgets, but a client-side cap would harden it.

No findings for risk-tier drift (M2 states unwired), injection (no tool results in context), or isolation (no worker/CSP/MCP changes in this diff).

### perf-warden — ultrabook budget (docs/01 §4.1, 8 GB): **PASS (static review; no measured numbers — see D1)**

No budget violations at static-review level. Confirmed:
- Outbox dispatcher is **event-driven** (Postgres LISTEN/NOTIFY, not polling), batch-capped at 100, drains on shutdown.
- WS broadcast channel bounded (capacity 1024); lagging clients dropped → REST resync; inbound frames capped at 64 KiB.
- Run queue capacity-bounded (background ≤100, FIFO eviction; interactive unbounded but naturally ≤1–2 for a single owner).
- Single-flight semaphore serializes model access; active-run map and task tracker drain on shutdown.
- No heavy dependencies added; per-token `RunId` clone (~26 B) is negligible.
- Idle-friendly checklist (docs/09 §5): event-driven outbox ✅, 10 s health-poll interval ✅, worker serialization ✅, OTel off by default ✅.

Non-blocking notes: health-poll interval is hard-coded (make configurable by M2); `ProviderHealthTracker` map is unbounded in principle (single entry in M1 — document the static-profiles assumption).

---

## 4. Deviations requested

- **D1 — `cargo xtask perf` harness not implemented.** M1's exit evidence (docs/08 §1) is **functional, not perf-gated** — it names no latency/RSS threshold — so the milestone can exit on functional evidence. But the docs/01 §4.1 budget (jarvisd idle RSS 40–80 MB, first-token/turn latency) is **not yet measured**. Requesting sign-off to **defer the perf harness to an M2-preceding task**, with the static perf review (§3) standing in for M1. Recommend implementing it before M2 adds resident surface.
- **D2 — degraded-mode queue is in-memory (F1.7 "minimal viable", per docs/milestones/M1-features.md).** A run queued at the exact moment of a restart is not recovered (NIT 2). The general "run survives restart" case (active, non-terminal runs) **is** demonstrated (E3). Requesting sign-off to accept the transient queue for M1 and track durable queueing as F1.7-depth / M2 work.

---

## 5. Open risks / items carried forward (into the M2 feature list)

1. **Fix SHOULD-FIX 1 before any new provider adapter** — reduce `Unavailable` outcome detail to a stable code; keep the `unavailable_reason`→`classify` path working. (Invariant 5 hardening; becomes blocking at M2.)
2. **Durable degraded queue** — replace the in-memory `RunQueue` + terminal-`Failed`-then-retry pattern with a durable `run.queued` record, so parked runs survive restart and the audit log has no contradictory terminal events (NIT 2).
3. **Bound the provider subprocess line read** (NIT 3).
4. **Cap browser transient-delta accumulation** (NIT 4).
5. **Make the health-poll interval configurable**; document the `ProviderHealthTracker` static-profiles assumption (perf notes).
6. **Implement `cargo xtask perf --rss` + latency scenarios** and record the M1 baseline (D1).
7. **`conversation.ts` has no unit spec** — add component tests (DoD completeness; not an M1 exit item).

---

## 6. Session note — three defects found and fixed while driving this gate

Independent gate verification found the prior session's "F1 complete" claim was premature.
Fixed this session (all with regression tests, all pushed):

1. **Degraded-mode reason code always wrong** — an off-by-one strip left a leading space, so `classify` fell to the generic `"unavailable"` instead of the specific code (`a6c3faf`).
2. **F1.8 live streaming was dead code** — the conversation WS handler guarded on an impossible condition; "question streams" did not work until fixed (`c5208eb`).
3. **Queued runs never recovered** — the poll loop re-drove the terminal `Failed` run (a no-op that re-queued forever); now re-queues a fresh `Received` run, with a host-level integration test (`94877bd`).

Eight failing CLAUDE.md gates (clippy, fmt, web lint, web build budget) were also brought green.

---

## 7. Sign-off

- [ ] **Owner approves M1 exit evidence** (§1) and the deviations D1, D2 (§4).
- [ ] On approval: tag `m1-complete` at `94877bd` (or the merge commit), check the M1 row in docs/08 §1, and open the §5 items in the M2 feature list.
- [ ] If any item is rejected: it returns to the M1 feature list — the gate is not passed.
