# M7 "Distributed rooms" — gate report

**Status: SIGNED OFF — owner approval 2026-08-13.** Tagged `m7-complete`; docs/08 §1 ticked;
ADR-031 moved to **Accepted**. Two of the four deviations were **closed after approval rather
than carried**: D-M7-1 (PR #40) and D-M7-2 (PR #39). D-M7-3 and D-M7-4 are accepted as
written — both are statements about what this hardware can demonstrate, not about the code.

**Original report follows.** Produced 2026-08-12 by the `/gate` loop (docs/11 §2)
on Opus 5. Covers `git diff m6-complete..main` plus the gate-hardening commit on
`fix/m7-gate-hardening`: 21 commits, 70 files, +8 476 / −545, eight features (F7.1–F7.8)
merged as PRs #29, #31–#37, with #30 unblocking CI first.

Requirement: **FR-19** — "Pair remote room/display nodes with scoped device capabilities."
Verification shape (docs/01 §6): *pair / revoke / reconnect tests*.

---

## 1. Exit evidence (docs/08 §1, M7 row)

| # | Evidence | Result |
|---|---|---|
| 1 | **A second node pairs** | ✅ Golden 11, over a real TLS listener with a real Ed25519 keypair through the real `/api/v1/devices/pair` route. Asserted: class `room-node` (**requested** by the node, **assigned** by the server), scopes exactly `["display-agent","voice-capture"]` with no tool scope anywhere, and `serverFingerprint` equal to the certificate the listener is actually serving. |
| 2 | **It receives a surface** | ✅ Golden 11: `POST /artifacts/{id}/open` with `node`, and the node's socket receives `display.place_surface` carrying its own `targetDeviceId` on the named monitor. Alias resolution and all five refusal paths in `display_api.rs::node_targeting`. |
| 3 | **It performs a voice/display flow** | ⚠️ **Partial — see D-M7-3.** The node's capture stream is accepted because it holds `voice-capture`; a screen-only node is refused and audited; two nodes prove only the room that heard is answered. **No STT/TTS service runs in the scenario**, so no transcript or spoken reply is produced. |
| 4 | **Revocation works** | ✅ Golden 11 revokes mid-flow: the socket closes on its own (code 1008) with the node doing nothing, and its token is dead for HTTP on the next request (401). Plus the subscribe-race, re-revocation, owner-socket-survives, and the DB last-owner guard tests. |

**Repeatable:** `cargo xtask golden` (golden 11 included) — see `docs/milestones/M7-acceptance.md`
for what is real versus substituted in that scenario, and why.

---

## 2. Measurements

| Gate | Budget | Measured | Result |
|---|---|---|---|
| Workspace tests | all green | **1 343 passed**, 74 suites, 0 failed | ✅ |
| Golden traces | mapped scenarios | **37 scenarios**, traces 1–7, 9, **11** + M3a/M3b/M5/M6 acceptance | ✅ |
| `cargo xtask arch-test` | dependency rule | 9 crates, rules hold | ✅ |
| `cargo xtask codegen --check` | no drift | up to date | ✅ |
| `cargo deny check` | no unaccepted findings | clean | ✅ |
| `cargo clippy -D warnings`, `cargo fmt --check` | clean | clean | ✅ |
| gitleaks | no secrets | clean | ✅ |
| Cold start to healthy | < 2 s (NFR-15) | **0.051 s** | ✅ |
| Idle RSS | 40–80 MB typical, 120 MB ceiling (docs/01 §4.1, 8 GB profile) | **21.8 MB** | ✅ |

Adversarial suites named by docs/06 §8: injection (4), policy/grants (14), **pairing
adversarial (13)**, delivery scope (4) — all green.

**perf-warden: PASS.** Idle RSS roughly doubled since the M2 gate (~11 MB → 21.8 MB),
explained by rustls, ed25519-dalek and the per-connection state; still 27 % of the typical
band. Every new structure is bounded and verified in code: TLS 256-connection semaphore /
10 s handshake timeout / 100 ms accept backoff, revocation channel 16, in-flight challenges
8, per-socket owned-stream deque 8. `ConnectedDevices` and `SurfaceState` are bounded by
paired-device count and deregister eagerly (presence via a `Drop` guard, surfaces cleared on
revocation). Worst case with 256 connections ≈ 22.3 MB. Idle polling is event-driven
throughout (5-minute idle interval, LISTEN/NOTIFY outbox, kernel-blocked accept).

---

## 3. Carried items — all three closed

| Item | Since | Result |
|---|---|---|
| **CF-8** — the WS hub broadcast every domain event, including `approval.requested` with its exact effect, real arguments and a decision oracle, to **every** authenticated socket; `replay_since` replayed every outbox row the same way. | M2 gate, explicitly "before M7" | ✅ **CLOSED** by F7.4. One pure `delivers_to` applied at **both** delivery sites — the split-enforcement bug this class of fix usually has is avoided. Backed by a class × event table *and* a real-router test. Mutation-checked: unfiltered, a room node sees the owner's whole run. |
| **M5 transcript fan-out** — live microphone text reached every connected socket. | M5 gate | ✅ **CLOSED** by F7.4. A voice envelope naming a stream reaches only the socket owning it, proven with two live room nodes. |
| **Per-device scope differentiation** — a second device inherited the owner's full tool scope set. | M6 gate B1 | ✅ **CLOSED** by F7.1, and closed in the direction that was missing: `DeviceClass` is now the single definition of authority, and B1's test has been given its inverse — `no_node_class_can_execute_any_registered_tool` and `a_node_is_denied_at_the_policy_engine_not_merely_by_arithmetic` drive the **real** registry through the **real** `policy::evaluate` with a context built the way the gateway builds one. |

---

## 4. Security audit

Whole-milestone pass over `m6-complete..main`. **No BLOCKING findings.** Verdict: M7 can be
signed off, conditional on the revocation-completeness gap being named rather than passing
in silence — it is D-M7-1 below.

Confirmed clean: invariant 1 (the only new text→effect path, a node's transcript, goes
through the same `RunApi::start_turn` typed input takes, with a policy context that is
empty for a node); the class gate's layering order, which puts approval resolution behind
`ui` so **a node cannot supply the human decision that mints an R2/R3 grant**; class-derived
authority with fail-closed parsing and a dropped backfill default; the pairing ceremony
(class bound at challenge issue, challenge spent on presentation, `verify_strict` +
`from_bytes` rejecting small-order and malleable forms, lockout, cap, one window one node);
the fail-closed bind rule and fingerprint provenance; transactional audit for every
authority change; no secret ever logged or serialized.

**Four of five SHOULD-FIX items were fixed at the gate rather than carried** (commit
`fix(gate): M7 audit hardening`):

- **S-1 (partially) — a revoked device's grants now die with it.** `check_and_consume`
  never asked whether the bound device was still active, so revoking a stolen `owner-ui`
  device left every already-minted R2/R3 grant consumable — and a grant carries its
  authority alone, so the attacker no longer had to be present for the effect to land. One
  predicate inside the existing transaction, failing closed on an unknown device. Breaking
  two fixtures was itself informative: both minted grants for device ids that had never
  existed in `identity.devices`, which production cannot do.
- **S-2 — the pairing surface records its refusals** (`device.pairing_refused` for wrong
  code, failed signature, class escalation; `device.pairing_window_opened` for enrolment).
- **S-3 — `reqwest`, `rcgen`, `tempfile` moved out of the daemon's runtime dependencies.**
- **S-4 — the LAN-facing accept loop is bounded** (handshake timeout, connection ceiling
  that refuses rather than queues, backoff on repeated accept errors).

---

## 5. Deviations requested

**D-M7-1 — ~~revocation does not cancel a revoked device's in-flight runs~~ — CLOSED after
sign-off (PR #40).** The live-run registry now records which device each run belongs to, so
revocation cancels exactly that device's work through the same path `POST /runs/{id}/cancel`
uses; another device's runs are untouched, and an unattributed run (which carries no tool
authority) is left alone. Original text: The grant half
is fixed (above); the run half is not. A run already executing when its device is revoked
continues with the `PolicyContext` it cached at start. Bounded by run lifetime, and every
*new* grant consumption is now refused, but it is a real gap against docs/06 §7's
"immediate revocation". Fix is a cancellation token keyed by device, wired from the
revocation bus into `RunEngine` — a contained change, but a new seam through the
orchestrator, which is not something to add during a gate. → *Recommend ACCEPT as a tracked
deviation, scheduled into M8.*

**D-M7-2 — ~~untargeted display directives fan out to every presenter~~ — CLOSED at the
gate for the part that mattered.** `MediaWindowSink::open_url` had no target parameter at
all, so cast-a-link's URL — **R1, auto-executing, and influenceable by model output derived
from untrusted web content** — reached every paired `display-agent` holder. It now takes a
target, and `[display].media_window_device` (a device id or a room alias) pins the media
window to one screen; unset keeps the pre-node behaviour exactly, which is what a
single-screen house has run for six milestones.

What remains is the deliberate part: an **untargeted `place_surface` still reaches every
presenter**, which is tested backward compatibility rather than an oversight, and whose
payload carries only surface/app-id/monitor (the artifact document route stays `ui`-gated).
→ *Recommend ACCEPT as documented behaviour; revisit if a deployment ever wants untargeted
placements pinned too.*

**D-M7-3 — exit evidence 3 shows routing, not a spoken round trip.** No Wyoming service
runs in golden 11: capture acceptance, refusal-and-audit for a screen-only node, and
"only the room that heard is answered" are all asserted, but no transcript or audio is
produced. M5 already proves the STT/TTS round trip, and CLAUDE.md's fixture-over-live rule
points this way. **D-M5-3 (the NFR-04 latency figure needs real services on reference
hardware) is unchanged and still open.** → *Recommend ACCEPT.*

**D-M7-4 — the "second node" is a second process, not a second machine.** Stated up front
in the approved feature list, consistent with docs/02 §9 and the D-M5-3 precedent. The wire
path is real in every respect the code can observe. Not shown: real-network loss, echo
cancellation on satellite hardware, cross-machine clock skew. → *Recommend ACCEPT.*

### Decisions the owner still owes (recorded during the milestone, not new)

1. **ADR-031** (`docs/adr/README.md`) is **Proposed** and is accepted at this gate:
   Ed25519 challenge-response + key-bound tokens over pinned TLS; mTLS rejected as a full
   PKI lifecycle to authenticate what a proven key already does.
2. **Three `cargo deny` acceptances** (PR #30) — `paste` RUSTSEC-2024-0436 (unmaintained,
   not vulnerable, no upgrade path), `0BSD`, `MPL-2.0`. `cargo deny` had been **red on main
   since the M5 gate**, so docs/06 §8 gate 6 was not actually passing on any PR in between.
3. **Revocation is not approval-gated** (docs/06 §3). Accepted by the security audit on the
   merits; the residual gap is named there: the last-owner guard prevents accidental
   self-lockout, **not an adversary holding a stolen owner token**, who can revoke the
   owner's other owner devices and keep their own.
4. **Feature-list deviations**: revocation shape (F7.1), class set `owner-ui` /
   `display-node` / `voice-node` / `room-node` with no `capabilities` column (F7.1), and the
   pairing window opening over the owner's API rather than `jarvisd pair --new` (F7.2,
   ADR-031 §5).

