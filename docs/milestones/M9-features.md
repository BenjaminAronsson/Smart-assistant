# M9 "Load-bearing" — feature list

Status: **PROPOSED — awaiting owner sign-off.** Decomposed 2026-08-14 on Opus 5 from a
full-tree architecture review, while M8 is still in flight. Milestone feature lists are a
human-only decision (docs/11 §3), so nothing here is committed to until it is approved.
**M8 finishes first** — see decision 1. Check items off as their PRs merge, and do not pull
a later feature forward without an approved change to this list (docs/11 §2).

**Proposed for resolution at approval:**
1. **This is M9; product hardening becomes M10**, keeping its scope and golden 10 intact.
2. **A refactoring milestone is gated on evidence, not aesthetics** — behaviour identical
   and *provably* so, plus a measured structural change. The exit-evidence section below is
   the whole argument for whether this milestone is worth running.
3. **ADR-034** records the structural norm, because the absence of one is the root cause
   of everything in this list. Drafted in F9.13, accepted or rejected at the gate.
4. **Nothing in this milestone changes behaviour.** Any bug found mid-refactor is fixed in
   its own PR with its own test — never folded into a structural diff.

## Why this milestone exists, and what it displaces

Eight milestones of vertical delivery have produced ~97k lines of Rust across 9 crates and
~13.4k hand-written lines of Angular. **The discipline held everywhere it was written
down.** The dependency rule is enforced and unviolated; `thiserror`-per-crate and
"`anyhow` only in binaries" are actually followed (zero `anyhow` in the five library
crates); there are 1,363 tests, 37 golden scenarios, **two** real `TODO`s and **zero**
`FIXME`/`HACK` in the entire tree; every non-obvious dependency choice carries a written
rationale. This milestone is not a quality complaint.

What was never written down is **internal structure**. `docs/02 §3` fixes the *crate*
boundaries and `cargo xtask arch-test` enforces them — but it checks nothing *inside* a
crate, and no rule anywhere governs module layout, file size, or where test doubles live.
Eight milestones of unenforced structure produced exactly what that predicts:

- **`jarvis-adapters/src/spotify.rs` is 3,789 lines** and `home_assistant.rs` is 3,469.
  Each holds a transport, an OAuth/metadata cache, private wire DTOs, argument parsers,
  4–6 tool implementations, and ~1,540 lines of inline tests — in one file.
- **`jarvisd/src/ws.rs` is 2,348 lines**, implements five separate sink traits, owns the
  voice-stream state machine and replay/resync, and contains a **381-line `handle_socket`**.
- **`jarvisd/src/main.rs::run` is 708 lines** — every adapter, repo, tool descriptor,
  route and background task wired in one function body. `api.rs::router_with` is 290.
- **`jarvisd/src/` and `jarvis-application/src/` contain zero subdirectories** between
  them — 27 and 21 modules, all flat files. The *test* trees already discovered the
  shared-module pattern (`voice_fixture/`, `identity_fixture/`, `golden11_support/`) that
  `src/` never did.
- `rfc3339` is reimplemented **9×** in `jarvisd/src/` alone, in **two variants that
  disagree on failure behaviour** — one `.expect()`s, one silently returns the epoch. That
  is a latent inconsistency, not cosmetic duplication. Alongside it: `repository_problem`
  ×6, `not_found` ×5, `truncate_to_micros` ×3, and the 7-line `descriptor()` quartet **×25**.
- `FakeBlobs`/`FakeArtifacts` are independently reimplemented **7–8 times**, `FakeAuditLog`
  ×4; there are **6 separate `async fn harness()`** definitions in `jarvisd/tests/`. A
  shared kit (`jarvis-application/src/testing.rs`) exists, works, and is used cross-crate —
  the blob/artifact/audit doubles simply never got promoted into it.
