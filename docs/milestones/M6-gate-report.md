# M6 "Generated apps" — gate report

**Status: AWAITING OWNER SIGN-OFF.** Every exit-evidence item is demonstrated and every
check in the loop is green. There is **one BLOCKING finding** below that is not an M6
regression but makes M6's own exit evidence unreachable on a real device, and **two ADRs**
plus **two deviations** that need an owner decision. A gate is never passed with exceptions
silently (docs/11 §2), so those are stated as decisions to take, not as noise.

Run 2026-08-11 on Opus 5. Scope since `m5-complete`: **9 commits**, 89 files,
+12 235 / −91 lines. Feature list: `docs/milestones/M6-features.md` (all 7 checked).

---

## 1. Exit evidence (docs/08 §1, M6 row) → result

> *Dashboard app generated; cannot access undeclared capabilities; golden 8.*

| # | Evidence | Result | Where |
|---|----------|--------|-------|
| 1 | A dashboard app is generated | **PASS** | `cargo xtask golden` → "M6 acceptance #1" (`crates/jarvisd/tests/apps_end_to_end.rs`) |
| 2 | It cannot access undeclared capabilities | **PASS** | golden 8 (`crates/jarvisd/tests/golden8_generated_app.rs`) + 18-case application table (`appbridge_tests.rs`) |
| 3 | Golden 8 | **PASS** | `cargo xtask golden` → "golden 8: generated app requests an undeclared capability; bridge rejects" |

Traceability adds the verification shape for FR-18 (docs/01 §6): **CSP / capability-denial /
escape tests** — all three present, see §3.

**Evidence #1 in detail.** Nothing below the transport is faked: the real
`tools/app-builder` Node worker runs a real Vite build of the locked `dashboard/v1`
template (committed `package-lock.json`, 72 packages); the bundle is stored through the
real content-addressed `FileBlobStore` and `PgArtifactStore` against **live Postgres**;
the artifact **reopens from a fresh store instance** (the restart analogue); the audit
chain still verifies with `artifact.created` in it; and the document is served under the
sandbox CSP while the blob route returns the *same* artifact as `attachment`.

**Evidence #2 in detail.** Golden 8 runs the real HTTP routes behind the real bearer
middleware, the real `AppBridge`, the real `policy::evaluate`, and the real `PgAuditSink`.
Undeclared (at mint **and** at exchange), forged, malformed, replayed, cross-artifact and
cross-version are each rejected, each written as a durable `app.capability_denied` row, and
none reaches a tool. The trace **opens by succeeding** on the declared capability so every
refusal is demonstrably a refusal, and closes by asserting that across the whole matrix
exactly **one** tool call ever happened.

---

## 2. The loop (CLAUDE.md "Build & test loop")

| Check | Result |
|-------|--------|
| `cargo fmt --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace` | **1266 passed, 0 failed** (was 1131 at `m5-complete`) |
| `cargo xtask arch-test` | 9 crates, dependency rules hold |
| `cargo xtask codegen --check` | generated outputs up to date |
| `cargo xtask golden` | **35 scenarios green**, traces 1–9 + M3a/M3b/M5/M6 acceptance |
| `cargo xtask perf --rss` | cold start **0.051 s** (budget < 2 s); idle RSS **21.6 MB** (typical band 40–80 MB, ceiling 120 MB) |
| web `lint` / `test` / `build` | clean / **256 passed** / builds |
| `cargo deny check` | **not run locally** — `cargo-deny` is not installed on this host; CI runs it per `.github/workflows/ci.yml`. No new third-party Rust crate entered the tree this milestone (see §4), so the licence/advisory surface is unchanged. |

Adversarial suite (M2 onward, docs/06 §8): `adversarial_tests` green (golden 6's two
scenarios), plus M6's own escape suite.

