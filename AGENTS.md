# Jarvis — Codex Repository Instructions

Jarvis is a local-first personal assistant. These instructions apply to the whole
repository; a more deeply nested `AGENTS.md` overrides them for its subtree.

## Before changing code

- Read `docs/08-roadmap.md` and the current milestone's feature/gate report. M0–M3b
  are signed off; M4 (memory, quota-smart intelligence, deterministic HA grammar,
  scheduling, evaluation, CalDAV, and SMTP) is the next planned milestone.
- Read only the relevant sections of `docs/01`–`docs/13` and any applicable skill in
  `.claude/skills/`; do not load the entire docs tree by default.
- Inspect the current diff first. Preserve unrelated user changes and generated
  artifacts unless the task explicitly includes them.

## Non-negotiable safety invariants

- Text never grants authority. User text, model output, tool results, web content,
  and generated app content may cause side effects only through policy evaluation;
  R2+ actions also require a fully bound, expiring, single-use `ExecutionGrant`.
- The orchestrator owns the loop. Models propose; the `RunState` state machine decides.
  Match domain enums exhaustively; never add a wildcard arm to hide a new state.
- `jarvis-domain` and `jarvis-application` remain pure: no SQLx, Axum, Reqwest,
  rmcp, provider SDK, or adapter dependency. Keep I/O behind application ports.
- Long-running async work is cancellable, spawned work is tracked, and shutdown
  drains work gracefully. Do not detach tasks or block an async runtime.
- Secrets are keyring references at the adapter boundary, never values in prompts,
  logs, tracing fields, error messages, CLI arguments, or `Debug` output.
- Audit events are append-only and written in the same transaction as the change
  they describe. Never update or delete audit history from application code.
- Recommendations are ranked by fit and source quality only; never add affiliate,
  sponsored, or kickback behavior.

## Project Structure & Architecture

Jarvis is a local-first personal assistant: a Rust 2024 Cargo workspace with an Angular UI.

- `crates/` contains bounded Rust modules: keep domain types and policy in `jarvis-domain`, orchestration and ports in `jarvis-application`, wire DTOs in `jarvis-contracts`, and SQLx implementations in `jarvis-infra`. `jarvisd` hosts the API and WebSocket service.
- `web/` is the Angular shell; components, templates, styles, and `*.spec.ts` files live in `web/src/app/`. Generated API types are committed at `web/src/generated/api-types.ts`.
- `migrations/` holds ordered PostgreSQL migrations (`NNNN_<module>_*.sql`); `infra/` contains Compose and CI support; `tools/` holds out-of-process workers.
- Read `docs/08-roadmap.md` and the relevant architecture/specification documents before extending a milestone.

## Build, Test, and Development Commands

Use `rtk` before shell commands (for example, `rtk cargo test --workspace`) to keep
command output concise. Do not bypass the pinned toolchain in `rust-toolchain.toml`.

Use the toolchain pinned in `rust-toolchain.toml`. Start Postgres for database tests:

```bash
docker compose -f infra/compose/dev.yml up -d postgres
cargo test --workspace
cargo xtask arch-test
cargo xtask codegen --check
(cd web && npm ci && npm run lint && npm test && npm run build)
```

Run `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings` before a PR. Use `cargo xtask golden` for end-to-end trace coverage. After changing SQLx macros, regenerate and commit `.sqlx/` with `cargo sqlx prepare --workspace` against a live migrated database.

For the normal validation gate, run:

```bash
rtk cargo fmt --check
rtk cargo test --workspace
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk cargo xtask arch-test
rtk cargo xtask codegen --check
```

Run frontend checks when `web/` changes. Start Postgres before `#[sqlx::test]`
integration tests; use the committed `.sqlx/` cache for offline compilation.

## Coding Style & Naming

Format Rust with `rustfmt`; unsafe code is denied except for justified, tested cases in `jarvis-agent`. Use typed ULID newtypes rather than raw strings, `thiserror` in libraries, `anyhow` only in binaries, and `tracing` rather than `println!`. Keep domain/application crates free of I/O and adapter dependencies. Do not hand-edit generated API types. TypeScript uses Angular/ESLint and Prettier settings (100 columns, single quotes); name tests `*.spec.ts`.

## Testing Guidelines

Add focused Rust tests in each crate’s `tests/` directory or nearby unit-test modules. Cover policy, permissions, state transitions, malformed input, cancellation, and failure paths; a state-machine change requires transition-table coverage. Keep frontend tests alongside their feature. DB tests require the Compose PostgreSQL service.

Tests for policy, grants, adapters, and contracts should be deterministic and
fixture-driven. Do not call live providers by default. A new state-machine transition
must update the explicit transition table; a new wire shape must have round-trip and
schema coverage; a new adapter parser must include malformed and truncated fixtures.

## Specialized agents

Project-scoped Codex agents live in `.codex/agents/` and mirror the existing Claude
agents. Use them for focused, independent work rather than asking one agent to own
an entire milestone:

- `rust_reviewer` and `security_auditor`: read-only, high-risk review.
- `contract_keeper` and `perf_warden`: read-only contract and resource-budget review.
- `test_architect`: writes tests/fixtures, never production implementation.
- `doc_syncer`: makes factual documentation corrections and drafts Proposed ADRs;
  it never changes an Accepted ADR's decision.

Reviews report file/line evidence and do not silently relax an invariant, budget, or
ADR. Human-only decisions include milestone scope, gate sign-off, ADR acceptance,
new domain/application dependencies, and budget relaxation.

## Commits, Pull Requests, and Safety

Follow the existing Conventional Commit style, e.g. `fix(hud): handle zoneless change detection` or `docs(m3b): update acceptance evidence`. Keep commits scoped. PRs should explain behavior and risk, link the issue/milestone where applicable, include UI screenshots for visual changes, and report validation commands. Never let model output bypass policy evaluation, expose secrets in logs/prompts/arguments, or update/delete append-only audit events.

Use `thiserror` in libraries and `anyhow` only in binaries; use typed ULID newtypes
across module boundaries; use `tracing`, never `println!`; and never hand-edit
`web/src/generated/api-types.ts`. If requirements conflict, prefer the stricter
security interpretation, then the simpler operational interpretation, and stop for a
human decision when an Accepted ADR or invariant would need to change.