- CI **cold-compiles the 547-crate workspace five times per run** — there is no Cargo cache
  of any kind — and builds `sqlx-cli` and `cargo-deny` from source twice. **41 of 46**
  external dependencies are declared per-crate with literal versions; **`tokio` appears in
  11 manifests**, each with a hand-tuned feature set.
- Web: **two hand-rolled WebSocket clients** with duplicated reconnect, sequence-gap and
  resync logic, and `setHudPresenceForRunState` duplicated **byte-identically** in both.
  Adding a `RunStateDto` variant needs two edits and TypeScript will not catch the miss.
  Plus `authHeaders()` ×3, two parallel HTTP layers, and **zero SCSS sharing** across 17
  card stylesheets.
- **18 stale M3b-era worktrees are committed as gitlinks** (mode `160000`) with no
  `.gitmodules`, occupying **135 GB**. A fresh clone gets 18 empty directories, and every
  repo-wide `rg` returns ~5× duplicate hits.

None of this is breaking the product. All of it is a tax on every future milestone, and
the tax compounds: M8's own features are landing in `ws.rs`, `runs.rs` and `main.rs` —
the three worst files in the tree.

**It displaces nothing.** M10 keeps the whole of the old M9 hardening row.

---

## The decisions, as proposed (docs/11 §3 — human only)

**1. ⬜ M8 finishes first; this is M9 and hardening becomes M10.** F9.x would race
F8.5–F8.11 through `ws.rs`, `runs.rs` and `main.rs` — precisely the files room attribution
and the settings surface must touch. Two large diffs converging on a 2,348-line file is how
a refactor loses its "behaviour identical" claim. Hardening also genuinely benefits from
going second: an installer and a diagnostics bundle are easier to build over a composition
root that is not one 708-line function.

`docs/milestones/M8-features.md` records "hardening becomes M9" as an approved decision, and
`docs/08 §1`'s old M9 row is now M10. **That signed-off M8 record is deliberately left
unedited** — it was true when written, and rewriting an approved decision for a milestone
that is not itself approved yet would be the wrong order. If this list is approved, the
cross-reference is reconciled at the next `/sync-docs`.

**2. ⬜ Evidence, not aesthetics.** A refactoring milestone whose gate is "it reads better"
is unfalsifiable and should not be run. This one asserts *behaviour identical, provably*:
no weakened assertion, `codegen --check` green **without regenerating**, and an **empty
`.sqlx/` and `migrations/` diff**. Those three turn "we didn't change anything" from a
claim into a check. `jarvis-infra` is out of scope specifically to keep the last one usable.

**3. ⬜ ADR-034, and an automated gate.** Cleanup without a written norm decays back. There
is no `clippy.toml`, no `rustfmt.toml`, and exactly one lint in the workspace table
(`unsafe_code = "deny"`). The norm is **ratcheted, not aspirational**: every threshold is
set to what the tree actually achieves after F9.12, so the gate is green on landing and can
only tighten later.

**4. ⬜ No behaviour changes, and bugs get their own PR.** The `rfc3339` divergence and the
duplicated `setHudPresenceForRunState` are real defects this review surfaced. They are
fixed *inside* the features that touch them, with their own tests — but anything else found
along the way leaves in a separate PR, or the structural diff stops being reviewable and
the milestone's central claim stops being checkable.

---

## Exit evidence (proposed docs/08 §1 M9 row)

**Behaviour identical, provably:** the workspace test count is at or above the M8 baseline
with **no assertion weakened or deleted**; every golden scenario passes unchanged;
`cargo xtask codegen --check` passes **without regenerating** (no wire contract moved); the
diff touches **no file under `migrations/` and leaves `.sqlx/` empty** (no SQL changed);
idle RSS is unchanged against the M8 figure.

**Structure measurably changed:** no `.rs` file over **1,000 lines** and no function over
**150 lines**, both now enforced by `cargo xtask arch-test`; test doubles come from one
crate rather than 7–8 copies; CI wall-clock materially reduced, with the before/after
figure recorded in the gate report.