**Perf note.** Idle RSS moved 20.9 → 21.6 MB across the milestone (+0.7 MB), from the
bridge's in-memory token map and the new routes. The token map is bounded by *live* tokens
— expired entries are swept on every mint — so it does not grow with uptime. The heavy
component (a Node/Vite build) is out of process, opt-in, and bounded on both sides
(worker-side kill at the spec's `maxBuildSeconds`, host-side round-trip backstop, and a
2 MiB bundle cap enforced by the host). F6.3 additionally *reduced* peak per-request memory
for artifact serving from one blob to one 64 KiB chunk.

---

## 3. The three FR-18 test classes (docs/01 §6, docs/06 §8 gate 4)

**CSP.** The app document is served with `sandbox allow-scripts; default-src 'none';
script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:;
connect-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'self'`, and the
host's own `<meta http-equiv>` copy is the **first thing in the document** (CSP composes
intersectively, so a policy the bundle declares can only narrow it). Asserted in
`artifacts_api.rs`, `golden8_generated_app.rs` and the end-to-end scenario.

**Capability denial.** 18-case application table + the HTTP matrix in golden 8. Expiry is
deliberately *not* in the HTTP trace — a 60-second wait is not a test — so the deadline is
pinned by the domain table and the application table against a controlled clock, including
the inclusive-at-deadline boundary.

**Escape.** (a) The frame carries `sandbox="allow-scripts"` with **no** `allow-same-origin`
— a static attribute, because Angular refuses to bind `sandbox` at all (NG0910), so no
runtime value can widen it; the web suite asserts the token set verbatim and that the app's
markup exists only inside `srcdoc`. (b) A non-`Bundle` artifact can never be fetched as an
app. (c) The **builder** cannot be walked out of its template directory: traversal,
absolute paths, encoded traversal and unknown ids are all "unknown template", because ids
are mapped through a closed table and never used as path components. (d) `connect-src
'none'` means a rendered app cannot open a network connection at all.

---

## 4. Security review (whole-milestone diff, `m5-complete..HEAD`)

Performed **inline in this session** rather than dispatched to the `security-auditor`
subagent, because subagent dispatch was disabled for this session. Stated plainly so the
owner can decide whether to re-run it independently before signing off; the substance below
is the six-invariant pass the subagent would have made.

**One finding was found and fixed during this pass**, and it is the kind a gate exists for:

- **G1 — the capability bridge did not apply CF-9 to an edited approval. FIXED**
  (`c746346`). The orchestrator validates the *approved* arguments against the tool's own
  schema before minting a grant, because a human may edit an approval away from the
  proposal and an edit is otherwise the one path into a grant that never met the tool's
  input rules. F6.5 introduced a **second** grant-minting site and did not carry that gate
  across. Narrow but real: an edited app-originated approval could have bound a grant to
  arguments the tool would reject, failing later inside `execute` with a grant already
  minted. Now the bridge runs the identical check, audits `approval.invalid_args`, and
  mints nothing. Regression test:
  `an_edited_approval_that_breaks_the_tools_schema_never_mints_a_grant`.

**Invariant 1 — text never grants authority.** The M5 gate recorded that
`orchestrator.rs:712` was the *only* production call site of `ToolExecutor::execute`. **That
is no longer true**: `AppBridge::exchange` is a second one, and it is the milestone's whole
point, so it is stated rather than buried. It is safe because it is the *same path*, not a
parallel one — `policy::evaluate` against the **live registry** (never
`Capability::risk()`, which is a display preview; a test registers a *read* tool at R2 and
asserts the bridge demands approval anyway), approval where the tier demands it, a real
`ExecutionGrant` minted / CF-9-validated / consumed for R2+, execution, audit. What an app
can influence is a capability from a **closed** vocabulary, a target, and one value; the
*host* builds the argument tree, so there is no wire field for a tool id, an argument name,
a count or a nested structure. The target passes the identical domain validation an app
spec's binding target does — one function, so a looser second check cannot open a hole in
the first — and the backing tool still re-resolves it through its own allowlist.

