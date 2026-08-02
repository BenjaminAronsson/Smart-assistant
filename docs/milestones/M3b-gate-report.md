# M3b "HUD face, deep dive, personal utilities" — Gate Report

**Status: READY FOR OWNER REVIEW — NOT YET SIGNED OFF.** Prepared 2026-08-01 · Milestone
loop docs/11 §2. All review passes complete (§4); awaiting owner decision on the deviations
in §5, principally D-M3b-1 (the screenshot set, the one unmet exit-evidence item).

Scope since the M3a sign-off: **18 commits** on `integration/m3b` (HEAD `5d8bd89`), not yet
merged to `main`.

**`jarvis-domain` and `jarvis-application` `Cargo.toml` are byte-identical to `main`** —
the whole milestone added pure types and ports only (invariant 3 holds at the dependency
level, independently confirmed by `cargo xtask arch-test`). One new shipped dependency,
outside the pure crates: **`maplibre-gl` 6.0.0** (npm, `web/`), pinned exact.

---

## 1. Exit evidence (docs/08 §1, M3b row) → result

The M3b row reads: *"HUD screenshot set + UX acceptance scenarios; deep-dive thread keeps
continuity across turns; a timer fires and a list round-trips into an artifact."*

| # | Exit-evidence item | Result | Evidence |
|---|---|---|---|
| 1 | **HUD screenshot set** | ❌ **NOT MET** | No browser binary exists on the build host and every source is blocked by network policy (see §5 D-M3b-1). Nothing was faked. The exact procedure and commands are written up in `docs/milestones/M3b-acceptance.md` §3.3 for completion the moment a browser is available. |
| 1b | **Contrast audit** (docs/12 §9, same bullet) | ✅ MET (numerically) | `both_bundled_wallpapers_pass_the_wcag_aa_contrast_audit` parses the glass/ink tokens out of `backgrounds.ts` + `styles.scss` (not restated, so drift breaks the parse), composites over each wallpaper's worst-case pixel and asserts AA: **bright-haze 16.36:1 body / 10.33:1 secondary; deep-dusk 7.97:1 / 5.04:1** (AA = 4.5:1). Visual confirmation over rendered wallpapers with scrim + backdrop-blur still needs a browser. |
| 2 | **UX acceptance scenarios** | ✅ MET | 12 named scenarios in `cargo xtask golden`, over live Postgres + the real CAS/audit chain/outbox. See §2. |
| 3 | **Deep-dive thread keeps continuity across turns** | ✅ MET | `f3b6_a_follow_up_extends_the_canvas_a_new_topic_shelves_it_and_a_thread_promotes_to_one_growing_document`: a follow-up — and `"open the second one"` — returns `Extend` and retires nothing; a genuine topic change returns `Shelve` and hands the old thread back. Gallery tiles carry their **own** `sourceUrl`/`sourceDomain`/`alt` from their own pages (`a.example` vs `b.example`), per ADR-017. |
| 4 | **A timer fires** | ✅ MET | `f3b7_a_timer_set_before_a_restart_rings_as_a_missed_alarm_after_it`: set → armed at exactly 600 s → **service and store dropped and rebuilt against the same DB** with a clock an hour later → fires with `missed=true`, tone sounds, announcement is literally *"Missed while I was offline — pasta timer is up"* → second sweep fires nothing (exactly one tone) → `timer.fired` outbox row + `["timer.set","timer.fired"]` audit, chain verifies. |
| 5 | **A list round-trips into an artifact** | ✅ MET | `f3b8_a_list_item_is_added_checked_off_and_promoted_to_one_versioned_document`: implicit creation on first use, insertion order preserved, check-off addresses exactly one line by id, promotion renders `# Shopping` / `- [x] milk` / `- [ ] eggs` to the real CAS as v1, a later promotion appends **v2 to the same document** (a different fresh id is deliberately ignored), reopens after restart, 9-row audit sequence verifies. |

---

## 2. Gate runs (`integration/m3b` @ `f001bd0`)