---

## Ordering

Deliberate, and the ordering is most of the engineering judgment in this list: hygiene and
CI first because every later feature pays their cost; dependency inheritance while diffs
are still small; the shared test-support crate **before** the splits, because the god-files
are far easier to split once their inline doubles live somewhere else; the web work after
the Rust work so the two never collide in review; enforcement **last**, because its
thresholds are calibrated to what the cleaned tree achieves.

## M9 — features (F9.1–F9.13)

- [ ] **F9.1 — Untrack the worktrees, reclaim the tree** · *Sonnet*
      Removes the 18 `160000` gitlinks under `.claude/worktrees/` from the tracked tree,
      adds the path to `.gitignore` (3 lines today: `/target`, `node_modules/`,
      `web/dist/`), and deletes the 0-byte `.gitmodules`. This is first because it is not
      cosmetic: **every repo-wide `rg` currently returns ~5× duplicate hits** from stale
      M3b-era copies of `crates/`, `web/`, `docs/` and `CLAUDE.md`, which would obstruct
      every remaining feature in this list — and a fresh clone gets 18 empty directories.
      Two worktrees are dirty (`agent-abcec2b9455a97cd3`, `agent-af605475d4f15d67d`) and
      must be inspected, not discarded.
      Tests: a fresh `git clone` into a temp dir contains no `.claude/worktrees`; `rg` for
      a known-unique string returns exactly one hit.
      Refs: `.gitignore`, `git ls-tree -r HEAD`. Deps: none.

- [ ] **F9.2 — CI: cache the toolchain and the workspace** · *Sonnet*
      `.github/workflows/ci.yml` runs 5 jobs that each cold-compile 547 crates, and
      `cargo install`s `sqlx-cli` and `cargo-deny` from source — twice. Adds
      `Swatinem/rust-cache` to every Rust job, installs both tools as prebuilt binaries,
      and drops `build`'s `--workspace --release` from `integration`'s `needs` (which does
      its own `cargo build -p jarvisd` regardless). Second because every later feature runs
      this loop, and because a refactor milestone is CI-bound by construction.
      Tests: full CI green; before/after wall-clock recorded for the gate report.
      Refs: `.github/workflows/ci.yml`, docs/09 §5. Deps: F9.1.

- [ ] **F9.3 — Workspace dependency inheritance** · *Sonnet*
      `[workspace.dependencies]` holds 10 entries (5 internal, 5 external) while 41 of 46
      external crates are declared per-crate with a literal version — `tokio` in **11**
      manifests, `tokio-util` in 5, `tracing`/`sha2`/`async-trait` in 4 each. A version bump
      is an 11-file edit today. Promotes them to `workspace = true`, keeping per-crate
      `features = [...]`. **Preserves every existing rationale comment**: the `rand_core6`
      alias (ed25519-dalek pins 0.6), the deliberate `futures-core`-not-`futures-util` split
      in `jarvis-application`, and the opposite-role `rmcp` declarations are all documented
      choices, not accidents.
      Tests: `cargo tree -d` shows no new duplicate majors; `Cargo.lock` unchanged;
      `arch-test` and `cargo deny check` pass.
      Refs: root `Cargo.toml`, the 9 member manifests. Deps: F9.2.