**Invariant 2 — the state machine owns the loop.** Untouched; no `RunState` arm added or
relaxed, no `_` arm anywhere.

**Invariant 3 — domain purity.** `arch-test` green. New crypto (`sha2`) and randomness
(`getrandom`) live in `jarvis-infra`; the application layer names them as ports
(`ArgumentDigest`, `CapabilityTokenStore`). No new *third-party* Rust crate entered the
tree — F6.3 enabled a feature (`tokio-util/io`) on a dependency already present, and
everything else is workspace-internal.

**Invariant 4 — cancellable.** The builder round trip and the bridge exchange both take a
`CancellationToken`; the bridge reuses the orchestrator's own `run_or_cancel` helper, so an
app-originated call is as promptly abandonable as a model-originated one. Cancellation is
re-checked before the persist phase, so a cancelled build mints no artifact.

**Invariant 5 — no secrets.** Capability token ids render as `<redacted>` in `Debug`
(`Display` remains the wire form, which is what the one authorized holder needs). Audit
payloads carry the argument **hash**, never the arguments. No worker stderr is forwarded —
a build child inherits the host environment, so its stderr may carry a credential. Only a
stable machine code crosses back into a generated app; a server sentence rendered inside a
generated app would read as the shell speaking.

**Invariant 6 — append-only audit.** Artifact + `artifact.created` are one transaction. Every
bridge refusal and every execution is an audit row, verified durably in golden 8 against
Postgres rather than against a returned error. CF-2 (audit atomicity for the separate
`PgAuditSink` transaction) remains open from M2 and was **not** made worse; closing it is a
port-signature change and is out of scope here, as the feature list recorded.

**Invariant 7 — recommendations never monetized.** Not touched.

---

## 5. BLOCKING finding for the owner

**B1 — a real paired device is denied every tool, so M6's own exit evidence is unreachable
in production.**

`jarvisd::auth::FIRST_DEVICE_SCOPES` grants a paired device `["ui"]`, and pairing is the
**only** scope-granting path in the system (grep-verified: no other write to
`identity.devices.scopes`). Every tool's `required_scopes`, meanwhile, speaks a different
vocabulary — `home:read`, `home:write`, `app:build`, `coding:patch`. `policy::evaluate`
rejects on the missing-scope arm before any risk logic, so **a real paired device cannot
execute any tool at all**: not `app.generate`, not the bridge's `home.read_state`, and not
M5's home and media tools either.

This is not an M6 regression — it predates the milestone — but M6 is where it becomes
load-bearing, and writing golden 8 is what surfaced it. It is the
**fixture-vs-caller** class for the fourth time in this project: every golden and acceptance
suite constructs `PolicyContext` directly with the scopes it needs, so all of them are green
while the real caller is denied.

Golden 8 grants its device the scope in a commented block that says exactly this, rather
than absorbing the gap silently.

**Why this is not fixed in this commit:** widening what a device is granted is an
authorization decision, and docs/11 §3 reserves those for the owner. Three options, with a
recommendation:

1. **(Recommended) The first paired device is the owner's device and receives the full tool
   scope set.** This is a single-owner, loopback-first system (docs/05 §6); the pairing code
   is the trust ceremony, and the risk tiers — not the scope list — are what gate
   consequential actions. Smallest change, restores the intended behaviour, and leaves
   scope differentiation for the M7 multi-device work docs/05 §6.3 already anticipates.
2. **Per-tool scope grants in settings.** Correct long-term, but it is a UI surface that
   does not exist and would be M8 work.
3. **Accept as a known limitation and carry it forward.** Honest, but it means M6 ships a
   feature nobody can use, and M5's home control is equally unusable.

Whichever is chosen, the fix needs the missing test: *a device paired through the **real**
pairing route can execute an allowlisted tool.* Its absence is why this survived three
milestones.

---

## 6. Deviations requested

