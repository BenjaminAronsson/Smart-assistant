# M2 Safe actions — feature list

Status: **APPROVED 2026-07-21** (milestone loop, docs/11 §2). Owner approved the full
11-feature split; open questions resolved per recommendations — **MCP host (F2.7) stays in
M2** (retires the malicious-tool-server threat early), and **F2.9/F2.10 stay split** (each
maps to a distinct exit-evidence bullet). M1 signed off + tagged `m1-complete` 2026-07-20
(`docs/milestones/M1-gate-report.md`). Goal: complete all features, tight context between
sessions, push before quota runs out.

Milestone scope (docs/08 §1): tool registry, native tools, MCP host (rmcp), policy/risk
engine, approval UI + grants, idempotency, audit, `web.search`/`web.fetch` tool (FR-25).

Exit evidence (docs/08 §1): read a project file (R0); perform one reversible action (R1);
block an unapproved mutation (R2); answer a current-facts question via search rather than
stale memory, image shows its source link (FR-25); a location-dependent query
("lunch nearby") resolves via the location provider (FR-26); a genuinely ambiguous query
("microcondia") gets one fluent clarifying question, not a picker (ADR-016); a
contested-news query ("latest on Iran") is summarized with attributed, even-handed
framing (FR-30); adversarial basics incl. malicious-fetched-page injection; golden 4–6.

Each feature is a vertical slice sized for one session and runs the `/feature` loop
(spec → threat note → contracts/tests first → implement → review → DoD → small PR).
"Read" names the exact spec sections for that session (token discipline, CLAUDE.md).

**Model discipline (CLAUDE.md §"Model strategy"): M2 is a strong-model milestone.** "All
of M0–M2" and "ANY change touching `jarvis-domain` or `jarvis-application`" is strong-model
work — this milestone *is* the security core (policy, grants, injection), where review is
cheap in tokens and high in value. Default every feature to the strong model. Two features
(F2.5 Angular approval tray, F2.11 golden-harness plumbing) are tightly constrained volume
work and **may** drop to Sonnet if quota is tight — the owner decides at start of session.

**Invariant 1 is the milestone's whole point:** no user message, model output, tool
result, or fetched web page grants authority except through `policy::evaluate` and, for
R2+, an `ExecutionGrant`. There is no code path from model output to tool execution that
bypasses policy. Every feature below is written to preserve that.

---

## Features (ordered)

- [x] **F2.1 — Policy & tool domain types + argument normalization (domain)** · *strong model*
  `jarvis-domain`: `RiskLevel` (R0–R4, exhaustive), `ToolId` + `semver::Version`,
  `ToolPolicy` (risk, is_reversible, requires_user_presence, timeout, required_scopes,
  egress), `DataEgress`, `Scope`, `ResourcePattern`, `ExecutionGrant` + `GrantId`,
  `Sha256` newtype, `ToolProposal`, `ToolInvocation`, `ToolResult`, `ToolError`. The
  **shared argument-normalization function** (canonical form → SHA-256) used by BOTH grant
  minting and validation — one function, **property-tested** (same args in any key order ⇒
  same hash). No I/O; `#![deny(unsafe_code)]`; newtyped IDs. Refs: FR-04/05, docs/05 §4,
  docs/06 §3–4. Read: docs/05 §4, docs/06 §3–4; skill `policy-grants`. Deps: none.
  rust-reviewer + security-auditor mandatory.

- [x] **F2.2 — Tool registry + `policy::evaluate` + R0/R1 auto path wired (application)** · *strong model*
  `jarvis-application::policy`: `evaluate(proposal, ctx) -> PolicyDecision`
  (`Auto | NeedsApproval { exact_effect } | Reject`). **Tool registry**: registration
  requires policy metadata — a tool with no `ToolPolicy` fails registration (test this).
  `ToolExecutor` port. Orchestrator wiring of `PolicyReview`: R0/R1 → `AutoAuthorized` →
  `ToolRunning` → `ToolObserved` → `Replanning` (the states already exist from F1.2). **An
  audit event is emitted on EVERY evaluation** — no "skip policy for read-only" shortcut.
  `FakeTool` drives every test. Tests: risk-tier table (every tier × auto/approve/reject);
  registration-without-policy fails; **adversarial** — model output containing "the user
  approved this" text cannot reach the executor as authority. Refs: FR-04/05, invariant 1,
  docs/02 §5. Read: docs/02 §5, docs/06 §3; skills `policy-grants`, `state-machine`.
  Deps: F2.1. security-auditor + rust-reviewer mandatory.