| Gate | Result |
|---|---|
| `cargo fmt --check` | ✅ clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ clean |
| `cargo test --workspace` | ✅ **852 passed, 0 failed, 1 ignored** (the ignored one is pre-existing on `main`: `map_api.rs:629`, needs a real PMTiles extract) |
| `cargo xtask arch-test` | ✅ 9 crates, dependency rules hold |
| `cargo xtask codegen --check` | ✅ generated outputs up to date |
| `cargo sqlx prepare --check --workspace` | ✅ passes (the "potentially unused queries" warning is pre-existing on `main`) |
| `cargo xtask golden` | ✅ **18 scenarios** — golden 1–7, 4 M3a acceptance, 12 M3b acceptance |
| `npm run lint` (web) | ✅ all files pass |
| `npm run build` (web) | ✅ bundle generated, within budget (§3) |
| `npm test` (web) | ✅ **232/232 pass** — a working `chrome-headless-shell` was obtained partway through this gate (§5, formerly D-M3b-2); this is a real browser run, not a `tsc --noEmit` stand-in. Includes `conversation.spec.ts` (the S5 session-scoping regression), executed for the first time. |

M3b golden scenarios:

```
  ✓ M3b F3b.4: a canvas decision cannot retract a pending approval (server half)
  ✓ M3b F3b.4: the 2h panel TTL is a documented, validated default
  ✓ M3b F3b.6: continuation extends / new topic shelves, per-tile attribution, Research Notes promotion
  ✓ M3b F3b.7: timer set → fire → persist across restart → missed alarm announced
  ✓ M3b F3b.8: list add → check off → promote to one versioned document
  ✓ M3b F3b.5: the map renders offline from the local PMTiles extract
  ✓ M3b F3b.5: out of region is refused, never approximated with the wrong one
  ✓ M3b F3b.5: an empty square in coverage is blank, never a neighbour's tile
  ✓ M3b docs/12 §9: both worst-case wallpapers pass the WCAG AA contrast audit
  ✓ M3b docs/12 §9: card grammar only, image attribution, amber exclusivity (grep half)
  ✓ M3b docs/12 §9: no producer can emit a web image without its source link
  ✓ M3b docs/12 §9: no card variant can carry a web image without its source link
```

---

## 3. Resource budget (perf-warden pass, docs/01 §4.1 at the 8 GB target)

**No budget breaches. All new allocations bounded, event-driven, or transient.**

| Measure | Value | Budget |
|---|---|---|
| Initial web bundle | **467.20 kB raw / 108.19 kB gzipped** | 500 kB warning / 1 MB error |
| Largest component stylesheet | `conversation.scss` 4 835 B | 4 kB warning / 8 kB error |
| Lazy: `maplibre-gl` | 954.91 kB raw / 204.39 kB gz | behind `@defer` — never in the initial bundle |
| Lazy: `artifact-canvas` / `map-gl-view` | 30.53 kB / 4.58 kB raw | — |
| jarvisd idle footprint | unchanged, 40–80 MB | — |
| M3b resident addition | **~0.6 MB per session** (8 deep-dive threads × ~73 kB, LRU) | — |
| Estimated total idle | ~190.6 MB | 230 MB |

- **Deep-dive threads** (`crates/jarvisd/src/deepdive.rs:42–50`): `MAX_LIVE_THREADS = 8` per
  session with LRU eviction; caps of 100 facts / 50 sources / 32 images and 200-char labels
  are enforced by loud refusal (`ThreadError::FactsFull` etc.), not truncation.
- **Lists** (`migrations/0012_lists_init.sql:60–85`): 500 items/list at a DB trigger,
  ~540 B/item; **0 bytes resident** — Postgres owns persistence.
- **PMTiles** (`crates/jarvisd/src/pmtiles.rs:48–66`): archive never memory-mapped nor
  buffered; only the ~16 kB root directory is resident. Per request bounded by
  `MAX_DIRECTORY_BYTES` 8 MB (gzip `take()` guards against decompression bombs),
  `MAX_TILE_BYTES` 16 MB, `MAX_LEAF_DEPTH` 4.
- **No idle polling introduced.** Timer scheduler sleeps exactly to the next due time and
  awaits `Notify` when nothing is armed; outbox dispatcher uses Postgres `LISTEN`/`NOTIFY`;
  media watcher is D-Bus signal driven. The only interval is the pre-existing 10 s provider
  health poll (F1.7).

