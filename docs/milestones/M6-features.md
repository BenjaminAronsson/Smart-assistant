# M6 Generated apps — feature list

Status: **APPROVED — owner sign-off 2026-08-09.** Decomposed 2026-08-09 (milestone loop,
docs/11 §2) on Opus 5, the strongest model available in this Claude Code (CLAUDE.md's
model-strategy section reserves milestone decomposition for Fable 5, else Opus —
satisfied). The three scope decisions below were resolved by the owner the same day, all
as recommended. Nothing has been implemented yet; **F6.1 is the next feature loop.**
Check items off as their PRs merge, and do not pull a future milestone's feature forward
without an approved change to this list (docs/11 §2).

M5 signed off 2026-08-09 (`docs/milestones/M5-gate-report.md`, tag `m5-complete`). This is
the first M6 decomposition attempt.

Milestone scope (docs/08 §1, M6 row): **template/spec format, sandbox builder, manifests,
CSP, capability bridge.** One requirement: **FR-18** — "Generate small local web
applications from validated templates; open them sandboxed." Governing security section is
**docs/06 §6** in full; it is short and every clause in it is a feature below.

Exit evidence (docs/08 §1, M6 row): **(1)** a dashboard app is generated; **(2)** it
cannot access undeclared capabilities; **(3)** golden 8. Traceability adds the verification
shape (docs/01 §6, FR-18 row): **CSP / capability-denial / escape tests**.

Each feature is a vertical slice sized for one session and runs the `/feature` loop
(spec → threat note → contracts/tests first → implement → review → DoD → small PR).
"Read" names the exact spec sections for that session (token discipline, CLAUDE.md).

---

## What M3–M5 already built for this milestone

M6 is unusually well-prepared — M3a reserved the seams deliberately. Do not rebuild any of
this; extend it.

- `ArtifactKind::Bundle` exists (`crates/jarvis-domain/src/artifact.rs`), maps to
  `renderer_id() == "sandboxed-webapp/v1"`, and `is_renderable_in_m3()` returns `false`
  for it. **M6 removes that last restriction** — and the method's name is now wrong;
  rename it as part of F6.4 rather than leaving an `is_renderable_in_m3` in an M6 tree.
- `Capability` (same file) is already carried through the manifest as build/provenance
  metadata, with a doc comment saying the enforcing bridge is M6. It is currently a
  **free-form `String` newtype** — see F6.1, which must close that vocabulary.
- `BuildProvenance { worker_image, lockfile_hash, network }` + `BuildNetwork::Disabled`
  exist and are exactly the fields docs/06 §6 requires a builder to record. Today every
  producer uses `BuildProvenance::none()`; the app builder is the **first real user**.
- Two out-of-process worker patterns are shipped and twice-reviewed:
  `jarvis-adapters::browser` + `tools/browser-worker`, and `jarvis-adapters::coding` +
  `tools/coding-worker` (patch-only, disposable worktree, network disabled). The app
  builder is the third instance of that pattern, not a new execution primitive.
- **ADR-027** already governs worker isolation (container = the contract, process +
  profile-dir = the dev/CI fallback). The app builder inherits it; no new isolation ADR is
  needed unless the builder needs something ADR-027 does not cover.
- Artifact CAS, immutable versioned manifests, `GET` version list + blob, `artifact.created`
  audit, the Angular `ArtifactCanvas` + renderers, `ToolRegistry` with a uniform timeout
  wrap, policy/grants/audit, and `cargo xtask golden` scenarios 1–9.

## Scope decisions (owner, 2026-08-09 — all three as recommended)