- [ ] **F9.4 — `jarvis-test-support` crate** · *strong model*
      A new workspace crate is a deliberate act — `arch-test` fails any crate with no rule
      — so this needs its own rule entry, reachable by **dev-dependency edges only**, and it
      must not become a back door around `jarvis-domain`'s purity allowlist. Promotes the
      doubles written 7–8× over (`FakeBlobs`, `FakeArtifacts`, `FakeAuditLog` ×4,
      `FakeArtifactStore` ×3, `FakeSessionStore`, `RecordingSink` ×3, `RecordingCanvas` ×3)
      and the duplicated harness helpers (`harness()` ×6, `send()` ×5, `temp_root()` ×5,
      `spawn_worker` ×2). Folds in `tests/golden11_support/mod.rs`, already a partial
      attempt at the same thing. **Leaves `jarvis-application/src/testing.rs` in place** and
      re-exports through it — it is feature-gated, used cross-crate, and it works.
      Before the splits, because a 3,789-line file with 1,539 lines of inline doubles is
      much harder to divide than the same file without them.
      Tests: every migrated call site compiles with no signature change; `arch-test` rejects
      a `jarvis-test-support` edge from a non-dev dependency; workspace test count unchanged.
      Refs: `crates/xtask/src/main.rs` `RULES`, docs/02 §3. Deps: F9.3.

- [ ] **F9.5 — Split `spotify` and `home_assistant` into module directories** · *Sonnet*
      The two largest files in the repo, 7,258 lines combined, each a god-module: transport
      + auth/metadata cache + private wire DTOs + arg parsing + 4–6 tools + tests. Each
      becomes `<name>/{client,wire,tools/*}.rs`, one file per tool. Moves the ~3,080 lines
      of inline tests into a new `crates/jarvis-adapters/tests/` — the crate has no `tests/`
      directory at all today — using F9.4's doubles. A move, not a rewrite: **no signature
      changes**, so the diff stays reviewable at that size.
      Tests: adapter test count unchanged; golden 9 and the M5 acceptance suite pass
      untouched; the HA allowlist and area-resolution tests keep every assertion.
      Refs: docs/02 §3, `.claude/skills/media-integration`. Deps: F9.4.

- [ ] **F9.6 — One tool-declaration seam** · *strong model* · **security-auditor required**
      The `new()`/`id()`/`policy()`/`descriptor()` quartet repeats at **25** `impl
      ToolExecutor` sites, with `descriptor()` byte-identical 6× inside `spotify.rs` alone.
      Introduces a `declare_tool!` macro (or `ToolDefinition` blanket trait) for the
      mechanical part. **The constraint governs the design:** every tool's `risk`, `egress`
      and `required_scopes` must stay individually written and greppable at its declaration
      site. A macro that lets a tool *inherit* a risk tier by default is a policy
      regression, not a cleanup — invariant 1 depends on that classification being explicit
      and auditable, and "less code" is the wrong objective on this particular surface.
      Tests: a registry snapshot asserting every registered tool's `policy()` is identical
      before and after; the adversarial suite passes; security-auditor returns no BLOCKING.
      Refs: `.claude/skills/policy-grants`, invariant 1. Deps: F9.5.

- [ ] **F9.7 — Split `jarvisd::ws`** · *strong model* · **security-auditor required**
      2,348 lines carrying five sink traits (`CanvasSink`, `OutboxPublisher`,
      `DisplayDirectiveSink`, `MediaWindowSink`, `RunEventSink`), the voice-stream state
      machine (`ActiveVoiceStream`, `speak_task`, `ActiveSpeech`, barge-in), replay/resync,
      the axum upgrade handler, and a **381-line `handle_socket`** select-loop. Splits into
      `ws/{hub,sinks,voice,replay,socket}.rs` and decomposes `handle_socket` into named
      branch handlers. This module owns **per-connection event scoping** — the thing that
      closed CF-8 at the M7 gate — so the audit's job is to confirm scoping is untouched.
      Tests: `ws_stream.rs` (1,096 lines) and `voice_round_trip.rs` pass untouched; golden
      11 passes; the `delivery_scope_tests` module keeps every assertion.
      Refs: `.claude/skills/ws-contracts`, M7 gate report §1, docs/05 §1. Deps: F9.6.

