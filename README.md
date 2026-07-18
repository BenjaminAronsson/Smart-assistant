# Jarvis — Local-First Personal Assistant

**Design baseline v4.1 (final handover) · 17 July 2026 · Rust core, Claude-CLI-only provider**

Jarvis is a personal, local-first operating layer: it accepts text or voice, understands
active context, plans bounded work, requests approval for consequential actions, executes
typed tools, presents live results on one or more displays, and remembers only what policy
permits.

This repository contains the complete implementation-ready design. It supersedes the
.NET-based *Jarvis Technical Design (v1)* document; the material change is the core
technology decision (Rust replaces .NET — see [ADR-001](docs/adr/README.md#adr-001)) and a
requirements-first reorganization. The security model, orchestration model, and reuse
strategy from v1 are retained and tightened.

## How to use this with Claude Code

1. Create the implementation repo and copy `docs/`, `CLAUDE.md`, and `.claude/` into its
   root. The `.claude/` tree ships the subagents, skills, slash commands, and
   permission/hook settings — the loops are one keystroke.
2. Open Claude Code in the repo and run `/milestone`. It decomposes M0 into a feature
   list and stops for your approval before writing any code.
3. Drive features with `/feature`, close milestones with `/gate` (you sign off every
   gate), keep docs truthful with `/sync-docs`, record decisions with `/adr`.
4. The process, loops, and your non-delegable decision points are defined in
   [docs/11-development-process.md](docs/11-development-process.md).

## Document map

| File | Contents |
|---|---|
| [`CLAUDE.md`](CLAUDE.md) | Project instructions for Claude Code: conventions, commands, guardrails. |
| [`docs/00-vision.md`](docs/00-vision.md) | Problem, product definition, design principles, non-goals. |
| [`docs/01-requirements.md`](docs/01-requirements.md) | Assumptions, functional & non-functional requirements, acceptance criteria, hardware sizing. |
| [`docs/02-architecture.md`](docs/02-architecture.md) | System architecture, crate/module boundaries, runtime flows, deployment topology. |
| [`docs/03-tech-stack.md`](docs/03-tech-stack.md) | Rust stack in detail: crates, patterns, and the .NET comparison. |
| [`docs/04-data-model.md`](docs/04-data-model.md) | Entities, PostgreSQL schemas, artifact store. |
| [`docs/05-api-contracts.md`](docs/05-api-contracts.md) | REST endpoints, WebSocket event protocol, core Rust contracts. |
| [`docs/06-security.md`](docs/06-security.md) | Trust zones, threat model, risk tiers, execution grants, release gates. |
| [`docs/07-testing.md`](docs/07-testing.md) | Test pyramid, golden traces, definition of done. |
| [`docs/08-roadmap.md`](docs/08-roadmap.md) | Milestones M0–M8, first slice, handover checklist, deferred decisions, risks. |
| [`docs/09-operations.md`](docs/09-operations.md) | Configuration reference, deployment units, backup/restore, runbooks. |
| [`docs/10-references.md`](docs/10-references.md) | External sources and Rust-stack references. |
| [`docs/11-development-process.md`](docs/11-development-process.md) | The four nested build loops, subagents, gates, human-in-the-loop points. |
| [`docs/13-use-case-catalog.md`](docs/13-use-case-catalog.md) | ~50 realistic interactions validated against the design; source for golden traces and acceptance walks. |
| [`docs/12-ui-design.md`](docs/12-ui-design.md) | UI design (normative): voice-first HUD, card grammar, panel lifecycle, backgrounds, real maps. |
| [`docs/design-refs/`](docs/design-refs/) | Working HTML design references — `jarvis-hud-final.html` is the intended feel; earlier iterations kept for history. |
| [`.claude/`](.claude/) | Claude Code scaffolding: 6 subagents, 8 skills, 7 slash-command workflows, settings + hooks. |
| [`docs/adr/README.md`](docs/adr/README.md) | Architecture decision records (ADR-001 … ADR-026). |

## The decision in one paragraph

Build a small, deterministic **Rust core** (`jarvisd`) that owns policy, orchestration,
memory, artifacts, and audit. Reuse Home Assistant, Wyoming voice services, MCP tool
servers, Ollama/llama.cpp, and the Claude Code CLI strictly as replaceable edge adapters
behind typed boundaries. The Angular web shell renders conversation, run timeline,
approvals, and artifacts over a versioned WebSocket protocol. Nothing the model says, and
nothing any untrusted content says, ever grants authority — authority comes only from
authenticated identity, policy rules, and exact expiring execution grants.

## Status

Architecture baseline, ready for M0. Licenses, provider terms, and model capabilities
change; re-verify external references before redistribution. This is technical guidance,
not legal advice.
