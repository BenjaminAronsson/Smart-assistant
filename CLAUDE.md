# CLAUDE.md — Jarvis implementation instructions

You are implementing **Jarvis**, a local-first personal assistant with a Rust core.
The specification lives in `docs/`. Read `docs/08-roadmap.md` to find the current
milestone and its exit evidence before writing code.

## Non-negotiable invariants

1. **Text never grants authority.** No user message, model output, tool result, web page,
   or generated app content may cause a side effect except through the policy engine and,
   for R2+, an `ExecutionGrant`. There is no code path from model output to tool execution
   that bypasses `policy::evaluate`.
2. **The state machine owns the loop.** Models propose; `orchestrator` decides. Never
   implement a "call tools until the model feels done" loop. All transitions are the
   `RunState` enum in `jarvis-domain`; `match` on it must be exhaustive (no `_` arm).
3. **Domain crates stay pure.** `jarvis-domain` and `jarvis-application` must not depend
   on sqlx, axum, reqwest, rmcp, tokio-specific APIs beyond `async` traits, or any
   provider SDK. Enforced by `cargo deny` + the dependency test in `xtask`.
4. **Everything is cancellable.** Every async operation that can outlive a user's patience
   takes a `CancellationToken`. Spawned work is tracked; graceful shutdown checkpoints
   runs and drains streams.
5. **No secrets in prompts, logs, or CLI args.** Secrets are keyring references resolved
   at the adapter boundary. The tracing layer redacts known secret fields.
6. **Append-only audit.** Audit events are written in the same transaction as the domain
   change they describe, and never updated or deleted by application code.
7. **Recommendations are never monetized.** Product/shopping recommendations are ranked
   only by fit and source quality — no affiliate links, sponsored placement, or retailer
   kickbacks, ever (ADR-021). Retailer links are plain references, no different from any
   other source link.

## Workspace layout

Cargo workspace, one crate per bounded module (see `docs/02-architecture.md` §3):

```
crates/
  jarvis-domain          # entities, value types, RunState, policy types — no I/O
  jarvis-application     # use cases, orchestrator state machine, ports (traits)
  jarvis-contracts       # versioned wire DTOs (serde + schemars), WS envelope
  jarvis-infra           # sqlx repos, keyring, artifact CAS, outbox
  jarvis-adapters        # claude-cli, home-assistant, mcp-host, wyoming, embeddings (fastembed)
  jarvisd                # axum host binary: REST, WebSocket hub, DI wiring
  jarvis-agent           # desktop agent binary (Hyprland IPC, window control)
  xtask                  # dev automation: codegen, arch tests, golden traces
web/                     # Angular workspace (shell UI)
tools/                   # out-of-process MCP tool servers (browser, coding)
infra/                   # compose, systemd units, otel collector, postgres init
docs/                    # this specification
```

The layout above is the target. **Build state (verify against `docs/08-roadmap.md`):** M0
and M1 are signed off (tags `m0-complete`, PR #4); the current milestone is **M2 (safe
actions)** — read `docs/milestones/M1-gate-report.md` and `docs/08` §1 before writing code.
Many crates are still thin: `jarvis-adapters` has only `claude_cli.rs`, `jarvis-agent` is a
stub, and the orchestrator's `match` on `RunState` (`crates/jarvis-application/src/orchestrator.rs`)
deliberately returns `UnwiredInM1` for the not-yet-built states (`ToolRunning`, `PolicyReview`,
`WaitingApproval`, `Replanning`) — keep those arms exhaustive; never add a `_` arm. Ports
(repository traits the domain depends on) live in `jarvis-application/src/ports.rs`; their
sqlx implementations are in `jarvis-infra`.

## Build & test loop

```bash
docker compose -f infra/compose/dev.yml up -d postgres   # start before DB tests
cargo build --workspace
cargo test --workspace                 # unit + contract + #[sqlx::test] DB tests
cargo test -p jarvis-application orchestrator   # one crate; add ::test_name to narrow further
cargo xtask arch-test                  # dependency-direction rules (NFR-08)
cargo xtask codegen --check            # generated web/ TS types must match contracts
cargo xtask golden                     # golden trace scenarios (fake adapters)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
(cd web && npm ci && npm run lint && npm test && npm run build)
```

`cargo xtask` is a `.cargo/config.toml` alias; subcommands are exactly `arch-test`,
`codegen [--check]`, `golden`. Toolchain is pinned to `1.94.1` by `rust-toolchain.toml`
(edition 2024) — CI runs exactly this; don't invoke a different toolchain.