---

## 6. Open risks carried forward

- **Fingerprint delivery is trust-on-first-use.** The node learns the pin over the very
  connection the pin is meant to authenticate, so a LAN on-path attacker *during the pairing
  ceremony* wins. The mitigation is real and documented (the daemon logs the fingerprint at
  startup; the owner compares) but not enforced. — security audit, informational.
- **Stream ownership is claim-based, not exclusive.** Two sockets could both register the
  same `streamId` and both receive its transcripts; safe today only because the shell uses
  `crypto.randomUUID()`. Key by `(device_id, stream_id)` when convenient.
- **h2 can still be negotiated** without ALPN despite the `http/1.1`-only advertisement
  (`hyper_util` auto builder). Use `.http1_only()` or assert the negotiated protocol.
- **The unique index on `public_key` has no `WHERE revoked_at IS NULL`,** so a revoked
  node's key is blocked forever while the error message tells the operator to revoke and
  re-pair. Fail-closed and arguably right; the message is wrong.
- **No test exercises `WINDOW_TTL` / `CHALLENGE_TTL` expiry**, though
  `open_window_for_test(now)` makes it testable.
- **Surface memory and presence are per-process**, so a daemon restart forgets what each
  node should show. Deliberate (F7.7); persisting it would mean a restart silently
  re-lighting screens.