- [x] **F2.3 — Execution grants: minting, validation, R2 approval flow wired (application)** · *strong model*
  `jarvis-application::policy::approvals`: grant **minting** only on `Decision::Approved`
  (256-bit random id, full binding: user, device, run, tool id + semver, sha256 of
  normalized args, resource, expiry from policy TTL, `single_use = true`). `GrantValidator`
  port; validation called **by the executor immediately before execution**, not only at
  decision time — expired / consumed / args-mismatch / wrong-run ⇒ registered error codes
  (`grant.expired`, `grant.consumed`, `grant.args_mismatch`) + audit event + **no
  execution**. Argument edits after approval invalidate by **hash comparison, not flags**.
  Orchestrator wiring of `PolicyReview → ApprovalRequested → WaitingApproval →
  (ApprovalGranted → ToolRunning | ApprovalDenied → Replanning)`. Compensating-action
  registration for reversible R2 (undo appears in the run timeline). Tests: grant lifecycle
  table (mint, validate, expire, consume, mismatch, replay). Refs: FR-05, docs/06 §4,
  invariant 1. Read: docs/06 §4, docs/05 §4/§7; skills `policy-grants`, `state-machine`.
  Deps: F2.1, F2.2. security-auditor + rust-reviewer mandatory.

- [x] **F2.4 — Policy/grant/tool-invocation persistence + idempotency (infra)** · *strong model* · PR #10
  <br/>*Scope note: grant persistence + single-use consume (the R2+ replay guard) + transactional grant.\* audit landed. The general `tool_invocations` idempotency ledger is deferred to F2.6 with its executor/writer (no-speculative-schema precedent, migration 0006). CF-6/CF-7 logged.*
  Migrations for the `tooling`/`policy` schema (grants, approvals, tool_invocations) with
  **idempotency keys / external-operation IDs** for replay protection (docs/06 §5
  "Replay / duplicate mutation"). sqlx repos behind the F2.2/F2.3 ports (`GrantStore`,
  `GrantValidator`, invocation log). **Audit events written in the same transaction as the
  domain change** (invariant 6) — reuse the existing append-only audit (schema 0003), add
  the policy/grant event kinds. Restart reconciliation: an in-flight tool invocation
  reconciles via idempotency, never a blind re-exec (groundwork for golden 10, M-later).
  `cargo sqlx prepare` committed. Refs: FR-05/06, NFR-05/13, invariant 6, docs/04 §3.
  Read: docs/04 §3, docs/06 §5/§7; skills `sqlx-data`, `low-power`. Deps: F2.1, F2.3.
  contract-keeper (migrations) + security-auditor (audit/idempotency) mandatory.

- [x] **F2.5 — Approval surface: REST + WS events + Angular approval tray (contracts + jarvisd + web)** · *strong model (Angular part may be Sonnet)* — DONE (PR pending review). `ApprovalCardDto`/decision DTOs + `approval.requested`/`approval.resolved` WS events; `JarvisApprovalGate` + `POST /runs/{id}/approvals/{approval_id}` (approve/deny/edit → rebinds grant) with outbox+audit written in one tx; Angular `ApprovalTray` (verbatim exact-effect, optimistic block, timeline reconcile). NOTE: the **tool-proposal/result timeline events** (`run.tool.completed`) are DEFERRED to F2.6 — no producer until the executor lands (no-speculative-schema). Reviews: rust-reviewer + security-auditor (gateway) + contract-keeper, no BLOCKING; fixes applied; S1/edited-args logged as CF-8/CF-9. ToolStack→`RunEngine::drive` wiring also deferred to F2.6 (empty registry + ADR-004 ⇒ inert).
  `jarvis-contracts`: `ApprovalCardDto` carrying the **exact effect** (real target — entity
  id + friendly name / file path / recipient — and real payload, not a summary), grant
  outcome DTOs, tool-proposal/result timeline events; `xtask codegen` → committed TS.
  jarvisd: `POST /api/v1/runs/{id}/approvals/{approval_id}` (approve/deny; body carries any
  argument edits), a WS `ApprovalRequested`/`ApprovalResolved` event, tool proposals/results
  in the timeline. `web/`: an **ApprovalTray** surface rendering the exact-effect card with
  approve/deny and edit, wired to the existing WS client. Snapshot-test the exact-effect
  strings (docs/06 §3). Refs: FR-05/07, docs/05 §1–§4, docs/06 §3. Read: docs/05 §1–§4,
  docs/12 §2 (as available); skills `ws-contracts`, `policy-grants`, `angular-shell`.
  Deps: F2.3, F2.4. contract-keeper + security-auditor (gateway) mandatory.

