# M1 Text vertical slice — feature list

Status: **approved — in progress**. M0 signed off + tagged `m0-complete`
2026-07-19. F1.1 and F1.2 complete (merged / on branch); continuing in order.

Milestone scope (docs/08 §1): message/run state machine, WS streaming UI, Claude CLI
adapter (single-flight queue, quota/health detection), deterministic fallback mode,
cancellation, tracing.

Exit evidence: a question streams; a quota/auth/network failure switches to a visible
degraded mode and queues the run; a run survives restart; golden traces 1–3 pass.

Each feature is a vertical slice sized for one session and runs the `/feature` loop
(spec → threat note → contracts/tests first → implement → review → DoD → small PR).
"Read" names the exact spec sections for that session (token discipline, CLAUDE.md).
Model column follows CLAUDE.md's model strategy (strong model for domain/application/
contracts; Sonnet for infra/adapter/web volume).

---

## Features (ordered)

- [x] **F1.1 — Run/message/timeline contracts + typed WS event union** · *strong model*
  `jarvis-contracts`: message-submit DTO + run acknowledgement, `RunDto` (state, budgets,
  outcome), timeline snapshot response (messages + persisted run events, resync source),
  provider-health DTO. The typed WS event union split into `DomainEvent` (outbox-published,
  replayable via `since`) vs `TransientEvent` (token deltas, never replayed) — the
  docs/05 §3 persistence classification carried in the type system. `xtask codegen` →
  committed TS. Refs: FR-01, FR-07, NFR-13. Read: docs/05 §1–§4; skill `ws-contracts`.
  Deps: none. contract-keeper review.

- [x] **F1.2 — `RunState` enum + transition table (domain)** · *strong model*
  `jarvis-domain`: the full `RunState` enum (docs/02 §4), pure
  `fn next(state, event) -> Result<RunState, TransitionError>` with an exhaustive match
  (no `_` arm), `RunBudget` value type. Transition-table test FIRST
  (`tests/transitions.rs`): every (state, event) pair as Allowed(next)/Rejected. No I/O.
  Refs: FR-01/06/07, NFR-12, ADR-003. Read: docs/02 §4; skill `state-machine`. Deps: none.
  test-architect writes the table first; rust-reviewer mandatory.

- [x] **F1.3 — Orchestrator loop + step ports + `FakeModel` (application)** · *strong model*
  `jarvis-application::orchestrator`: the code loop (`while !terminal`, budgets checked at
  loop top, `cancel.check()?`, checkpoint per safe transition). Step-executor ports for the
  M1 path (context assembly, model step, response commit); `ModelProvider` port (docs/05
  §4). `FakeModel` mirroring the port (streaming deltas, errors, delays) drives every
  orchestrator test + the golden harness. Tests: transition-driven loop to `Completed`,
  cancellation mid-model → `Cancelled` (no orphan), budget-exceeded → `Failed`.
  M2+ steps (policy/tool/approval) are intentionally unwired — states exist, executors
  don't. Refs: FR-01/03/06, NFR-05/12/13, ADR-003. Read: docs/02 §4–§5; skills
  `state-machine`, `provider-adapter`. Deps: F1.1, F1.2. rust-reviewer mandatory.

- [ ] **F1.4 — Run persistence + checkpoints + event-driven outbox dispatcher (infra)** · *Sonnet*
  Migrations for the `orchestration` (runs, plan_steps, checkpoints, cancellations) and
  `models` (invocations, usage_samples, health_state) schemas (docs/04 §3). sqlx repos
  behind the application ports; message persistence; checkpoint save/load for restart
  recovery (reconcile via idempotency, not blind re-exec). The transactional **outbox
  dispatcher** — event-driven via Postgres `LISTEN/NOTIFY`, **not polling** (perf-warden)
  — publishes committed domain events. `cargo sqlx prepare` committed. Refs: FR-01/07,
  NFR-05/13. Read: docs/04 §3, docs/02 §2; skills `sqlx-data`, `low-power`. Deps: F1.2,
  F1.3. contract-keeper (migrations) + perf-warden (dispatcher) review.