- [ ] **F9.8 — Extract the composition root; unmix `jarvisd::runs`** · *strong model*
      `main.rs::run` is **708 lines** — the whole DI graph in one body — and
      `api.rs::router_with` is 290. Splits the composition root into per-area builders and
      the route table into per-area routers. Separately, `runs.rs` (1,524 lines) mixes HTTP
      handlers, the `RunEngine`/`ToolPlane` driver, DTO mapping, **and production trait
      implementations that belong elsewhere**: `EchoModel: ModelProvider`,
      `PassthroughAssembler`, `MemoryAssembler` (~180 lines of retrieval logic), and
      `SystemClock: Clock`. Relocates each to its owning module.
      Tests: `runs_api.rs`, `m5_acceptance.rs` and the golden suite pass untouched; a
      startup test asserts the **same route set and the same registered tool ids** as
      before — the one assertion that makes a composition-root split safe.
      Refs: docs/02 §3, `.claude/skills/state-machine`. Deps: F9.7.

- [ ] **F9.9 — Kill the `jarvisd` helper duplication** · *Sonnet*
      `rfc3339` ×9 **in two variants that disagree on failure** (five `.expect("UTC
      timestamp formats")`, two `unwrap_or_else(|_| "1970-01-01T00:00:00Z")`), plus
      `truncate_to_micros` ×3, `repository_problem` ×6, `not_found` ×5, and per-module
      `*_problem` mappers in 8 more files. All 14 already funnel into the single
      `problem.rs::problem` — the primitive exists; the **mapping tables** are what is
      duplicated. Replaces them with one time helper and an `impl From<RepositoryError> for
      Response` (or a `ProblemFrom` trait with an overridable title), deleting ~150 lines.
      **Deciding which of the two divergent timestamp behaviours is correct, and writing it
      down, is the point of this feature** — not a detail to settle in review.
      Tests: one test per `RepositoryError` variant asserting status + stable machine code;
      every existing API test's problem-body assertion unchanged.
      Refs: docs/03 (RFC 9457 mapping), `crates/jarvisd/src/problem.rs`. Deps: F9.8.

- [ ] **F9.10 — Split the two remaining grab-bags** · *Sonnet*
      `jarvisd/src/config.rs` (1,311 lines: ~25 config structs, each with a `Default`, and
      ~35 one-line `default_*` serde helpers) becomes `config/` submodules by area.
      `jarvis-application/src/ports.rs` (833 lines: 5 error enums plus every port trait)
      splits by port area. Also lifts `deepdive.rs`'s six near-identical problem helpers
      onto F9.9's seam.
      Tests: config round-trip and `validate` tests unchanged; `arch-test` passes and the
      `ports` **re-export surface is byte-identical** — it is the crate's public seam and
      three crates depend on its shape.
      Refs: docs/02 §3, docs/09 §2. Deps: F9.9.

- [ ] **F9.11 — Web: one socket, one HTTP layer** · *strong model*
      `app.ts` and `conversation.ts` each hand-roll a WebSocket client with their own
      reconnect, sequence-gap detection and resync, over the one shared piece
      (`ApiService.openSocket()`). Worse, **`setHudPresenceForRunState` is duplicated
      byte-identically** (`app.ts:321-347`, `conversation.ts:462-488`): the next
      `RunStateDto` variant needs two edits and the compiler will not flag the miss. That is
      a live defect, not a smell. Extracts one `RunStreamService`; collapses `authHeaders()`
      ×3 and routes `artifact-api.service.ts` / `app-bridge.service.ts` through `ApiService`
      instead of injecting `HttpClient` directly (two parallel HTTP layers today).
      Tests: a spec asserting an **exhaustive** `RunStateDto` → presence mapping, so a new
      variant fails to compile; gap-detection and resync specs against the single client.
      Refs: `.claude/skills/angular-shell`, docs/05 §2, `web/src/generated/api-types.ts`.
      Deps: F9.10.