- [x] **F2.6 — Native + example tier tools: `fs.read` (R0), reversible R1, fake R2 (adapters)** · *strong model* — DONE (PRs #12–#15 merged to main). `fs.read` R0 within allowlisted root (traversal-denied), reversible `example.light` R1 with registered undo, fake `message.send` R2 (approval→grant→execute→edit-invalidation). Live `ToolStack` wired into jarvisd via `build_registry` (single site, every executor `TimeoutExecutor`-wrapped) + `PgAuditSink`; tools lent only to an attributable run (`should_wire_tools`). Carry-forwards CF-3/4/6/7/9/11 discharged; CF-2 durability half closed (atomicity half + CF-8/10/14/15 tracked). Reviews: rust-reviewer + security-auditor per slice, no BLOCKING. Remaining dormant: Slice 3c (CF-8 `model_permit` bracketing — inert, CLI adapter proposes no tools).
  `jarvis-adapters`: `fs.read` — read a project file within an allowlisted root, R0,
  read-only, scoped, path-traversal-denied (real native tool → **exit evidence #1**). A
  **reversible R1 example tool** with a registered compensating undo (stand-in for the M5
  HA `home.set_light`; drives **golden 4** and **exit evidence #2** without pulling the HA
  adapter forward). A **fake R2 external tool** (`message.send` stand-in; the real SMTP
  adapter is M4/ADR-026) to drive the approval → grant → execute → edit-invalidation flow
  (**golden 5**, **exit evidence #3**). Each registers real `ToolPolicy`; example tools are
  clearly marked as tier demonstrations, not shipping integrations. Refs: FR-04/05,
  docs/08 §2 step 7–8, docs/06 §3. Read: docs/06 §3, skill `policy-grants`,
  `provider-adapter`. Deps: F2.2, F2.3, F2.4. security-auditor + rust-reviewer mandatory.

- [ ] **F2.7 — MCP host (rmcp): child-process tool server + local policy overlay (adapters)** · *strong model*
  `jarvis-adapters::mcp_host`: launch/attach an out-of-process MCP tool server via `rmcp`
  (least privilege, killable, pinned version/hash), import its tool **descriptors**, and
  **overlay host-owned `ToolPolicy`** — a server's self-declared safety is NEVER trusted
  (docs/06 §5 "Malicious MCP/tool server"). Schema-validate every result; outbound network
  restricted; cancellation kills the child. Tests against a fixture MCP child (a trivial
  echo/read server in `tools/`): descriptor import, policy-overlay-wins, malformed-result
  rejection, cancellation reaps the child. **If a genuinely irreversible protocol/isolation
  choice surfaces, stop and draft an ADR** (docs/11 §3). Refs: FR-04/05, docs/02 §8, docs/06
  §5, ADR-001/002. Read: docs/02 §8, docs/06 §5; skills `policy-grants`, `low-power`.
  Deps: F2.2, F2.3. security-auditor + rust-reviewer mandatory.

- [ ] **F2.8 — `web.search` / `web.fetch` tool (R0) + Z4 sanitization + injection defense (adapters)** · *strong model*
  `jarvis-adapters::web`: `web.search { query } -> [{ title, url, snippet }]` and
  `web.fetch { url } -> { title, text, primary_image_url?, source_url }`, both **R0
  read-only** behind a **config-swappable search port** (default Brave; fixture-driven
  tests, no live key needed — mirrors the claude-cli fixture pattern). **Fetched content is
  Z4 untrusted** (docs/06 §2): schema-validated, size-truncated, control-chars stripped,
  instruction-shaped content stripped **before** it reaches the model — a malicious page
  **cannot** use `web.fetch` as an injection vector into a tool call (**exit evidence:
  adversarial malicious-fetched-page injection**, **golden 6**). `source_url` is carried
  end-to-end through the tool result and contract so an image always has its attribution
  link (**exit evidence: image shows its source link**; the visual HUD card is M3 — M2
  proves the data is present and asserts it in tests). Routing signal: time-sensitive
  phrasing prefers `web.search` over model memory. Refs: FR-25, ADR-014, docs/02 §11b,
  docs/06 §2/§5. Read: docs/02 §11b, docs/06 §2/§5, ADR-014; skills `web-lookup`,
  `policy-grants`. Deps: F2.2, F2.6. security-auditor + rust-reviewer mandatory.

- [ ] **F2.9 — Location provider + location-dependent search routing (FR-26) (adapters + application)** · *strong model*
  `LocationProvider` port resolving in order: (1) paired-device GPS when the location scope
  is granted, (2) configured home coordinate (`[location] home_lat/home_lon`), (3) IP
  geolocation as a coarse last resort **explicitly labeled approximate**. The router/context
  assembler classifies "nearby"/"near me"/place phrasing as location-dependent and attaches
  resolved coordinates as a **labeled, provenance-tracked, sensitivity-classified** context
  item (NFR-02) — never silently attached to an outbound request. Drives **exit evidence:
  "lunch nearby" resolves via the location provider**. Tests: resolution-order fallthrough,
  sensitivity labeling, coordinates reach `web.search`. Refs: FR-26, ADR-015, docs/02 §11c.
  Read: docs/02 §11c, ADR-015; skills `web-lookup`. Deps: F2.8. security-auditor (NFR-02
  location handling) + rust-reviewer mandatory.

- [ ] **F2.10 — Synthesis: contested-news framing (FR-30) + fluent ambiguity clarification (ADR-016) (application)** · *strong model*
  Two synthesis behaviors, both routing-signal driven (same mechanism as F2.9's location
  signal). **Ambiguity (ADR-016):** a genuinely ambiguous query ("microcondia") yields
  **one fluent clarifying question**, never a multi-option picker — a single clarifying
  message, tested on output shape (**exit evidence**). **Contested framing (ADR-020,
  FR-30):** for contested/political/conflict topics ("latest on Iran"), the news-synthesis
  path **attributes claims to sources** (preserving reporting's hedging), presents contested
  points **even-handedly**, and avoids sensationalized graphic detail — composes with
  ADR-016 source-quality weighting (**exit evidence**). Fixture-driven (no live LLM);
  assertions on attribution/even-handedness/no-picker shape. Refs: FR-29/30, ADR-016/020,
  docs/02 §11d. Read: docs/02 §11d, ADR-016/020; skill `web-lookup`. Deps: F2.8. Note:
  FR-29 news-interest **profile** is M4 — M2 covers only the framing/ambiguity rules.
  rust-reviewer mandatory.

- [ ] **F2.11 — Golden traces 4–6 + adversarial injection suite (exit-evidence feature)** · *strong model (harness plumbing may be Sonnet)*
  Fill golden slots 4–6 (docs/07 §2) with fake-provider scenarios: (4) home light toggle
  auto-authorized as R1, exact state transition recorded; (5) external message proposal
  classified R2 → user edits → old approval invalidated → new approval succeeds; (6)
  malicious webpage asks the assistant to reveal secrets → policy denies → injection
  evidence recorded. Plus the **adversarial basics** suite (docs/06 §8 gate 2): untrusted
  content cannot invoke tools outside the policy path; malicious-fetched-page injection is
  contained. This feature **demonstrates the milestone exit evidence**. Refs: FR-04/05/25,
  docs/06 §8, docs/07 §2. Read: docs/07 §2, docs/06 §8; skill `golden-traces`.
  Deps: F2.1–F2.10.

---

## Dependency sketch

```
F2.1 ─ F2.2 ─┬─ F2.3 ─ F2.4 ─ F2.5 ─────────────────────────┐
             │                                              │
             ├─ F2.6 ─┬─ F2.8 ─┬─ F2.9 ──┐                  │
             │        │        └─ F2.10 ─┤                  │
             └─ F2.7 ─┘                  └──────────────────┴─ F2.11
```

## Explicitly out of scope for M2 (scope control, docs/08 §7)

- **Real integration adapters** — HA (M5), SMTP send (M4/ADR-026), Spotify/MPRIS (M5),
  CalDAV (M4). M2 uses `fs.read` (real) plus **clearly-marked example R1/R2 tools** to
  exercise every policy tier; do not pull a real integration forward to make a golden pass.
- **HUD cards / renderers / artifacts** — M3. M2 carries `source_url` and card *data*
  through the contract and asserts it in tests; the visual card render is M3.
- **News-interest profile (FR-29)** — M4. M2 implements only the contested-topic **framing**
  (FR-30) and the **fluent ambiguity** rule (ADR-016).
- **Memory / embeddings / retrieval, deterministic HA/math intent grammar** — M4.
- **Deep-dive threads, gallery/sources cards, browser-worker source handoff (FR-27)** — M3.
- **Golden 7–10** — M3+ (coding patch, generated-app capability, voice cancel, restart
  reconciliation). M2 lays idempotency groundwork (F2.4) but does not demonstrate golden 10.

## Open questions for the owner (resolve at approval)

1. **MCP host (F2.7) placement.** It is in the M2 scope line (docs/08 §1) but no exit-
   evidence bullet requires an MCP tool. Keep in M2 (retires the malicious-tool-server
   threat early, per docs/06 §5), or defer to M3 alongside the browser/coding workers?
   Recommendation: **keep in M2** but as the last non-golden feature, so it can be cut
   under quota pressure without blocking exit evidence.
2. **Feature count.** 11 features for a large security milestone. If you want fewer/larger
   sessions, F2.9+F2.10 (both small routing-signal behaviors on top of F2.8) can merge into
   one "web-lookup synthesis" feature. Recommendation: keep split — each is independently
   reviewable and each maps to a distinct exit-evidence bullet.