- [ ] **F1.5 — WS hub + run REST endpoints + timeline resync (jarvisd)** · *Sonnet*
  `/ws/v1` token-authenticated hub (monotonic `seq`, gap/reconnect → REST snapshot
  resync, persisted events replayed since `since`, transient deltas not); wires the outbox
  dispatcher → broadcast. REST: `POST /sessions/{id}/messages` (start run, ack <100 ms),
  `GET /runs/{id}`, `POST /runs/{id}/cancel`, `GET /sessions/{id}/timeline`. Refs:
  FR-01/06/07, NFR-03/13. Read: docs/05 §1–§3; skill `ws-contracts`. Deps: F1.1, F1.4.
  security-auditor (gateway) review.

- [ ] **F1.6 — Claude CLI adapter: stream-json, health, single-flight (adapters)** · *Sonnet*
  `jarvis-adapters::claude_cli`: spawn `claude -p --output-format stream-json` (tokio
  process, controlled workdir, built-in tools disabled, **no secrets/prompt in argv**);
  line-by-line stream-json → `ModelEvent` parsing developed against
  `tests/fixtures/claude-cli/` (healthy, quota, auth, truncated, garbage — unknown events
  log-and-skip); health classification (AuthMissing | QuotaExhausted{reset} | RateLimited
  | Malformed | IdleTimeout | Crash) with backoff; single-flight semaphore(1); cancel =
  kill process group + reap + assert no zombie (fake sleeping CLI). Implements
  `ModelProvider` only. Refs: FR-03/11, NFR-08, ADR-004/011. Read: docs/03 §4, docs/05 §4;
  skill `provider-adapter`. Deps: F1.3. security-auditor + rust-reviewer mandatory.

- [ ] **F1.7 — Degraded mode: run queue + provider health + providers endpoint** · *Sonnet*
  Application-layer run queue (interactive > background FIFO, single-flight honored);
  provider health scoring feeding router eligibility (router never self-selects, never
  switches when sensitivity forbids); degraded mode on quota/auth/network loss — LLM-needing
  runs queue with a **visible waiting state** and complete when the profile recovers.
  `GET /api/v1/providers` (health, quota, reset window) + a provider-health WS event.
  Refs: FR-12, NFR-06, ADR-011. Read: docs/02 §5, docs/03 §4; skills `state-machine`,
  `provider-adapter`. Deps: F1.3, F1.5, F1.6. security-auditor (no ambient bypass) review.

- [ ] **F1.8 — Angular conversation/timeline streaming UI + WS client** · *Sonnet*
  `web/`: native `WebSocket` client with reconnect + `seq` resync (docs/05 §3); a
  conversation/timeline surface rendering streamed token deltas, run state + waiting
  indicator, a cancel control, and a visible provider/degraded indicator. Generated types
  only (no hand-written wire types). Refs: FR-01/09/12, NFR-03/11/13. Read: docs/03 §3,
  docs/12 §2 (as available); skill `angular-shell`. Deps: F1.1, F1.5, F1.7.

- [ ] **F1.9 — Golden traces 1–3 + restart-recovery (exit-evidence feature)** · *Sonnet*
  Fill the golden harness slot (empty since M0) with FakeModel-driven scenarios (docs/07
  §2): (1) simple question streams within budget, deterministic paths make zero extra model
  calls; (2) complex question streamed to two display subscribers; (3) quota-exhausted →
  visible degraded queue with reset window shown → profile recovers → queued run completes.
  Plus the restart-survives-an-active-run test (NFR-05). Refs: FR-01/03/12, NFR-05,
  docs/07 §2. Read: docs/07 §2; skill `golden-traces`. Deps: F1.1–F1.8. This feature
  demonstrates the milestone exit evidence.

---

## Dependency sketch

```
F1.1 ──┬─ F1.3 ─┬─ F1.4 ── F1.5 ─┬─ F1.7 ─┐
F1.2 ──┘        ├─ F1.6 ─────────┘        ├─ F1.8 ─ F1.9
                └──────────────────────────┘
```

## Explicitly out of scope for M1 (scope control, docs/08 §7)

- Tools, MCP host, policy engine, grants, approvals, `web.search` — **M2**. `RunState`
  defines the policy/tool/approval states (F1.2) but their step executors stay unwired.
- Artifacts, CAS, renderers, desktop agent, media, HUD cards — **M3**.
- Memory, embeddings, retrieval, HA intent grammar, CalDAV/SMTP — **M4**.
- Session branch/archive/full-text search beyond the timeline read — M1+ as needed.
- R0 native tools in degraded mode: deferred to M2 (needs the tool/policy layer); M1
  degraded mode = queue-and-wait with visible state + provider-health surfacing.
