# M4 "Memory & quota-smart intelligence" — Gate Report

**Status: DRAFT — awaiting owner sign-off.** Prepared 2026-08-06. Milestone loop
docs/11 §2. All planned gate runs are green (§2); a whole-milestone security review found
no BLOCKING findings and all reasonably-scoped SHOULD-FIX items are closed (§4); two
items are carried forward as deviations requiring an owner decision (§5); one unrelated
permission-scope change needs explicit owner confirmation (§6). **Do not tag
`m4-complete` until §5 and §6 are resolved by the owner.**

Scope since the M3b sign-off (`m3b-complete`): **21 commits** on `feat/m4-complete`,
comprising the milestone's original feature work (memory retrieval, deterministic
math/home grammar, CalDAV, SMTP, deferrable-work scheduler + provider health scoring,
ADR-028) plus a same-session hardening pass that closed the DB-validation blocker the
milestone was left with, a missing perf-gate harness, and the findings from this gate's
own security review.

---

## 1. Exit evidence (docs/08 §1, M4 row) → result

The M4 row reads: *"Offline search/retrieval and rule-based home commands work with zero
LLM calls; '15% of 230' answers with zero LLM calls; 'what's on today' renders an agenda
card; the landlord message sends end-to-end through approval → SMTP; deferred
summarization runs in a healthy-quota window; memory forget verified."*