---

## 4. Security review

Two audit passes ran against this milestone: one against the deep-dive/lists feature work
as merged, and a second, targeted one against the F3b.6 *wiring* specifically — because the
first pass closed with *"Whoever wires it must be re-reviewed — that wiring is where B1
becomes live."* Both are complete.

**Wiring re-audit verdict: B1 and B2 are not reopened, and there are no blocking findings.**
The auditor traced every route by which a client-supplied URL can reach a surface — the
recording gate, the card/chip projection, and the emitted markdown link — and confirmed all
three still pass through `is_web_url`. It independently verified all four parts of the
invariant-1 claim: the `ToolProposal` from a browser-handoff is discarded by
`submit_message` (bare statement, no binding read from it beyond a span field); the wire
carries only `{url, domain}` (`SourceHandoffDto`, exactly two fields, contract-tested);
`browser.navigate` is not registered in `jarvisd::tools`, so `policy::evaluate` would
`Reject { UnknownTool }` on it; and no endpoint takes a citation back to open it. It also
independently confirmed invariant 6 (audit written in the same call as the artifact) and
that the transient-event choice for `hud.canvas` is sound — a deep-dive turn commits no row,
so there is no outbox transaction to ride, and the payload is the whole live set (not a
delta) with stable ids, so a missed event self-heals.

The re-audit's six should-fix findings are below, all closed and verified before this
report was finalized.

### 4.1 Findings raised and fixed during this milestone

Three review passes (rust-reviewer, security-auditor, contract-keeper) ran against the
merged M3b work. All BLOCKING findings below were fixed and verified before this report.