- **`InMemorySmtpIdempotencyStore` remains process-local** (unchanged since M4).
- **CF-2 audit atomicity** — open since M2; needs a port-signature change (human decision).

---

## 7. Process notes

Two are worth recording because they cost real time.

**Parallel review subagents corrupted the shared worktree.** Three reviewers ran at once
during F7.1; they ran `git stash`/`git restore`, which silently reverted every modified
file, and a commit made on top recorded those reverts as a partial revert of the feature.
Recovered with `git reset --hard` to the good commit. Diagnosis was slowed further by
`rtk`-filtered shell output returning stale file contents while `cargo check` reported a
cached success. **Run reviewers one at a time, and verify state by writing it to a file and
reading that file.** Both later gate reviews were run sequentially with an explicit
instruction not to mutate the tree, and neither caused a problem.

**CI's `security` job has a network dependency that flakes** — the gitleaks binary download
returned a 503 once, failing a PR that was otherwise green. A re-run passed.

---

## 8. Recommendation

**Sign off M7**, accepting D-M7-1 through D-M7-4 and the four owner decisions in §5.

On approval: tag `m7-complete`, tick the docs/08 §1 M7 row, and move ADR-031 to
**Accepted**. The next milestone is **M8 (product hardening)** — with D-M7-1 (cancel a
revoked device's in-flight runs) as its first item. The FR-17 automations slice, parked at M6 and again at the
start of M7, is still unscheduled.