- [ ] **F9.12 — Web: a shared card layer** · *Sonnet*
      There is no `shared/`, `ui/` or `core/` directory, and **zero `@use`/`@import`**
      across 853 lines of card SCSS in 17 standalone files, coordinating only through the 33
      custom properties in `styles.scss` (`display:flex` ×31, `color: var(--ink)` ×22,
      `background: var(--glass-bg)` ×8 …). Adds a `_card.scss` partial for the repeated
      glass/flex/ink shapes, and replaces `hud-card.html`'s flat `@if` chain — 18 narrowing
      `computed`s in a 92-line component, three edits per new card type — with a card-type
      registry. The Angular tree is otherwise in good shape (signals throughout, `OnPush`
      everywhere, only two components over 300 lines), so this is the whole of the web debt.
      Tests: all 45 specs / 256 `it()` blocks pass; HUD acceptance screenshots unchanged.
      Refs: `.claude/skills/angular-shell`, docs/12. Deps: F9.11.

- [ ] **F9.13 — ADR-034 and the structural gate** · *strong model*
      Drafts ADR-034 (Proposed; owner accepts at the gate) fixing the norm: a directory
      module above a size threshold, tests out of `src/` for adapter crates, test doubles
      only from `jarvis-test-support`, and the file/function ceilings. Extends `xtask
      arch-test` — which today checks only crate-level edges and **explicitly nothing
      intra-crate** — with those checks. Adds `clippy.toml`, `rustfmt.toml` and a
      `[workspace.lints.clippy]` table (the workspace lint table has exactly one entry
      today). **Ratchet, don't aspire:** every threshold is set to what the tree achieves
      after F9.12, so the gate is green on landing and can only tighten.
      Tests: `arch-test` fails a deliberately oversized fixture file and a fixture crate
      with no rule; full CI green under the new lint table.
      Refs: docs/02 §3, `crates/xtask/src/main.rs:518-701`, NFR-08. Deps: F9.12.

---

## Explicitly out of scope

- **`jarvis-infra`** — already the best-organised crate in the workspace (17 modules,
  largest file 629 lines). Touching it means regenerating `.sqlx` against a live Postgres,
  the hardest gate in CI, for no structural gain. Keeping it out is precisely what makes
  "empty `.sqlx/` diff" a usable piece of exit evidence.
- **The golden 1–6 test locations.** `xtask golden` runs them as `--lib` filters inside
  `jarvis-application/src/*_tests.rs`. Moving them to `tests/` breaks the golden runner.
  F9.5's "tests out of `src/`" applies to `jarvis-adapters` only, deliberately.
- **Error-type consolidation.** 56 typed error enums is a design property, not debt. (The
  four stringly-typed `struct X(pub String)` errors and the two bare `unwrap()`s at
  `jarvis-domain/src/grants.rs:41-42` are noted for whoever next edits those files.)
- **`jarvis-agent`'s hand-rolled 317-line HTTP client** — a documented tradeoff: `reqwest`
  hides the peer certificate that pinning needs, and weight matters on a satellite.
- **Behaviour, of any kind.** Every open deviation and carry-forward belongs to M8 or M10.
- **Doc drift found in passing** — `CF-8` recorded dormant in the M2 registry but closed at
  the M7 gate; `UnwiredInM1` described as current in `CLAUDE.md:56` but gone from the
  source tree. That is `/sync-docs`, not a feature.

## Carried in from earlier gates

| Item | Source | Lands in |
|---|---|---|
| *(none — this milestone deliberately carries no behavioural debt)* | — | — |

## Carried out of this review, for M10

| Item | Found | Lands in |
|---|---|---|
| **CF-8 registry drift** — M2 doc says dormant, M7 gate says closed | this review | `/sync-docs` |
| **`UnwiredInM1`** documented as current, absent from source | this review | `/sync-docs` |
| 4 stringly-typed `struct X(pub String)` errors | this review | opportunistic |
| 2 bare `unwrap()` at `jarvis-domain/src/grants.rs:41-42` | this review | opportunistic |