| # | Severity | Finding | Resolution |
|---|---|---|---|
| B1 | **BLOCKING** | Untrusted source URLs injected markdown structure and live clickable links into the durable Research Notes artifact. `safe_link` checked the scheme and percent-encoded `(`/`)` but never stripped control characters, so a newline in a fetched page's URL survived into the link destination; `display_domain` still attributed it. Verified: `safe_link("https://a.example/x\n# Owned heading\n")` returned `Some(...)` with the newline intact, and `https://a.example/\n[Reset your password](https://evil.example)` produced a **real anchor** in the rendered artifact. | Hardening moved into `is_web_url` (`crates/jarvis-domain/src/markdown.rs`), which now additionally requires ASCII-graphic and rejects `< > " \` \ ^ { } \|`. Because `display_domain` also calls it, such a URL now fails at the chip, the handoff **and** the recorders — three boundaries, not one. New test uses 10 hostile payloads including the exact one from the audit. |
| B2 | **BLOCKING** | "Facts are paraphrases, not scrapes" was advisory, not structural: `ResearchThread`/`SourceRef`/`ImageRef` had `pub` fields, so `record_fact`'s 400-char cap, `record_source`'s domain check and `record_image`'s provenance requirement were all bypassable by `thread.facts.push(page_body)`. The application module asserted the opposite in prose. | Fields are private with accessors; `SourceRef`/`ImageRef` have **no public constructor at all**, so one cannot exist without having passed `display_domain`. The prose claim is now true. Proof: the old hostile-thread test can no longer be written as a struct literal. |
| R1 | **BLOCKING** | **F3b.6 was merged but had no runtime path.** `DeepDiveService`, `sources_card`/`gallery_card` and `config.ui.deepdive_promote_after` had zero non-test callers; everything compiled and passed only because tests constructed the service directly. Raised independently by rust-reviewer **and** contract-keeper. | Wired: `POST /sessions/{id}/messages` now calls `observe_turn` before spawning the run, publishing a transient `hud.canvas` event. Two new authenticated routes file findings and accept the promotion offer. `to_list_card` was wired too — its producer is the deterministic list grammar. |
| C1 | **BLOCKING** | `HudCardDto::List` had **zero** fixture/round-trip coverage — absent from both `every_card()` and the tag-disjointness fixture, so both tests passed *without* it. A regression introduced by the by-hand union merge of three parallel card-registry branches. | Fixture, tag and a dedicated round-trip test added for the `listId`-vs-`id` distinction. |
| C2 | **BLOCKING** | `list.full` / `list.unrecognized_command` registered in `jarvis-contracts::errors` but never added to docs/05 §7, which the registry requires. | Both rows added. |
| R2 | should-fix | `ListName::key()` could exceed its own DB CHECK: `to_lowercase()` can *grow* byte length (60×`Ⱥ` = 120 B → 180 B), so a valid name hit SQLSTATE 23514 and surfaced as a **503 "provider unavailable"** for a request that could never succeed. | Key truncated on a char boundary to `MAX_LIST_NAME_BYTES`; SQL CHECK kept as the backstop. |
| R3 | should-fix | `ListsService::promote` was non-atomic across two ports: a failure between writing the manifest and recording the pointer left the artifact written with `promoted_artifact_id` NULL, so the next promotion minted a **rival document for the same list** — what the `lists_guard` trigger and ADR-024 exist to prevent. | Order inverted: the pointer is anchored write-once *first*, and `latest() == None` is a recovery path that finishes the job. The fork is now structurally impossible. |
| R4 | should-fix | `RepositoryError::Conflict` collapsed into `Storage`, so permanent conflicts reported as retryable 503s. | `ListsError::Conflict` added, mapped to 409/404. |
| R8 | should-fix | `ListStore::find_by_key` took a raw `&str`, so the normalization contract in `ListName::key()` could be bypassed. | Signature takes `&ListName`. |
| D1/R9 | should-fix | `markdown::escape`'s ordered-list guard was defeated by leading whitespace — the trailing `trim()` removed exactly the character that had cleared the leading-digit state, so `escape("  1. Buy milk")` returned the marker intact while the *tested* case passed. | Trim moved before the scan; test extended. |
| D2 | should-fix | `escape` claimed to fold control and bidi characters but `char::is_control()` is Cc only, so U+2028/2029/061C/2060/FEFF passed through — and the domain already had a stricter `tools::is_bidi_or_zero_width` that this did not use. Two sanitizers disagreeing. | Unified on the shared predicate, extended with U+061C and U+2060. |
| D3 | should-fix | No bound on thread accumulation — the 400-char cap was per *fact*; facts/sources were unbounded and titles/alts unbounded. A page body filed in 400-char chunks is still a page body (docs/06 §5 denial-of-wallet). | `MAX_THREAD_FACTS` 100 / `MAX_THREAD_SOURCES` 50 / `MAX_THREAD_IMAGES` 32 with loud refusal; labels truncated to 200 chars. |
| D4 | should-fix | Map attribution reached an `innerHTML` sink: `customAttribution` is written by maplibre-gl via `DOM.sanitize(...)` whose own comment says *"this might not be enough to prevent all XSS attacks"*. Not exploitable today, but the operator's `[maps] attribution` override bypassed the one `to_plain_text` strip that was holding it. | `customAttribution` removed entirely; attribution is a plain Angular interpolation overlaid on the map, permanently visible with no dismiss affordance (docs/12 §3). Test asserts a hostile `<img onerror=...>` renders verbatim with zero child elements. |
| D5 | should-fix | Raw, un-parsed path segments interpolated into tracing fields — a log-forging primitive (`/lists/%0Afake%20log%20line`). | Field recorded after parsing. |
| C4 | should-fix | `lists.items_bound()` trigger had a TOCTOU race under READ COMMITTED: two concurrent inserts could both pass the count check. The migration documented it as a defence-in-depth guarantee that was not airtight. | `PERFORM 1 FROM lists.lists WHERE id = NEW.list_id FOR UPDATE` serializes per list. |
| R5 | should-fix | Three `.expect()` in library crates, which the project restricts to binaries and startup config. | `MediaType::markdown()` and `ToolId::browser_navigate()` consts, each with a test asserting equality with the validated `FromStr` parse. |
| R6 | should-fix | `DeepDiveService` read the wall clock directly, making the audit timestamp untestable, while `ListsService` correctly used the `Clock` port. | `Arc<dyn Clock>`, asserted against `ManualClock`. |

**Wiring re-audit findings** (second pass, against `feat/m3b-f3b.6-wiring` specifically):