1. **FR-17 (automations) stays OUT of M6 — it gets its own slice afterwards.** docs/01 §6
   maps it to "M5+"; M5 shipped without it and docs/08's M6 row does not mention it, so it
   was a Should-have with a real design section (docs/02 §11) and no home. It is now
   scheduled as **its own small milestone or the M7 opener** — not an M6 bolt-on, because
   M6's gate would otherwise mix sandbox-escape evidence with missed-run evidence.
   Two findings from this decomposition that its future feature list should start from:
   - **It is assembly, not new construction (~2 features).** The hard reliability problem
     is already solved: `jarvis-application/src/timers.rs` (~1000 lines) +
     `jarvis-infra/src/timers.rs` fire durably and announce missed runs on restart
     (ADR-023); `jarvis-application/src/scheduler.rs` has attempts/`not_before`/health
     gating (M4); `policy::evaluate` + grants already make execution-time re-evaluation a
     matter of *calling it at fire time* rather than caching a decision; the `automation`
     schema (automations, triggers, executions) is reserved in docs/04 §3. Genuinely new:
     the Automation entity + persistence, the trigger evaluator (time + HA event
     subscription + bounded condition polling), execution identity, notification policy.
     It would also likely **close D-M4-1**, whose gap is precisely a daemon-level driver
     for due work.
   - **NanoClaw (ADR-028) cannot own the schedule; it fits as an *action*.** Considered
     and rejected as the automation engine. Its design spec contains no scheduling
     mechanism at all, so the trigger half stays Jarvis's regardless; and ADR-028's own
     accepted consequence ("Jarvis cannot see or gate nanoclaw's internal tool use — the
     grant covers the delegation, not nanoclaw's sub-actions") is the direct negation of
     FR-17's central property. The distinction that decided it: the coding worker is
     opaque **within a bounded invocation** (patch-only, disposable worktree — nothing
     survives the return), whereas a nanoclaw-authored schedule **outlives the invocation
     and re-fires**, giving a persistent unattended execution surface the policy engine
     cannot enumerate, cancel, or audit. What *does* fit: Jarvis owns the trigger, the
     execution-time policy re-evaluation and the audit row, and invokes nanoclaw as one
     ordinary gated action for work Jarvis has no business doing itself (run a script in a
     container). Separately, **read-only visibility** into nanoclaw's own schedules is
     strictly better than today's status quo and worth doing — but they must never be
     rendered as Jarvis automations, because Jarvis cannot make the execution-time-policy
     promise about them.
2. **D-M5-4 is closed inside F6.5** (approved scope addition). It was accepted at the M5
   gate as a carryforward "to be scheduled before the next physical-effect tool". The
   capability bridge is not itself a physical-effect tool, but it is a **new authority
   surface**: a generated app can reach an already-registered physical-effect tool
   (`home.set_light`) through a declared capability, and "which arguments actually
   executed" is precisely the question an app-originated call raises.
3. **The template/spec format keeps its recorded default** — "JSON spec + locked Vite
   template" (docs/08 §6). F6.1 writes the ADR confirming it rather than re-opening the
   option space: a locked template is what makes "validated templates" (FR-18) mean
   anything, and a Vite build is what `lockfileHash` in the existing manifest schema was
   shaped for.

Two more things that are *not* M6, recorded so they are not accidentally pulled in:

- **ADR-028 (NanoClaw as a policy-gated worker, FR-20) is still *Proposed*.** It is a
  different requirement, needs owner acceptance, and its design spec does not cover the
  scheduling behaviour discussed under decision 1 — if any of that becomes load-bearing,
  the mechanism has to be pinned down first. Not in this list.
- **CF-2 (audit atomicity)** stays open from M2; F6.5 must not make it worse, but closing
  it is a port-signature change (human-decision territory) and is not scoped here.

## Invariants that bite in M6

This is the milestone the invariants were written for. Every one of them is load-bearing:

- **Invariant 1 (text never grants authority) is the whole feature.** A generated app is
  model-authored content executing in a browser — the most direct "text tries to act" path
  the system will ever have. A `postMessage` from a bundle is untrusted input exactly like
  a webpage's text: it may name an operation, it may never *perform* one. There is no code
  path from a bridge message to a side effect that does not pass `policy::evaluate` and,
  for R2+, mint a real `ExecutionGrant`. A capability token is an **authorization to ask**,
  never an authorization to execute.
- **Invariant 5 (no secrets in prompts, logs, or CLI args)** extends to the sandbox: a
  bundle must never be able to read a credential, a keyring reference, or another
  artifact's blob, and a capability token must not be a bearer for anything but its own
  declared operation.
- **docs/06 §6's "no same-origin relationship with the control UI"** is a hard requirement,
  not a best effort. The v1 market-scan lesson carried in the ADR appendix says it plainly:
  *agent-editable HTML is always untrusted and never shares an origin with privileged
  surfaces.* F6.4 owns proving this, and the proof is a test, not a header review.
- **Invariant 4 (cancellable)**: builds are long, untrusted, and resource-hungry — the
  builder takes a `CancellationToken` and honours the host-owned timeout the same way
  every registered tool does.
- **docs/06 §8 gate 4** ("container/tool profiles pass filesystem and network escape
  tests") becomes checkable for the first time against a builder whose whole job is to run
  untrusted-ish dependency code. F6.7 owns it.

## Model discipline (CLAUDE.md §"Model strategy")

F6.1, F6.4, F6.5 and F6.6 touch `jarvis-domain`/`jarvis-application`, the policy path, or a
security boundary — **strong model, no exceptions**. F6.2 (Node builder worker + host
plumbing against the already-established worker pattern) and F6.3 (streaming blob read) are
tightly constrained by existing code and **may** run on Sonnet, owner's call per session.
F6.7 is harness work and may be Sonnet. `security-auditor` is **mandatory** on F6.2, F6.4,
F6.5 and F6.6 — this milestone's diff is the sandbox.

---

## Phase A — Format and contracts

- [x] **F6.1 — App spec format, closed capability vocabulary, bundle manifest contracts + ADR (domain + contracts)** · *strong model* — landed 2026-08-11 (ADR-029)
  The validated-template half of FR-18. Define the **app spec**: a JSON document naming a
  template id, the app's declared capabilities, its data bindings, and its size limits —
  validated before a build is ever started, so an invalid spec fails in the domain, not in
  a worker. Domain types in `jarvis-domain`; wire DTOs in `jarvis-contracts` +
  `cargo xtask codegen`.
  **The load-bearing change: close the capability vocabulary.** `Capability` is a
  free-form `String` newtype today (fine while it was provenance metadata). A bridge that
  enforces free-form strings enforces nothing — a host-defined, exhaustive enum of
  operations an app may request (each mapping to an existing registered tool + a risk tier)
  is what makes "undeclared capability ⇒ reject" a decidable question. Unknown capability
  in a spec ⇒ the spec is rejected at validation time, not at bridge time.
  **Produces an ADR** (docs/08 §6 decision point: template/spec format) **confirming the
  recorded default, "JSON spec + locked Vite template"** — the owner settled this on
  2026-08-09; write the record, do not re-open the option space. Tests first: a
  spec-validation table (unknown
  template, unknown capability, oversized spec, duplicate capability, empty capability set).
  Refs: FR-18, docs/06 §6, docs/04 §4, docs/08 §6. Read: docs/06 §6 in full, docs/04 §4,
  `crates/jarvis-domain/src/artifact.rs`; skills `ws-contracts`, `policy-grants`.
  Deps: none (first M6 slice). contract-keeper + rust-reviewer mandatory.

## Phase B — Build the bundle

- [ ] **F6.2 — Sandboxed app builder worker + host (adapters + tools/)** · *plumbing may be Sonnet; the ToolPolicy is strong-model*
  `tools/app-builder`: a locked template (Vite, per F6.1's ADR) built by a Node worker with
  a **dependency allowlist, a committed lockfile, network disabled, and size/time limits**,
  plus the static checks docs/06 §6 requires. `jarvis-adapters::app_builder`: a narrow
  transport trait + host mirroring `coding.rs`/`browser.rs` exactly — host-owned
  `ToolPolicy`, cancellation, output written to the CAS as an `ArtifactKind::Bundle` with a
  **real `BuildProvenance`** (worker image ref, lockfile hash, `network: Disabled`) and an
  `artifact.created` audit event. This is the first producer to populate those fields with
  anything but `none()`; a build whose provenance cannot be recorded does not produce an
  artifact.
  Isolation follows **ADR-027** unchanged (container = contract, process + profile-dir =
  dev/CI fallback). **If the builder needs something ADR-027 does not cover, stop and draft
  an ADR** rather than widening the fallback silently. Note for the gate: M3a deferred the
  *production* container launch profile (D-M3a-2) — decide in this feature whether the app
  builder ships one or inherits the same deferral, and say which in the PR.
  Refs: FR-18, docs/06 §6, ADR-027, docs/02 §12. Read: docs/06 §6, ADR-027,
  `crates/jarvis-adapters/src/coding.rs` (the pattern to copy), `tools/coding-worker/`;
  skills `provider-adapter`, `low-power` (a Node build is the heaviest thing this system
  spawns — bound it). Deps: F6.1. security-auditor + rust-reviewer + perf-warden mandatory.

- [x] **F6.3 — Streaming, size-capped blob read (infra + jarvisd)** · *may be Sonnet* — landed 2026-08-11, ahead of F6.2 (no structural dependency); CF-M3a-A closed
  Closes **CF-M3a-A**, whose stated trigger is exactly this milestone: `BlobStore::get`
  returns `Vec<u8>` and the blob endpoint buffers the whole artifact with no served-size
  cap. Markdown notes and patches were small; bundles are not. Needs a streaming/size-capped
  read port, and because verify-on-read currently re-hashes the buffered blob, streaming
  means **chunked-hash-then-emit** — the integrity check must not be the thing that forces
  buffering, and it must not be silently dropped either.
  Refs: CF-M3a-A (`docs/milestones/M3-features.md`, `M3a-gate-report.md` §5), docs/04 §1.
  Read: `crates/jarvis-infra/src/artifact_cas.rs`, `crates/jarvisd/src/artifacts.rs`;
  skill `sqlx-data`. Deps: none structurally, but must land before F6.4 serves bundles.
  perf-warden mandatory (this is a memory-footprint fix; measure it).

## Phase C — Run it, isolated

- [ ] **F6.4 — Isolated app origin, CSP, and the sandboxed surface (jarvisd + web + ADR)** · *strong model*
  The "open them sandboxed" half of FR-18 and the first two clauses of docs/06 §6: a bundle
  is served with **no same-origin relationship to the control UI**, under a restrictive CSP,
  with no arbitrary network and no direct MCP/host access. Decide and record how — a
  separate loopback origin (a distinct port is a distinct origin) versus an opaque-origin
  sandboxed iframe — **this is an ADR**, because it is the boundary everything else in the
  milestone leans on and it is expensive to move later. Today `jarvisd` serves the Angular
  SPA from a single origin via `ServeDir` and deliberately serves artifact blobs as
  `attachment` + `nosniff` so they are *never* rendered inline
  (`crates/jarvisd/src/artifacts.rs`); the bundle path is a new, separate, deliberately
  renderable route — do not weaken the existing blob route to get there.
  Web side: the surface that hosts the iframe. **Open question this list does not presume
  the answer to** — docs/12 says only that generated apps stay in the FR-18 sandbox and
  never put model-authored layout on the HUD face; whether an app opens as a full-canvas
  panel or as a dedicated window on a chosen display (reusing the M3a display-profile /
  agent path, like the media window) is for the feature spec to settle.
  Also rename `ArtifactKind::is_renderable_in_m3` and make `Bundle` renderable through this
  path only. Refs: FR-18, docs/06 §6, docs/12 §2.3, docs/02 §6/§12. Read: docs/06 §6,
  docs/12 §2.3 + §4, `crates/jarvisd/src/artifacts.rs`, `crates/jarvisd/src/api.rs`;
  skill `angular-shell`. Deps: F6.2, F6.3. security-auditor mandatory.

## Phase D — The bridge

- [ ] **F6.5 — Capability bridge: postMessage protocol, short-lived tokens, policy-gated execution (application + contracts + jarvisd + web)** · *strong model — the highest-risk feature in the milestone*
  docs/06 §6 clause 2, and the sharpest test invariant 1 has faced: *"optional interaction
  only via a `postMessage` bridge exchanging short-lived capability tokens for operations
  named in the artifact manifest; undeclared capability ⇒ reject."*
  A bridge message is untrusted input. It may **name** an operation from the app's own
  manifest and nothing else; the host then runs the ordinary path — `policy::evaluate`,
  approval where the tier demands it, a real `ExecutionGrant` for R2+, execution, audit.
  A capability token is scoped to one artifact id + version + capability + session, has a
  short TTL, and is not a bearer for anything else; an expired, forged, replayed, or
  cross-artifact token is rejected and audited. **Undeclared capability ⇒ reject + audit
  event** (this is golden 8's assertion, so it must be observable, not just a returned
  error). The origin of every inbound `postMessage` is verified against the F6.4 origin —
  never `*`, in either direction.
  **In scope, owner-approved (§"Scope decisions" #2): close D-M5-4 here** — bind the
  executed arguments (hash) into the tool-execution audit row, so an app-originated call to
  a physical-effect tool is answerable after the fact. This is a real addition to the
  feature's size; do not quietly drop it if F6.5 runs long.
  Refs: FR-18, docs/06 §6, docs/06 §4 (grants), invariant 1. Read: docs/06 §4 + §6,
  `crates/jarvis-application/src/policy.rs`, the M5 grant/approval path; skills
  `policy-grants`, `ws-contracts`. Deps: F6.4. security-auditor mandatory; tests-first is
  non-negotiable here (policy/grant surface, CLAUDE.md).

- [ ] **F6.6 — The dashboard app, end to end (application + adapters + web)** · *strong model*
  **Exit evidence #1.** The generation path as a user experiences it: a request produces an
  `app.generate` tool proposal → policy/approval → F6.2 builds → a `Bundle` artifact lands
  in the CAS with provenance → it opens in the F6.4 sandbox → it renders live data it
  declared and was granted (an existing read capability — HA state, timers, or lists; pick
  one already-registered tool rather than adding a new one). Reopenable after restart, like
  every other artifact since M3a.
  Refs: FR-18, docs/08 §1 (M6 exit evidence #1). Read: whatever F6.1–F6.5 produced, plus
  `docs/13-use-case-catalog.md` for the dashboard's realistic content. Deps: F6.5.
  security-auditor + rust-reviewer mandatory.

## Phase E — Prove it

- [ ] **F6.7 — Golden 8 + CSP / capability-denial / escape suite (xtask + tests)** · *may be Sonnet*
  **Exit evidence #2 and #3.** `cargo xtask golden` scenario 8 (docs/07 §2): *"generated app
  requests an undeclared capability; bridge rejects"* — against fixture adapters, per
  CLAUDE.md's fixture-over-live rule. Plus the three test classes docs/01 §6 names for
  FR-18 and docs/06 §8 gate 4 requires:
  **CSP** — the served bundle carries the intended policy and a violation is observable;
  **capability denial** — undeclared, expired, forged, replayed, and cross-artifact tokens
  all rejected and audited; **escape** — a bundle cannot reach the control origin, cannot
  read another artifact's blob, cannot open arbitrary network connections, and the
  *builder* passes filesystem/network escape tests (docs/06 §8 gate 4, checkable for the
  first time here).
  Refs: docs/07 §2, docs/01 §6 (FR-18 row), docs/06 §8. Read: docs/07 §2, existing
  `crates/xtask/src/main.rs` golden harness; skill `golden-traces`. Deps: F6.1–F6.6.

---

## Explicitly out of scope for M6

- **FR-17 automations** — see §"Scope decisions" #1; its own slice after M6 (own small
  milestone or the M7 opener), with the two starting findings recorded there.
- **FR-20 / ADR-028 NanoClaw worker** — ADR still *Proposed*; separate requirement.
- **FR-19 distributed rooms / device pairing** — M7.
- **App *editing* / iteration by the user, an app store, app persistence of its own data
  beyond declared capabilities** — none of these are in FR-18 or docs/06 §6. FR-18 says
  "small local web applications from validated templates"; anything that turns generated
  apps into a platform is a new requirement, not an M6 feature.
- **Golden 10 + the installer/backup/policy-UI/accessibility surface** — M8.
- **CF-2 audit atomicity** — open from M2, needs a port-signature change (human decision).
