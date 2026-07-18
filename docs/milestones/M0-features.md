# M0 Foundation — feature list

Status: **awaiting human approval** — no implementation begins until this list is
approved (docs/11 §3).

Milestone scope (docs/08 §1): Cargo workspace + Angular workspace, Postgres/pgvector +
migrations, `jarvis-contracts` + codegen, OTel wiring, auth placeholder, compose env,
CI pipeline.

Exit evidence: `jarvisd` starts; health page works; one persisted session round-trips;
CI green end to end.

Each feature is a vertical slice sized for one session and runs the `/feature` loop
(spec → threat note → contracts/tests first → implement → review → DoD → small PR).
"Read" lists the exact spec sections for that session (token discipline, CLAUDE.md).

---

## Features (ordered)

- [x] **F0.1 — Cargo workspace scaffold + arch gates**
  Workspace with all eight crates as compiling stubs (`jarvis-domain`,
  `jarvis-application`, `jarvis-contracts`, `jarvis-infra`, `jarvis-adapters`,
  `jarvisd`, `jarvis-agent`, `xtask`); `rust-toolchain.toml` (MSRV), edition 2024,
  `#![deny(unsafe_code)]` everywhere except `jarvis-agent`; `clippy -D warnings`,
  `rustfmt`, `cargo deny` config; `cargo xtask arch-test` enforcing the dependency
  direction (domain ← application ← {contracts, infra, adapters} ← jarvisd).
  Refs: NFR-08, NFR-10. Read: docs/02 §3, docs/03 §2. Deps: none.

- [x] **F0.2 — Compose dev environment**
  `infra/compose/dev.yml`: Postgres 16 + pgvector (loopback only, local volume,
  healthcheck, init script), OTel collector (loopback OTLP in, local export).
  Documented up/down/reset flow. Refs: NFR-10, NFR-14. Read: docs/09 §2,
  docs/02 §12. Deps: none (parallel with F0.1).

- [x] **F0.3 — Contracts seed + TypeScript codegen**
  `jarvis-contracts` v1: WS event envelope struct (docs/05 §3), error-code registry
  starter set as `jarvis-contracts::errors` (docs/05 §7), health/pairing/session DTOs
  with newtyped ULID ids, discriminated content blocks (serde `tag`), schemars schemas;
  `cargo xtask codegen` emits committed TypeScript types; drift check fails on
  uncommitted regeneration. Refs: FR-02 (DTO subset), NFR-10, NFR-13.
  Read: docs/05 §1–§3, §5, §7; skill `ws-contracts`. Deps: F0.1.

- [ ] **F0.4 — Angular workspace scaffold**
  `web/` Angular workspace (current LTS, signals + services default per docs/08 §6);
  imports the generated types from F0.3; lint/test/build wired; placeholder shell page.
  No SignalR — native WebSocket client comes in M1. Refs: FR-09 (scaffold only),
  NFR-11 (keyboard-first baseline). Read: docs/03 §3; skill `angular-shell`.
  Deps: F0.3.

- [ ] **F0.5 — jarvisd skeleton: config, tracing/OTel, health, shutdown**
  axum host binary: figment layered config validated fail-fast at startup; `tracing` +
  OTLP export to the collector with a secret-redaction layer; graceful shutdown via
  `CancellationToken` with tracked spawned work; `GET /api/v1/diagnostics/health`
  (unauthenticated, loopback only) reporting core + adapter readiness (db: up/down).
  Starts degraded when optional pieces are absent (docs/02 §12 startup order).
  Refs: NFR-14, NFR-15 (cold start <2 s, idle RSS budget). Read: docs/02 §12, §14;
  docs/09 §1, §5; skill `low-power`. Deps: F0.1, F0.2.

- [ ] **F0.6 — Migrations + conversation schema + session repository**
  sqlx migration stream with schema-per-module layout; seed schemas: `identity`,
  `conversation` (sessions, messages), `audit` (append-only `audit_events`,
  hash-chained), `outbox` (table only, no dispatcher yet). `Session` entity + newtyped
  ids in `jarvis-domain`; `SessionRepository` port in `jarvis-application::ports`;
  sqlx implementation in `jarvis-infra` writing the audit event in the same
  transaction as the domain change (invariant 6); `cargo sqlx prepare` offline data
  committed. Refs: FR-02, FR-07, NFR-05, NFR-07. Read: docs/04 §1–§3; skill
  `sqlx-data`. Deps: F0.1, F0.2.

- [ ] **F0.7 — Auth placeholder: pairing bootstrap + bearer middleware**
  Per docs/05 §6: one-time pairing code on first start (journal + health page),
  `POST /api/v1/auth/pair` exchanging it for a device record + opaque 256-bit token
  (stored hashed, `identity` schema); tower middleware requiring
  `Authorization: Bearer` on every route except loopback health; device scopes carried
  but not yet differentiated; per-device revocation fails closed. Refs: NFR-01,
  NFR-02. Read: docs/05 §6, docs/06 §2, §7. Deps: F0.5, F0.6.
  Security-auditor review mandatory.

- [ ] **F0.8 — Session round-trip vertical slice (exit-evidence feature)**
  `POST /api/v1/sessions`, `GET /api/v1/sessions/{id}`, `GET /api/v1/sessions` (basic
  list; full search deferred to M1+) behind auth; idempotency key on create; RFC 9457
  problem bodies with stable machine codes at the boundary; audit event on create.
  Angular: health page rendering `/diagnostics/health` + a minimal page that creates
  and re-fetches a session using only generated types. Proof: session survives
  `jarvisd` restart (NFR-05). Refs: FR-02, NFR-03 (ack <100 ms), NFR-05, NFR-07.
  Read: docs/05 §1–§2, §7; skills `ws-contracts`, `angular-shell`.
  Deps: F0.4, F0.6, F0.7.

- [ ] **F0.9 — CI pipeline (Azure DevOps)**
  `infra/azure-pipelines.yml` per docs/03 §6: Validate (fmt, clippy -D warnings,
  npm lint, codegen drift check), Test (cargo test --workspace, arch-test, Angular
  tests), Build (pinned toolchains, SBOM), Security (cargo deny, cargo audit, secret
  scan), Integration (compose Postgres; migration run; session round-trip smoke;
  `cargo sqlx prepare --check`). Wire `cargo xtask golden` as a stage with zero
  scenarios so the harness slot exists from M0 (docs/08 §3) — scenarios land in M1.
  Refs: NFR-08, NFR-10; docs/06 §8 gates that already apply. Read: docs/03 §6,
  docs/07 §1, §3. Deps: all prior (developed incrementally; final green = exit
  evidence).

---

## Dependency sketch

```
F0.1 ──┬── F0.3 ── F0.4 ──┐
       ├── F0.5 ──┬───────┼── F0.8 ── F0.9 (green end-to-end)
F0.2 ──┘          │       │
       └── F0.6 ──┴─ F0.7 ┘
```

## Explicitly out of scope for M0 (scope control, docs/08 §7)

- Run state machine, `RunState`, orchestrator — M1.
- WS hub / streaming / timeline UI — M1 (envelope *type* ships in F0.3; no live socket).
- Claude CLI adapter, degraded-mode queueing — M1.
- Tools, policy engine, grants, approvals — M2.
- Golden trace scenarios 1–3 — M1 (only the empty runner slot ships in F0.9).
- Messages beyond schema seed, branching, session search — M1+.
