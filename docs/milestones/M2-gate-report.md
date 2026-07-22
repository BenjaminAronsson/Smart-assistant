# M2 "Safe actions" — Gate Report

**Status: AWAITING HUMAN SIGN-OFF** · Prepared 2026-07-22 · Milestone loop docs/11 §2

Milestone M2 is feature-complete on `main` (all 11 features F2.1–F2.11 merged, PRs
#7–#21). This report presents the exit evidence, gate results, deviations, and open
risks for owner sign-off. **A gate is never "passed with exceptions" silently — the
deviations below are explicit and require an accept/reject decision.**

Scope since `m1-complete`: **58 commits, 91 files, +11 848 / −249 lines.** New shipped
dependencies (all in `jarvis-adapters`): `rmcp` 2.2.0 (client + child-process transport),
`reqwest` 0.12 (rustls-tls/ring, json, http2), `tl` 0.7.8 (pure-Rust HTML). Pure
domain/application additions (`location`, `synthesis`) added no dependencies.

---

## 1. Exit evidence (docs/08 §1) → result

| # | Exit-evidence item | Result | Evidence |
|---|---|---|---|
| 1 | Read a project file (**R0**) | ✅ MET | `fs.read` adapter (reads within allowlisted root, path-traversal + symlink-escape denied); `policy_tests::r0_tool_proposal_auto_executes_and_replans_to_completed` drives the R0 auto path end-to-end through the orchestrator. Golden 4. |
| 2 | Perform one reversible action (**R1**) | ✅ MET | `example.light` R1 with a registered compensating undo; `approval_tests::reversible_tool_registers_a_compensation_in_the_timeline`. Golden 4. |
| 3 | Block an unapproved mutation (**R2**) | ✅ MET | `message.send` R2 → parks for approval; `approval_tests::denied_r2_never_executes_and_replans`; edit-invalidation by arg-hash (`edited_arguments_bind_the_grant`, `malformed_edited_arguments_are_rejected`). Golden 5. |
| 4 | Answer a current-facts question via search; image shows its source link (FR-25) | ⚠️ MET (tool + Z4 proven; live real-model routing deferred — see Deviation D1) | `web.search`/`web.fetch` R0 behind config-swappable ports + live Brave/reqwest; `web.rs` tests prove sanitised results, title/**source_url**/og-image extraction end-to-end. `web.fetch` exercised through the real orchestrator in golden 6. |
| 5 | Location-dependent "lunch nearby" resolves via the location provider (FR-26) | ⚠️ MET (primitive-level; orchestrator injection deferred — D1) | `LayeredLocationProvider` (device→home→IP fallthrough, tested); `localize_query("lunch nearby", home)` attaches coordinates; sensitivity-labeled (NFR-02); IP source labeled Approximate. |
| 6 | Ambiguous "microcondia" → one fluent clarifying question, not a picker (ADR-016) | ⚠️ MET (primitive-level — D1) | `synthesis::clarifying_question` yields one single-line sentence (whitespace-collapse guarantees no picker); `None` for <2 distinct interpretations. |
| 7 | Contested-news "latest on Iran" → attributed, even-handed framing (FR-30) | ⚠️ MET (primitive-level — D1) | `synthesis::is_contested_topic` (token-level, by topic *kind*) + `frame_contested` attributes every claim to its source, never bare assertion; empty→empty. |
| 8 | Adversarial basics incl. malicious-fetched-page injection | ✅ MET | `adversarial_tests` golden 6: a malicious `web.fetch` page commanding exfiltration → the injected R2 `message.send` parks and is denied (exact audited sequence pinned); a page-named unknown tool is rejected and fails the run **closed**. Composes with F2.2/F2.6 model-text-in-args tests and F2.8 page sanitisation. |
| 9 | Golden 4–6 | ✅ MET | `cargo xtask golden` reports **traces 1–6 pass**. |

---

## 2. Gate runs (on merged `main`)

| Gate | Result |
|---|---|
| Full workspace suite (`cargo test --workspace`, incl. `#[sqlx::test]` vs live Postgres) | ✅ **433 passed, 0 failed** |
| `cargo xtask golden` | ✅ traces **1–6 pass** |
| `cargo xtask arch-test` (docs/06 §8 gate 1 — domain/application purity) | ✅ 9 crates, dependency rules hold |
| Adversarial suite (docs/06 §8 gate 2) | ✅ golden 6 + policy/approval adversarial tests |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ clean |
| `cargo fmt --check` | ✅ clean |
| `cargo xtask codegen --check` | ✅ generated TS up to date |
| `cargo deny check` (docs/06 §8 gate 6) | ⚠️ not run locally (tool absent) — **CI Security stage runs it**; licenses hand-verified this milestone: no MPL-2.0 (`scraper`→`tl`) and no OpenSSL-licensed TLS crate (`reqwest` pinned to reuse `rustls`+`ring`, no `aws-lc-rs`). |
| Perf / RSS (docs/01 §4.1) | ⚠️ no harness (see D2) — **measured `jarvisd` idle release RSS ≈ 11 MB** (no web/MCP configured), well within the 40–80 MB idle / ≤120 MB peak budget. |

**perf-warden verdict:** PASS by construction — M2 adds **no default-on resident component or background task**. `rmcp` (MCP host) and `reqwest` (web clients) are instantiated only when their integration is configured (`[integrations.web_search]` / MCP server list), both default-off; `tl` is synchronous with no resident state; `location`/`synthesis` are pure. Single `reqwest` version, single TLS stack (rustls+ring), no duplicate/OpenSSL crates. ~3 MB binary growth.

**security-auditor verdict (whole-milestone diff since `m1-complete`):** PASS, **no BLOCKING**. docs/06 §8 gates hold:
- **Gate 1** — `jarvis-domain`/`jarvis-application` `Cargo.toml` unchanged since `m1-complete`; the new `location`/`synthesis` are pure; `reqwest`/`rmcp`/`tl` confined to adapters.
- **Gate 2** — the **only** production `executor.execute` site is `orchestrator.rs` after `policy::evaluate` (+ grant validate + presence belt for R2). Model text / tool results / fetched pages reach an executor only as sanitised data (`sanitize_result_content` single choke point: strips C0/C1/DEL + bidi/zero-width, caps length; web + MCP re-sanitise at their boundaries). Invariant #1 preserved everywhere.
- **Gate 3** — `message.send` (R2) has exact effect + presence + 10 s timeout, wrapped uniformly at the single registration site (native, MCP-imported, web tools alike); a policy-less descriptor is refused; no R3/R4 tool ships.
- **Gate 5** — CF-12 redacted `Debug` on all arg/effect-bearing types incl. wire DTOs; `StepError` reduced to stable codes; web/MCP error strings sanitised; secrets are keyring refs, the Brave key sent as a header only; grant audit carries the args **hash**, never values.

---

## 3. Deviations (require accept/reject)

- **D1 — Some exit evidence (items 4–7) is demonstrated at the primitive/tool level via fixture + golden tests, not a live real-model end-to-end run.** This is a deliberate M2 scope boundary, consistent with the milestone's fixture-driven acceptance (CLAUDE.md: "fixture-driven tests over live-provider calls, always") and **ADR-004** (the Claude CLI is a transitional adapter that proposes **no** tools for the reasoning profile). Consequences: the *machinery* is proven end-to-end through the real orchestrator with a tool-proposing **fake** model (golden 6 executes `web.fetch`); the *primitives* for location/ambiguity/contested framing are unit-proven; but a real model answering "who is the president" via `web.search`, or the orchestrator auto-resolving location and emitting a clarifying question, awaits (a) a tool-proposing model adapter and (b) a router for the time-sensitive/location/ambiguity routing signals. Both are later-milestone work, not M2 regressions. **The security-relevant property of each item (injection containment, egress gating, sensitivity labeling, no-picker shape, attribution) is proven now.** → *Recommend ACCEPT: the exit-evidence properties are demonstrated; live real-model wiring is out of M2 scope by ADR-004.*
- **D2 — No `cargo xtask perf --rss` harness exists** (xtask has only `arch-test`/`codegen`/`golden`); the docs/01 §4.1 / docs/09 §5 numeric RSS gate cannot be asserted automatically. Mitigation: M2 adds no default-on resident component (perf-warden: budget held by construction) and a manual release measurement gives `jarvisd` idle ≈ 11 MB. → *Recommend ACCEPT for M2 with a tracked action to build the RSS harness before **M4** (fastembed) / **M5** (voice), the first milestones that add resident components.*
- **D3 — `cargo deny` not run in this local gate** (binary absent). CI's Security stage runs it on every PR; licenses were hand-verified this milestone. → *Recommend ACCEPT (covered by CI).*

---

## 4. Open risks (tracked, none blocking — from the whole-milestone audit)

All are recorded in `docs/milestones/M2-security-carryforward.md`; each is dormant behind an unshipped capability, a fail-safe (non-graceful) path, or resolved-by-design.

1. **CF-2 (atomicity half, SHOULD-FIX, dormant).** `AuditSink::record` has no transaction thread, so orchestrator-emitted observability events (`tool.executed/failed`, `policy.*`) are durable + hash-chained in their own tx but **not atomic** with the run checkpoint/outbox — a crash between side effect and sink commit could leave an unaudited effect. The security-critical *grant* lifecycle is already atomic. Closing needs a domain/application port signature change (human-decision territory).
2. **R0 `web.fetch` egress channel (accepted, labeled).** `web.fetch` auto-authorises (R0) and can encode data in a URL to any *public* host; the SSRF guard blocks private/loopback/metadata + redirect hops but not attacker-controlled public URLs — inherent to having a fetch tool. It is `DataEgress::External`, config-gated (CF-5), DNS-rebinding documented as a follow-up.
3. **CF-8 (WS fan-out, dormant single-user).** The WS hub broadcasts every domain event (incl. approval cards) to all authenticated connections with no per-user filter — inert on loopback single-user; **must** be scoped before **M7** multi-user.
4. **CF-10 (`fs.read` TOCTOU/hardlink, dormant).** Containment holes needing a concurrent local writer; harden at the first fs-**write** tool.
5. **CF-6 (`GrantValidator::validate` error arm).** DB faults still `.expect`-panic (mint's arm was fixed) — fail-safe (a panic authorises nothing) but not graceful.
6. **CF-14 (mutating-tool atomicity, dormant).** No real mutating tool ships; a real one needs compensation + idempotency key.
- **CF-15 is RESOLVED** (fail-closed by design: a requeued/recovered run gets `tools:None`). **CF-5 is RESOLVED** (config-gated registration).

---

## 5. Recommendation

All 9 exit-evidence items are demonstrated (items 4–7 with Deviation D1 noted); all automated gates on `main` are green (433/433 tests, golden 1–6, arch-test, clippy, fmt, codegen); both mandated reviews (perf-warden, security-auditor) return **PASS with no BLOCKING**; the six open risks are tracked and non-blocking.

**Requested decision:** accept deviations D1–D3 and sign off M2. **On approval:** tag `m2-complete` and check the M2 box in `docs/08-roadmap.md`. If any deviation is rejected, the corresponding item returns to the feature list (a gate is never passed with silent exceptions).