| # | Exit-evidence item | Result | Evidence |
|---|---|---|---|
| 1 | **Offline search/retrieval works with zero LLM calls** | ✅ MET | `MemoryRetrievalService::retrieve` is pure local computation (fastembed + pgvector), never touches a `ModelProvider`. `crates/jarvis-infra/tests/memory.rs` (new, 10 tests) proves the storage layer end to end against live Postgres; `jarvis-application::memory` unit tests cover bounding, cancellation, and the new similarity floor (§4.2). |
| 1b | **Rule-based home commands work with zero LLM calls** | ✅ MET (classification only — see note) | `DeterministicFirstProvider::run` recognizes `turn on/off <target>` and answers without opening the inner provider (`recognized_home_command_does_not_open_the_inner_provider`, plus two adversarial regression tests added this session, §4.2). **Note:** M4 delivers the deterministic *grammar* (FR-28's classification seam); it does not actuate a real device — there is no Home Assistant adapter yet (that lands in M5 per docs/08 §1's M5 row, "HA state/actions with allowlist"). The roadmap phrase is satisfied literally (a recognized command is answered with zero LLM calls); it should not be read as "a light actually turns on." |
| 2 | **"15% of 230" answers with zero LLM calls** | ✅ MET | `local_math_answer_has_no_provider_specific_formatting` asserts the exact string `"15% of 230 = 34.5"`; `recognized_math_does_not_open_the_inner_provider` confirms the inner (LLM) provider is never opened. |
| 3 | **"what's on today" renders an agenda card** | ✅ MET | `classify_calendar_query` recognizes the query deterministically (`today_classifier_is_deterministic_and_conservative`); `CalDavCalendarReader` reads the day window over HTTPS with same-origin enforcement and bounds (jarvis-adapters caldav tests); `WsHub::sink_maps_agenda_to_a_sensitivity_safe_hud_card` proves the WS event is literally typed `"card.agenda"`. The read is now audited and skipped on the no-identity context-assembly path (§4.3 finding #1, fixed). |
| 4 | **The landlord message sends end-to-end through approval → SMTP** | ✅ MET | `SmtpTool` is a host-registered R2 external tool: no grant ⇒ `Denied` before any transport is touched; `approved_grant_sends_once_and_duplicate_is_idempotent` and `mismatched_grant_is_denied_before_fake_transport` cover the approval→grant→execute path plus idempotent replay and argument-fingerprint mismatch denial. Verified clean by the security review (§4.3 "verified clean" list). |
| 5 | **Deferred summarization runs in a healthy-quota window** | ⚠️ **PARTIALLY MET — mechanism only, not end-to-end** | `DeferrableScheduler`/`DeferredWorkExecutor` (health/quota-gated, single-flight, cancellable, capped exponential backoff on failure) are implemented and unit-tested, but **not driven by `jarvisd`**: no background task calls `run_once`, no concrete `DeferredWorkHandler` exists for real work, and no mechanism derives a `QuotaWindow` from provider health signals. See deviation D-M4-1 (§5). |
| 6 | **Memory forget verified** | ✅ MET (closed a real gap this session) | Previously only reachable via the storage layer with zero tests anywhere. Now: `crates/jarvis-infra/tests/memory.rs::forget_removes_the_row_is_idempotent_and_cascades_to_embeddings` (storage layer) **and** `crates/jarvisd/tests/memories_api.rs` (new, 12 tests through the real axum router) — `forgetting_an_existing_memory_is_204_and_it_is_genuinely_gone` confirms the REST path, the store, and the cascaded embedding row; `forgetting_an_unknown_or_already_absent_id_is_404_never_a_silent_success` and `forgetting_another_users_memory_is_404_and_never_deletes_it` cover the security-relevant boundary cases. |

**5 of 6 items fully met; item 5 is a documented partial with a deviation for owner
decision (§5).**

---

## 2. Gate runs

All against live Postgres/pgvector (`docker compose -f infra/compose/dev.yml up -d
postgres`), release-mode where noted.

| Gate | Result |
|---|---|
| `cargo fmt --check` | ✅ clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ clean |
| `cargo test --workspace` | ✅ **932 passed**, 0 failed, 1 ignored (pre-existing `map_api.rs`, needs a real PMTiles extract — same as M3b) |
| `cargo xtask arch-test` | ✅ 9 crates, dependency rules hold (`jarvis-domain`/`jarvis-application` untouched by any new external dependency) |
| `cargo xtask codegen --check` | ✅ generated outputs up to date |
| `cargo xtask golden` | ✅ golden 1–7 + M3a/M3b acceptance scenarios (M4 adds no new numbered golden trace; its exit evidence is demonstrated by the feature-level suites in §1) |
| `cargo xtask perf --rss` (new this session, closes the M2 carryforward "no RSS harness exists — build before M4/M5") | ✅ cold start to healthy **0.051 s** (budget <2 s); idle RSS **20.4 MB** (budget 40–80 MB typical, 120 MB hard ceiling) — both well inside budget, reproduced across two runs |
| `web: npm run lint` | ✅ clean |
| `web: npm run build` | ✅ clean, initial bundle 110 KB transfer |
| `web: npm test` (Chrome headless) | ✅ **235/235** |

No failures anywhere in this pass.

---

## 3. Database-backed validation (the milestone's prior blocker)

`docs/milestones/M4-features.md` previously recorded the milestone as implementation-complete
but untested against a live database (the dev host's rootless-podman networking was broken).
That was fixed in an earlier session (`dev-host-podman-postgres` memory note) and confirmed
working again this session. Running the full suite against real Postgres surfaced and fixed
three real, previously-undetected bugs, none caught by `SQLX_OFFLINE` compilation:

1. **`memory.context_provenance` was missing its `rank` column** — the `record_context`
   `INSERT` bound a `rank` value the table didn't have a column for; every call would have
   failed at runtime with a Postgres `42703` error. Fixed in migration 0014 and the insert.
2. **`PgMemoryStore::list`'s `ILIKE ... ESCAPE` clause was double-escaped** (two literal
   backslashes instead of one) — every text-query memory search failed with
   `invalid escape string`. One-line fix.
3. **`crates/jarvis-infra/src/memory.rs` (the entire `PgMemoryStore`) and
   `crates/jarvisd/src/memories.rs` (the REST surface) had zero database-backed test
   coverage anywhere in the repository.** Both gaps are closed this session (22 new
   integration tests total across the two new test files).

---

## 4. Review passes

### 4.1 perf-warden equivalent

No dedicated `perf-warden` subagent run was needed beyond building and running the new
`cargo xtask perf --rss` harness (§2) — the measured numbers are an order of magnitude
under budget (20 MB vs. an 80 MB typical / 120 MB ceiling), and no new resident component,
dependency, or background task was added by this milestone that isn't already covered by
the existing low-power rules (fastembed is lazy-loaded/idle-unloaded per docs/09 §5; the
Claude CLI remains single-flight; CalDAV/SMTP are transient per-request, not resident).

### 4.2 rust-reviewer equivalent

Covered inline by the fixes in §4.3 plus the two new integration-test files; no additional
findings beyond what the security review surfaced.

### 4.3 security-auditor: whole-milestone diff review (`m3b-complete..HEAD`)

**Verdict: no BLOCKING findings.** 5 SHOULD-FIX items were found; **3 are fixed and
verified this session**, 2 are carried forward as deviations (§5).

**Fixed:**

1. **Home-intent classification ran on the fully assembled prompt, not the raw
   utterance** (`deterministic.rs`) — a home-intent match's greedy `target` capture could
   swallow appended memory context or a replanned untrusted tool result, echoing it back
   as first-person assistant speech claiming an effect nothing executed. Fixed: classify
   only the text before the first `[Untrusted ...]` marker. Two adversarial regression
   tests added.
2. **Memory retrieval had no relevance floor** (`memory.rs`) — every message, including
   ones wholly unrelated to anything the owner had told Jarvis, attached up to 8 stored
   personal facts to the prompt sent to the external provider. Fixed: a minimum cosine
   similarity (`MIN_RETRIEVAL_SIMILARITY = 0.3`, a conservative starting point, not
   empirically tuned — flagged in the doc comment for revisit with the M4 evaluation
   harness). Also incidentally fixes a functional risk: without this, any user with
   stored memories would likely have exceeded the 256-byte input cap on the deterministic
   home/math grammar on nearly every message, silently defeating exit-evidence items 1b/2.
3. **CalDAV titles reached the HUD unbounded and unsanitized** (`calendar.rs`) — unlike
   every other untrusted-content path in this codebase, event titles from a hostile or
   compromised CalDAV server were not run through `sanitize_result_content`, and had no
   length cap (only the whole-request 256 KiB / 256-event bounds applied). Fixed: same
   sanitizer every other path uses, 200-byte cap.
4. **The calendar read had no audit event and did not fail closed on a crash-recovered
   run** (`runs.rs`/`main.rs`) — the only production caller of `CalendarReader::read` was
   outside the tool/policy plane entirely (no `ToolPolicy`, no audit row), and — unlike
   memory retrieval, which is already skipped — it still ran on the no-identity
   context-assembly path used by crash-recovered/degraded-requeued runs, which are
   deliberately spawned with no policy context (CF-15 fail-closed). Fixed: the read is
   now audited (`calendar.read`, best-effort, mirrors `record_context`'s pattern) and is
   skipped entirely when there is no attributable user, matching memory retrieval.

**Carried forward as deviations (§5):**

5. **`FastEmbedProvider`'s first (cold-cache) embedding load is not raced against
   cancellation** and may perform an unbounded network fetch of the ONNX model on first
   use. Not fixed this session — the correct fix (pre-provision/eager-load at startup, or
   race the `spawn_blocking` `JoinHandle` against the cancellation token) touches adapter
   startup sequencing that deserves its own focused pass rather than a rushed change
   inside an already-large gate-hardening commit.
6. **`DeferredWorkExecutor` is unwired** — see exit-evidence item 5 and deviation D-M4-1.

**Verified clean** (see the full audit for the complete list): invariant 1 (every
`ToolExecutor::execute` call site still gated by `policy::evaluate` + grant validation,
no new bypass), invariant 3 (purity — `jarvis-domain`/`jarvis-application` `Cargo.toml`
gained no new external dependency), invariant 6 (audit rows for every memory mutation
committed in the same transaction, `forget` writes no audit row when nothing changed),
memory secret-shaped-content rejection on both write and read (`rebuild` fails closed),
SMTP's grant/argument-fingerprint match and idempotent replay, CalDAV transport
(HTTPS-only, same-origin, no embedded credentials, byte/event/time bounds), the
scheduler's bounded queue and cancellation, no new unauthenticated surface, no SQL
injection surface in the new dynamic-SQL memory queries, and no monetization surface
(ADR-021) anywhere in the new CalDAV/SMTP/memory work.

**Documentation-only findings, also fixed:** two `M4-features.md` wording
imprecisions (agenda "sensitivity-safe" rendering actually means the label isn't leaked
to the client, not that content is suppressed; SMTP idempotency's primary defense is the
single-use grant, not the process-local in-memory store) — corrected in the same commit
as the memories-API tests.

---

## 5. Deviations requested (owner decision required)

**D-M4-1: "Deferred summarization runs in a healthy-quota window" is demonstrated at the
scheduler-mechanism level only, not end-to-end in the running daemon.**

`DeferrableScheduler`/`DeferredWorkExecutor` are implemented, tested, and — per the
security review — have no latent bug (bounded queue, single-flight, cancellable, capped
backoff). What's missing to make this real:

- A background task in `jarvisd` that actually drives `DeferredWorkExecutor::run_once` on
  a loop.
- A concrete `DeferredWorkHandler` for real work (docs/03 §4 names summarization,
  memory-candidate extraction, and non-default session titles as the intended examples —
  none of these have an existing trigger or design in this codebase yet).
- A way to derive a `QuotaWindow` from provider health — today `ProviderHealthTracker`
  only tracks Healthy/Degraded/Unavailable + a reason-code string; there is no parsed
  reset-time or window concept anywhere to build on.

Building the mechanism's *driver loop* is bounded work; choosing *what gets summarized,
when, and why* is a product/design decision this session did not have a spec for, and
CLAUDE.md reserves exactly this kind of milestone-scope call for the owner. **Requested
disposition:** accept as a carryforward into a follow-up feature (M4-adjacent or folded
into M5), the same pattern accepted for M2's D1–D3 and M3a's D-M3a-1…7.

**D-M4-2 (informational, not requiring a decision, just visibility): `MemoryWriteService`/
`EmbeddedMemoryStore` have no production call site.** This is intentional, not an
oversight — `crates/jarvisd/src/memories.rs` explicitly defers memory *creation* to a
future explicit-confirmation/candidate-extraction feature, and this session's atomic
write path was built to satisfy the doc-comment requirement on `MemoryStore::replace`
("re-embedding is a later, deferrable job") once that feature exists. Left as a tested,
intentionally-unwired seam (the same pattern the codebase already uses for
`RunState::UnwiredInM1`), not flagged as a blocker.

---

## 6. Owner confirmation required (unrelated to gate content, found during the review)

`.claude/settings.json` changed within the reviewed commit range (part of the
uninformatively-named commit `2231353 things`, predating this session's work): it moved
`Bash(git push:*)` from `ask` into `allow`, and added `Bash(git -C:*)` to `allow` — the
latter re-admits push (and any other git operation, in any repository) past the narrower
rule below it. This is a permission-scope relaxation, not a code change, and this session
neither made it nor can consent to it on your behalf. **Please confirm this was
deliberate before signing off the gate** — if not, it should be reverted independently of
this milestone's disposition.

---

## 7. Open risks carried from earlier milestones

No new items. `docs/milestones/M2-security-carryforward.md` should be updated (a
`doc-syncer` task, not done in this session) to reflect: **CF-14** ("first mutating
tool") is now live via SMTP and dispositioned by this milestone's grant/idempotency
design (a drop-on-timeout leaves the claim `InProgress`, which fails closed for that
grant; the action is irreversible so there is no compensation; re-approval mints a new
grant and can legitimately send again — accepted as-is). **CF-8** (unfiltered WS
fan-out, dormant single-user risk) now also carries calendar event titles in its payload;
severity unchanged, noted for whenever CF-8 itself is addressed.

---

## 8. Recommendation

All fully-in-scope exit evidence is met, all gate runs are green, and the whole-milestone
security review found no blocking issues (3 of 5 SHOULD-FIX items fixed and verified
in-session; the other 2 are legitimate scope-bounded carryforwards, not defects).
**Recommend APPROVE WITH DEVIATIONS** (D-M4-1, D-M4-2) once the owner has:

1. Decided the disposition of D-M4-1 (accept as carryforward / scope a follow-up now).
2. Confirmed or reverted the `.claude/settings.json` permission change (§6) — independent
   of the milestone, but discovered during it.

On approval: tag `m4-complete`, update the docs/08 §1 roadmap row to ✅, and merge
`feat/m4-complete` to `main`.
