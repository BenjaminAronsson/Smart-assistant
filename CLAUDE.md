# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository state

This repository currently contains **only the design/specification baseline** for a
project called Jarvis — there is no Rust or Angular implementation code at the repo root
yet (pre-M0). Everything lives under `jarvis-design/`:

```
jarvis-design/
  CLAUDE.md          # implementation instructions for the future Jarvis codebase
  README.md          # design overview, document map, decision summary
  docs/              # the full spec: vision, requirements, architecture, tech stack,
                      # data model, API/WS contracts, security, testing, roadmap,
                      # operations, ADRs, UI design, use-case catalog
  .claude/           # subagents, skills, and slash commands meant for the future repo
```

Per `jarvis-design/README.md`, the intended workflow is: create the actual
implementation repo, copy `jarvis-design/docs/`, `jarvis-design/CLAUDE.md`, and
`jarvis-design/.claude/` into that repo's root, then drive development with
`/milestone` → `/feature` → `/gate` there. **That migration has not happened yet** —
so until `jarvis-design/` is promoted to the root (or a separate implementation repo),
there is no build/lint/test loop to run at this root level.

## Working in this repo right now

- If asked to implement Jarvis functionality (Rust crates, Angular UI, etc.), read
  `jarvis-design/CLAUDE.md` first — it is the authoritative instruction set (invariants,
  workspace layout, build/test commands, coding conventions, model strategy, dev process)
  and was written to govern exactly that work.
- If asked to edit or extend the *specification* (anything under `jarvis-design/docs/`),
  treat `docs/08-roadmap.md` (milestones/exit evidence) and `docs/11-development-process.md`
  (the four nested loops: `/milestone`, `/feature`, `/gate`, `/sync-docs`, plus ADRs) as
  the anchor documents, and check `docs/adr/README.md` before contradicting an existing
  ADR — ADRs win over prose docs.
- Token discipline applies even at the design-review stage: don't read the whole `docs/`
  tree speculatively — `jarvis-design/docs/10-references.md`'s document map (mirrored in
  `jarvis-design/README.md`) tells you which numbered doc covers which concern; go straight
  to it.

## The project in one paragraph

Jarvis is a local-first personal assistant: a small, deterministic Rust core (`jarvisd`)
that owns policy, orchestration, memory, artifacts, and audit, with Home Assistant,
Wyoming voice services, MCP tool servers, and the Claude Code CLI wired in as replaceable
edge adapters behind typed ports. An Angular web shell renders conversation, run timeline,
approvals, and artifacts over a versioned WebSocket protocol. The core design invariant
(`jarvis-design/CLAUDE.md` §"Non-negotiable invariants"): nothing the model says or any
untrusted content says ever grants authority — only the policy engine and, from M2 on,
exact expiring `ExecutionGrant`s can trigger a side effect.
