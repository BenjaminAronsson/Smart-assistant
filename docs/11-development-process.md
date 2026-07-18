# 11 — Development process: loops, agents, gates

This document defines *how* the project gets built: the nested loops Claude Code runs,
the subagents that review, the skills that encode domain know-how, and the points where
the human owner must be in the loop. The scaffolding lives in `.claude/`.

## 1. The four nested loops

```
┌─ MILESTONE LOOP (/milestone) ── weeks ─────────────────────────────┐
│  pick next milestone → decompose into features → run feature loops │
│  → /gate → human sign-off → tag                                    │
│                                                                    │
│  ┌─ FEATURE LOOP (/feature) ── hours-days ──────────────────────┐  │
│  │  read spec → threat/risk note → contracts & tests FIRST      │  │
│  │  → implement → INNER LOOP → subagent reviews → DoD check     │  │
│  │  → small vertical PR                                         │  │
│  │                                                              │  │
│  │  ┌─ INNER LOOP ── minutes ────────────────────────────────┐  │  │
│  │  │  edit → cargo check → cargo test -p <crate> → repeat   │  │  │
│  │  │  (fmt+clippy on save via hooks; full suite pre-PR)     │  │  │
│  │  └────────────────────────────────────────────────────────┘  │  │
│  └──────────────────────────────────────────────────────────────┘  │
│                                                                    │
│  ┌─ DRIFT LOOP (/sync-docs) ── after each merged feature ───────┐  │
│  │  reconcile docs/ADRs with reality; new decision → /adr       │  │
│  └──────────────────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────────────────┘
```

## 2. Loop definitions

### Inner loop (no command — this is just how work happens)
`cargo check` → targeted `cargo test -p <crate>` → repeat. Hooks run `fmt` on edit.
Full `cargo test --workspace` + `clippy -D warnings` + `xtask arch-test` before any PR.
On this hardware the inner loop is seconds; the workspace loop is minutes — structure
work so the workspace loop runs a few times per feature, not per edit.

### Feature loop — `/feature <description or FR-id>`
Sequenced per the DoD (`07` §3). Order is mandatory:
1. Locate the governing spec sections and FR/NFR; if none exist, **stop** — the feature
   needs a spec or an ADR first, not code.
2. Write the threat/risk note (2–10 lines) — which trust zones, which risk tiers, what
   the failure states look like to the user.
3. Contracts first: DTOs/ports/schema changes, then `xtask codegen`, commit generated
   types.
4. Tests first for anything touching `jarvis-domain`/`jarvis-application`: transition
   tables, policy tables, contract fixtures.
5. Implement to green. Skills auto-apply by area (state machine, grants, adapters, sqlx,
   contracts, Angular).
6. Reviews: `rust-reviewer` on every feature; `security-auditor` whenever the diff
   touches policy/grants/tools/adapters/gateway; `contract-keeper` whenever
   `jarvis-contracts` or migrations change; `perf-warden` whenever a resident component,
   dependency, or loop is added.
7. DoD checklist; then a small vertical PR with the threat note in its description.

### Milestone loop — `/milestone`
Reads `08` §1, finds the first milestone whose exit evidence isn't demonstrated,
decomposes it into an ordered feature list (posted for human approval **before**
implementation begins), then runs feature loops. Ends with `/gate`.

### Gate loop — `/gate`
Runs the milestone's exit evidence end-to-end: golden traces for that milestone, security
release gates (`06` §8) that apply, perf assertions against the ultrabook budget
(`01` §4.1), and produces a gate report. **A human approves every gate.** Failed
assertions are features, not footnotes — they go back into the feature loop.

### Drift loop — `/sync-docs`
After each merged feature: does `02`–`09` still describe reality? Small corrections are
applied directly; anything that changes a *decision* goes through `/adr` instead. The
ADR wins over prose on conflict — the drift loop's job is to make that situation rare.

## 3. Human-in-the-loop points (non-delegable)

| Moment | Why |
|---|---|
| Milestone feature-list approval | Scope control — the #1 project risk (`08` §7). |
| Every `/gate` sign-off | Exit evidence is judged, not just measured. |
| Every ADR acceptance | Irreversible decisions are the owner's. |
| Any change to the six CLAUDE.md invariants | These are the product. |
| Any new dependency in `jarvis-domain`/`jarvis-application` | Purity rule. |
| Any relaxation of a budget (latency, RSS, quota) | Recorded with rationale at the gate. |
| First run of any R2/R3 tool against a real target | Dry-run review. |

Review strategy for a Rust-learning owner: concentrate on `jarvis-domain` and
`jarvis-application` diffs (small, pure, test-mirrored); trust the compiler + CI +
subagents for infra crates. Ask Claude Code to annotate any diff on request.

## 4. Subagents (`.claude/agents/`)

| Agent | Trigger | Model | Charter |
|---|---|---|---|
| `rust-reviewer` | every feature | opus | Idiomatic Rust, error handling, cancellation correctness, dependency rule, no `unsafe`. |
| `security-auditor` | policy/grants/tools/adapters/gateway diffs; `/security-review` | opus | The six invariants; threat table (`06` §5); adversarial thinking on the diff. |
| `contract-keeper` | `jarvis-contracts`/migrations diffs | sonnet | Versioning discipline, codegen freshness, additive-only evolution, fixture coverage. |
| `test-architect` | start of each feature | sonnet | Turns spec sections into failing tests before implementation. |
| `perf-warden` | new resident components/deps/loops; `/gate` | haiku | Ultrabook budget (`01` §4.1), low-power defaults (`09` §5), idle = event-driven. |
| `doc-syncer` | `/sync-docs`; post-merge | sonnet | Doc/reality reconciliation; drafts ADRs for `/adr`. |

Model strategy for the main session (full rationale in CLAUDE.md): strong model
(Fable 5 / Opus-class) for `/milestone`, `/gate`, `/adr`, M0–M2, and any
domain/application change; Sonnet 4.6 for `/feature` volume work in infra, adapters, and
the web shell. Review agents never economize; implementation sessions do.

Agents are reviewers and drafters — they never merge, never relax a budget, never edit
ADRs to Accepted status.

## 5. Skills (`.claude/skills/`)

Domain know-how that auto-loads when work touches its area: `state-machine`,
`policy-grants`, `provider-adapter`, `sqlx-data`, `ws-contracts`, `golden-traces`,
`low-power`, `angular-shell`, `media-integration`, `web-lookup`. Each SKILL.md is the *operational* companion to a spec
section — the spec says what must be true; the skill says how to build it here.

## 6. Commands (`.claude/commands/`)

`/milestone` · `/feature` · `/gate` · `/golden` · `/security-review` · `/adr` ·
`/sync-docs`. Defined as slash commands so the loops are one keystroke, not tribal
knowledge.

## 7. Session hygiene for a long project

- Start sessions with the milestone command or a specific feature — not "continue".
  State lives in the repo (specs, TODO comments are banned; open items live in the
  milestone feature list committed under `docs/milestones/`).
- One feature per branch per session where possible; the feature loop is sized to fit a
  session.
- When quota is tight (shared with Jarvis itself once it's running): prioritize feature
  loops over drift loops; batch `/sync-docs` weekly.