**Database / sqlx.** `DATABASE_URL=postgres://jarvis:jarvis-dev-only@127.0.0.1:5432/jarvis`.
Compile-time query checking uses the committed `.sqlx/` offline cache (`SQLX_OFFLINE=true`),
so a plain `cargo build`/`cargo test` needs no DB. But `#[sqlx::test]` DB tests spin up
throwaway databases against a **live** Postgres, so start the compose `postgres` service
first. After changing any `sqlx::query!`/`query_as!` SQL, refresh the cache and commit it —
CI re-derives it from a migrated schema and diffs:
```bash
cargo sqlx prepare --workspace         # regenerate .sqlx (needs live DB + SQLX_OFFLINE=false)
```
Migrations live at repo-root `migrations/` (schema-per-module, ordered `NNNN_<module>_*.sql`)
and are applied by the `#[sqlx::test]` migrator and `sqlx migrate run` — not per-crate.

On low-power dev hosts use `cargo check` as the inner loop, mold as linker, and cache;
see `docs/09-operations.md` §5. CI is GitHub Actions (`.github/workflows/ci.yml`); every PR runs the full loop above plus
`cargo deny check` and container scans. Do not merge with warnings.

## Coding conventions

- Edition 2024, MSRV pinned in `rust-toolchain.toml`. `#![deny(unsafe_code)]` in every
  crate except `jarvis-agent` where a justified `unsafe` block requires a comment and test.
- Errors: `thiserror` per crate for domain errors; `anyhow` only in binaries. Every error
  crossing the API boundary maps to an RFC 9457 problem body with a stable machine code.
- IDs are ULIDs behind newtypes (`RunId`, `SessionId`, …). Never pass raw `String`/`Uuid`
  between modules.
- All wire DTOs live in `jarvis-contracts`, versioned, with `schemars` JSON Schemas
  generated by `cargo xtask codegen` — the Angular client types are generated from those
  schemas, never hand-written twice.
- Database access only through repository traits defined in `jarvis-application::ports`.
  SQL lives in `jarvis-infra` with sqlx compile-time checked queries.
- Tracing: every use case opens a span; span fields follow `docs/02-architecture.md` §7.
  Use `tracing`, never `println!`.
- Tests first for policy, grants, and state transitions. A state-machine change without a
  transition-table test is an incomplete PR.

## Model strategy and token discipline

The owner shares ONE subscription quota between building Jarvis and (later) running it.
Model selection is part of the engineering discipline, not a preference:

- **Strong model (Fable 5; Opus 4.8 if Fable is unavailable in this Claude Code):**
  `/milestone` decomposition, `/gate`, `/adr`, all of M0–M2, and ANY change touching
  `jarvis-domain` or `jarvis-application` (state machine, policy, grants, contracts).
  This is judgment work — ~20% of the tokens, ~80% of the risk. Switch with `/model`.
- **Sonnet 4.6:** `/feature` volume work — adapters, sqlx repositories, Angular
  components, fixtures, golden-trace harness, CI plumbing. The skills and contracts
  constrain this work tightly; the mid-tier model on a tight spec is the efficient
  choice.
- **Subagents are pinned via `model:` frontmatter** and keep their own isolated context:
  security-auditor and rust-reviewer run opus (review is cheap in tokens, high in
  value — never economize there); contract-keeper, test-architect, doc-syncer run
  sonnet; perf-warden runs haiku (it mostly executes measurement commands).

Token discipline:
- Do NOT read the whole docs tree at session start. `/milestone` and `/feature` name the
  exact sections to read; read those, plus the matching skill, and stop.
- One feature per session; long sessions accumulate stale context — prefer a fresh
  session with the feature list over continuing a bloated one.
- Batch `/sync-docs` weekly rather than after every merge when quota is tight.
- Fixture-driven tests over live-provider calls, always (this is also the correctness
  rule — it happens to be the cheap rule too).

## Development process

The project is built through four nested loops defined in `docs/11-development-process.md`
and encoded as slash commands: `/milestone` (decompose + drive a milestone),
`/feature` (spec → threat note → tests → implement → review → DoD), `/gate` (exit
evidence + report for human sign-off), `/golden`, `/security-review`, `/adr`,
`/sync-docs`. Subagents in `.claude/agents/` (rust-reviewer, security-auditor,
contract-keeper, test-architect, perf-warden, doc-syncer) review — they never merge,
never relax budgets, never accept ADRs. Skills in `.claude/skills/` carry the
area-specific know-how (state machine, grants, adapters, sqlx, contracts, golden traces,
low-power, Angular) — consult the matching skill before touching its area.

Human-only decisions (docs/11 §3): milestone feature lists, gate sign-offs, ADR
acceptance, changes to the invariants above, new domain/application dependencies, budget
relaxations. When in doubt, stop and ask rather than assume.

**Hardware note:** the deployment target may be an 8 GB ultrabook — the low-power rules
(`docs/09` §5) are defaults, and the perf budget (`docs/01` §4.1) uses the 8 GB numbers.

## When requirements are ambiguous

Prefer the stricter security interpretation, the simpler operational interpretation, and
the more observable implementation — in that order. If a design doc conflicts with an ADR,
the ADR wins; note the conflict in the PR description so the doc gets fixed.