**D-M6-1 — the production container launch profile for the app builder is deferred.**
Same shape as D-M3a-2 for the browser worker; ADR-027's process + profile-dir fallback is
what ships. The consequence is made honest rather than papered over: in the fallback the
host attests `network: enabled` in every bundle's provenance, because that is true, and
`check_provenance` **refuses** to attest `disabled` without a worker image. A build whose
provenance cannot be recorded produces no artifact. → *Recommend ACCEPT as a tracked
carry-forward, closing together with D-M3a-2 when the container profiles land.*

**D-M6-2 — a bundle's `created_by_run` is a host-minted correlation id, not the
conversational run.** `ToolInvocation` carries no run id, so `app.generate` mints one for
provenance and audit correlation. It is provenance, not authority — the same gap D-M3a-3
recorded for the coding worker, and it closes with the same orchestrator wiring. →
*Recommend ACCEPT as a tracked carry-forward, merged into D-M3a-3.*

---

## 7. ADRs needing acceptance

**ADR-029 — generated-app format: a JSON spec against a locked Vite template, over a closed
capability vocabulary.** *Proposed.* Confirms the default the owner settled on 2026-08-09;
F6.1 wrote the record rather than re-opening the option space. The load-bearing half is
closing the capability vocabulary: a bridge that enforces free-form strings enforces
nothing. → *Needs acceptance.*

**ADR-030 — generated apps render in an opaque-origin sandboxed frame, not a second loopback
origin.** *Proposed.* The M6 feature list named this choice explicitly and asked for an ADR.
Two facts decided it: jarvisd authenticates with a **bearer token**, not cookies, so an
`<iframe src>` on a second origin needs a whole new URL-token auth surface that exists for
no other reason; and a second loopback origin is **one** origin — every generated app served
from it would share `localStorage`, IndexedDB and BroadcastChannel, i.e. be same-origin with
*each other*. `sandbox="allow-scripts"` without `allow-same-origin` gives a unique opaque
origin **per frame instance**. The ADR also records, in advance, the one place this makes
life harder: an opaque origin cannot be named in a `postMessage` `targetOrigin` and arrives
as `origin === "null"`, so the bridge verifies `event.source` against the frame's
`contentWindow`. → *Needs acceptance.*

---

## 8. Carry-forwards and open risks

| Item | State |
|------|-------|
| **CF-M3a-A** (blob download buffers, no served cap) | **CLOSED** by F6.3. `BlobStore::open` streams in 64 KiB chunks under a caller cap; integrity got *stricter* — verify-then-emit, so a corrupt blob is a fail-closed error with zero bytes served. |
| **D-M5-4** (tool audit rows carry no argument binding) | **CLOSED** by F6.5. Orchestrator and bridge both bind `sha256(canonical_form(args))`, the same function the grant table binds, pinned by an infra test so the two can be joined. |
| **CF-2** (audit atomicity) | Open from M2. Not made worse; closing it is a port-signature change (human decision). |
| **D-M3a-2 / D-M3a-3** | Open; D-M6-1 and D-M6-2 fold into them. |
| **B1** (device scope provisioning) | **BLOCKING, owner decision — §5.** |
| `[apps]` config section | Documented in docs/09 §1 this milestone. |
| Flaky test | One transient failure of a `jarvis-adapters` lib test was observed once under full-workspace parallel load early in the session and never reproduced (8 consecutive isolated runs, plus every full-suite run since). Not identified; noted so a future recurrence is not treated as new. |

---

## 9. Recommendation

**Sign off M6 conditional on a decision for B1**, accepting ADR-029 and ADR-030 and
deviations D-M6-1 and D-M6-2. Everything M6 set out to build is built, demonstrated end to
end against live Postgres and a real build toolchain, and the milestone closed two
carry-forwards it inherited. B1 is not M6's bug, but M6 is the milestone that makes it
matter, and shipping a generated-app feature that no paired device can invoke would be a
gate passed with a silent exception.

On approval: tag `m6-complete`, tick the docs/08 §1 M6 row, and move ADR-029/ADR-030 to
**Accepted**.