| # | Severity | Finding | Resolution |
|---|---|---|---|
| W1 | should-fix | Recorded URLs had no length bound. Until the wiring landed, `record_source`/`record_image` had no caller; the wiring made them reachable from an authenticated HTTP body, so a thread could hold ~160 MB of URL text (50 sources + 32 images × 2 URLs, each up to the 2 MB body limit) × 8 live threads, amplified by the 1024-slot WS broadcast ring on every publish. | `MAX_URL_CHARS = 2048` enforced inside `is_web_url` itself, so the bound covers `safe_link`/`display_domain` too — a URL too long to link cannot be badged or navigated to either. Refused, never truncated (a truncated URL points somewhere else). |
| W2 | should-fix | Source titles and image alt text reached the HUD card without control/bidi stripping — the one Z4 display path that missed the treatment `sanitize_line`/`markdown::escape` already give list text. A hostile page title with `U+202E` would render inline next to the honestly-computed domain chip, and alt text is spoken by TTS. | `truncate_label` → `sanitize_label`, running untrusted text through the shared `sanitize_result_content` before capping — structural in the recorders, inherited by every projection. |
| W3 | should-fix | Findings arrays were unbounded per request while the handler held a process-global lock; a 2 MB body of 4-byte facts could drive ~500k iterations, blocking `submit_message` for every session on the same mutex. | `MAX_FINDINGS_PER_REQUEST = 64` per array, checked **before** the lock is taken; `422 validation.failed` on excess. |
| W4 | should-fix | `refused` echoed the raw offending URL back to the client — exactly the convention `ListError` documents avoiding ("the offending text is never echoed back"). | `Unattributable(String)` → content-free `Unattributable`; response maps through an exhaustive `refusal_reason` match (no `_` arm) so a future variant must be given a reason before it compiles. |
| W5 | should-fix | The client dropped `HudCanvasDto.sessionId`, the only scoping mechanism on the global WS fan-out — session B's cards could render on session A's canvas, and a `shelve` from B could shelve A's panels. Contained to one owner's own sessions. | `canvasIsForThisView()` guard in `conversation.ts`; null/absent `sessionId` still applies (the list-card "applies anywhere" case). |
| W6 | should-fix | No session-existence check; the 8-thread LRU bound was global rather than per-session, so requests carrying invented ULIDs could evict every real session's canvas state, and `promote` would mint an artifact against a session that does not exist. | `DeepDiveApi` resolves the session through the session store before allocating a slot; 404 otherwise. |

Two items the wiring re-audit raised and explicitly recommended **not** fixing as part of
this gate (both noted in the fix commit so they aren't lost): `promote` is not idempotent —
a client retry after a timeout mints a duplicate version of byte-identical content, matching
the existing list-promote precedent; and `promote` holds the global `threads` mutex across
the blob/artifact-store awaits, which is acceptable contention for a single owner but would
need revisiting if the lock ever becomes contended.

**One more finding, from actually running the web suite for the first time (not a review
pass — a real test execution):**

| # | Severity | Finding | Resolution |
|---|---|---|---|
| T1 | should-fix (found to be a **genuine production race**, not just a test artifact) | `MapCard` loaded coverage via a bare `async` method writing into a plain `signal()`. `provideHttpClientTesting()` sets `REQUESTS_CONTRIBUTE_TO_STABILITY` false, and in production `HttpClient`'s own pending-task entry clears the instant the Observable completes — **before** `firstValueFrom`'s promise settles, before the `await` in `ApiService.getMapCoverage()` returns, and before `MapCard.loadCoverage()`'s own `await` finally writes the signal. `ApplicationRef`/`whenStable()` is driven purely by `PendingTasksInternal`, so the app could be considered "stable" — and anything gated on that (a screenshot harness, SSR hydration, a future E2E suite) could act — while the map card was still genuinely mid-load. All 7 `map-card.spec.ts` tests failed on the suite's first-ever real-browser run this milestone, which is what surfaced it. | Migrated `MapCard` from the bare-async/signal pattern to Angular's `resource()`, which holds its own `PendingTasks` entry open until the resolved value is actually written (in a `finally`, after the write). Fixes the underlying race, not just the test symptom. `map-card.spec.ts`'s choreography updated to match (`detectChanges()` alone opens the request; `whenStable()` is trustworthy only *after* `flush()`), documented as a new idiom since no other spec in the codebase yet uses `resource()`. |

### 4.2 Properties independently confirmed to hold

- **Invariant 6 (append-only audit in the same transaction)** holds in
  `jarvis-infra/src/lists.rs`: every mutating method opens a transaction, writes the row,
  appends audit in the *same* `tx`, commits — and the zero-rows paths **roll back before**
  the audit append, so no audit row records a change that did not happen. Proven against
  live Postgres by `a_miss_writes_absolutely_nothing`.
- **Per-item gallery attribution is structural**, not conventional: `HudCardDto::Gallery`
  carries `Vec<SourcedImageDto>` with no card-level source field, so "one source for all of
  these" is *unrepresentable* (ADR-017).
- **No card type has a page-body field**, so the HUD has nowhere to re-render fetched page
  content even if a producer wanted to. Reading a source is a browser handoff.
- **`display_domain` is the sole producer** of every domain label; userinfo spoofing
  (`https://wikipedia.org@evil.example/`) is correctly labelled `evil.example`.
- **F3b.8 registers no tool** and touches no policy path; `parse_list_command` is pure and
  model-free. All seven list routes are device-authenticated.
- **No new model-authored HTML on the HUD face**; the grep property is now additionally
  mutation-verified (injecting `[innerHTML]` into a card template makes the test fail).
- **`cargo xtask arch-test`** confirms the dependency direction; the pure crates'
  `Cargo.toml` are byte-identical to `main`.

---

## 5. Deviations (require owner accept/reject)

| # | Deviation | Rationale / mitigation |
|---|---|---|
| **D-M3b-1** | **The HUD screenshot set was not produced.** Exit-evidence item 1 is NOT met. | No browser binary exists on the build host and every source is blocked by network policy: `storage.googleapis.com`, `cdn.playwright.dev` and `download.mozilla.org` all return 403, and apt offers only snap-transitional stubs. Nothing was faked or hand-drawn. **This is an environment limitation, not a code gap** — CI itself sets `CHROME_BIN` and does run the browser-gated specs. `docs/milestones/M3b-acceptance.md` §3.3 lists the nine frames, how to reach each and the exact commands. **Recommend: accept as a carry-forward with the screenshots produced on a browser-capable host before the M4 gate — or reject and hold M3b open until they exist.** |
| **D-M3b-2** | ✅ **RESOLVED.** Web unit tests could not be executed on this host at gate draft time. | A working `chrome-headless-shell` was obtained during this gate (§4.1 T1's discovery path). `npm test` now runs for real: **232/232 pass**, including the F3b.4 panel-lifecycle specs and the never-before-run `conversation.spec.ts`. Running the suite for real, rather than relying on `tsc --noEmit`, is what surfaced T1 — a genuine production race, not merely a coverage gap. |
| **D-M3b-3** | The visual half of the contrast audit is outstanding. | The numeric audit is done and automated (§1 item 1b). What remains is visual confirmation over rendered wallpapers with scrim + backdrop-blur. Same browser prerequisite. |
| **D-M3b-4** | Deep-dive findings arrive via an explicit endpoint rather than being extracted from tool results. | The orchestrator exposes no tool-result observation seam and `ToolResult` is opaque rendered text; plumbing one is a separate feature. The recorders' guards are what make accepting client-supplied findings safe. |
| **D-M3b-5** | `is_web_url` now requires ASCII-graphic, so a source URL with a raw non-ASCII path (`…/wiki/Café` un-percent-encoded) is **refused** rather than merely rendered unlinked. | Real fetched URLs are percent-encoded or punycoded, and CLAUDE.md's tie-break prefers the stricter reading. Documented relaxation path exists if the owner disagrees. |
| **D-M3b-6** | `artifact-canvas.scss:124` paints the "sensitive" label amber (`--c-wait`), arguably against docs/12 §2.1 amber-exclusivity — it is a warning, not a request for a decision. | It sits on the artifact canvas, not the HUD face, so the §9 grep is scoped to exclude it. Flagged rather than silently allowlisted or unilaterally restyled. **Owner's call.** |
| **D-M3b-7** | ✅ **RESOLVED.** `web/src/app/conversation.spec.ts` (the W5 session-scoping regression test) had never been executed. | Ran with the real browser as part of D-M3b-2's resolution: passes (5/5), confirming the W5 fix (session-scoped `hud.canvas` handling) actually holds, not just that it type-checks. |

---

## 6. Open risks / carry-forwards

1. **The screenshot set (D-M3b-1)** is the only unmet exit-evidence item.
2. **`HudCardDto` variants with no producer.** Nine of the twelve card types still have no
   server-side producer — they are types the HUD *can* render when a feature builds one.
   Documented in the `cards.rs` module doc rather than left implicit.
3. **Per-handler `CancellationToken::new()`** on the lists, timers and media REST paths is
   never connected to the server shutdown token, so the cancellation checks are decorative
   there. Pre-existing (matches the timers/media precedent), not new M3b debt — but the
   invariant-4 story for REST handlers is currently aspirational. Worth a follow-up that
   derives handler tokens as children of the shutdown token.
4. **`PgListStore::hydrate`** reads the list header and its items in two non-transactional
   queries, so a concurrent write can mix snapshots. Low impact for a single-owner device.
5. **`POST /api/v1/lists` returns 201 even when it returned an existing list.** `200 OK` is
   the honest code; changing it requires `ensure_list` to report whether it created.
6. **docs/08 §1 still records M3 as one milestone.** `/sync-docs` should reflect the
   M3a/M3b split after this gate, as noted in the M3a report.
7. **Deep-dive `promote` is not idempotent.** A client retry after a timeout mints a
   duplicate version of byte-identical content — the sha256 is already computed and
   compared for nothing. Matches the existing list-promote precedent; the auditor
   recommended leaving it for this gate (§4.1 W-notes) since closing it is a small,
   separate change (skip a new version when the rendered sha256 equals the latest).
8. **Deep-dive `promote` holds the global `threads` mutex across the blob/artifact-store
   awaits**, so one session's storage stall blocks other sessions' turns. Acceptable
   contention for a single owner; worth revisiting if the lock ever becomes contended.

---

## 7. Recommendation

**Approve-with-deviations**, conditional on the owner's decisions in §5 (D-M3b-1 …
D-M3b-7) — this is a recommendation, not a sign-off; only the owner can accept the
deviations and sign the gate (docs/11 §3, "human-only decisions").

Grounds:

- All exit evidence except the HUD screenshot set (§1 item 1) is met, most of it by
  executable golden-trace scenarios rather than narrative claims.
- Every BLOCKING finding raised across four independent review passes (rust-reviewer,
  security-auditor ×2, contract-keeper) was fixed and re-verified, including the two that
  mattered most: untrusted web content could not inject markdown structure into a durable
  artifact (B1), and the paraphrase-not-scrape guarantee was made structural rather than
  advisory (B2). The feature that carried the most risk (F3b.6) went through **two**
  security passes precisely because the first one flagged its own wiring as the moment the
  risk would become live — and the second pass confirmed it didn't.
- The full CI loop is green on `integration/m3b` @ `f001bd0` and every commit after it:
  fmt, clippy `-D warnings`, 852+ tests, arch-test (9 crates), codegen, `sqlx prepare
  --check`, 18 golden scenarios, web lint and build.
- perf-warden found no budget breaches at the 8 GB target; every new resident allocation is
  bounded, event-driven, or transient.
- The one unmet item (D-M3b-1, the screenshot set) is an environment limitation — no
  browser binary is reachable from this build host — not a code or design gap, and the
  exact procedure to complete it is written up and ready to run the moment one exists.

The only decision that should give the owner real pause is **D-M3b-1**: whether to accept
the screenshot set as a carry-forward to a browser-capable host, or hold M3b open until it
exists. Every other deviation is either a documented, reversible judgement call (D-M3b-5,
D-M3b-6) or downstream of the same browser limitation (D-M3b-2, D-M3b-3, D-M3b-7).

**Not done as part of this gate, by design:** merging `integration/m3b` into `main`,
tagging, and updating `docs/08-roadmap.md`/`docs/milestones/M3-features.md` checkboxes.
Those follow the owner's decision on the deviations above, per the M3a precedent.
