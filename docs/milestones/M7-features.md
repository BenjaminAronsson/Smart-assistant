# M7 Distributed rooms — feature list

Status: **APPROVED — owner sign-off 2026-08-12.** Decomposed 2026-08-11 (milestone loop,
docs/11 §2) on Opus 5, the strongest model available in this Claude Code (CLAUDE.md's
model-strategy section reserves milestone decomposition for Fable 5, else Opus —
satisfied). All three scope decisions below were resolved by the owner as recommended.
Nothing has been implemented yet; **F7.1 is the next feature loop.** Check
items off as their PRs merge, and do not pull a future milestone's feature forward without
an approved change to this list (docs/11 §2).

M6 signed off 2026-08-11 (`docs/milestones/M6-gate-report.md`, tag `m6-complete`). This is
the first M7 decomposition attempt.

Milestone scope (docs/08 §1, M7 row): **device keys/pairing, remote display/voice nodes,
mTLS/private network, resync.** One requirement: **FR-19** — "Pair remote room/display
nodes with scoped device capabilities" (docs/01 §6 verification shape:
*pair / revoke / reconnect tests*). Governing sections: **docs/06 §2 (trust zones), §5
(remote node impersonation), §7 (network)**; **docs/05 §6 (auth model, §6.5 is literally
"M7 upgrade path")**; **docs/02 §13 (whole-house evolution)**; **NFR-13** (clients resync
after event gaps).

Exit evidence (docs/08 §1, M7 row): **(1)** a second node pairs; **(2)** it receives a
surface; **(3)** it performs a voice/display flow; **(4)** revocation works.

Each feature is a vertical slice sized for one session and runs the `/feature` loop
(spec → threat note → contracts/tests first → implement → review → DoD → small PR).
"Read" names the exact spec sections for that session (token discipline, CLAUDE.md).

---

## The hardware constraint, stated up front

There is no second machine. Per docs/02 §9 ("do not block the milestone on hardware") and
the precedent set by D-M5-3, M7 demonstrates the **wire path**, not the purchase order:
the "second node" is a **second `jarvis-agent` process** that pairs over the real
`/api/v1/devices/pair` route, over a real TLS listener bound to a **non-loopback**
address, with its own keypair, its own narrow scope set, and no access to the owner
device's token. That is the whole of what a room satellite is on the network; a different
chassis changes nothing the code can observe.

What this deliberately cannot demonstrate: real-network loss characteristics, echo
cancellation on satellite hardware, and cross-machine clock skew. Those are named as
carry-forwards at the gate, not silently skipped. **Do not** substitute a fixture for the
pairing route or for the TLS handshake — this milestone is the one where the
fixture-vs-caller class (four hits: M5 ×3, M6 gate B1) would be most expensive, because
the "caller" here *is* the milestone's exit evidence.

## What M0–M6 already built for this milestone

M7 is more assembly than construction. Do not rebuild any of this; extend it.

- **Device identity exists.** `jarvis_domain::identity::Device` (id, user, name,
  `token_hash`, `scopes`, `revoked_at` + `is_active()`), `identity.devices` in
  `migrations/0001_identity_init.sql`, and `IdentityStore` behind a port. Tokens are
  stored hashed only; the value exists transiently at the gateway.
- **Pairing exists in its M0 form.** `crates/jarvisd/src/auth.rs` — one-time 6-digit
  code, journal + loopback health-page display, single-use consumption, digest comparison,
  a 5-attempt brute-force lockout, `POST /api/v1/auth/pair` → `{ deviceId, deviceToken,
  scopes }`. M7 adds the *second* device path beside it, it does not replace the bootstrap.
- **Scopes are enforced for real.** `policy::evaluate` rejects on the missing-scope arm
  before any risk logic, and `jarvisd::tools::scope_coverage_tests` walks the **real**
  registration paths (the B1 fix). `FIRST_DEVICE_SCOPES` (`auth.rs:49`) documents in its
  own comment that "a *second* device stops inheriting this set" at M7 — **F7.1 is where
  that promise comes due.**
- **The WS hub, resync cursor and replay exist.** `crates/jarvisd/src/ws.rs` (~1.8 kloc):
  outbox-backed `seq`, `?since=` replay with paging, `broadcast::Lagged` → resync-via-REST,
  and the deliberate distinction between durable domain events (advance the cursor) and
  ephemeral current-value readouts (do not). M7 adds **filtering**, not a new transport.
- **A real WS client node exists.** `crates/jarvis-agent/` — `client.rs` (WS client with
  reconnect), `handler.rs`, `compositor.rs` (Hyprland IPC), shipped and reviewed in M3a.
  This is the second node's chassis; node mode is a flag on it, not a new crate.
- **Display surfaces and profiles exist.** `jarvisd::display` + `POST
  /api/v1/artifacts/{id}/open` place an artifact on a selected monitor (M3a). M7 makes the
  *node* addressable, the monitor already is.
- **The voice pipeline exists end to end.** `jarvis-adapters::wyoming` (41 kB) + the M5
  push-to-talk / VAD / barge-in path, with binary WS PCM frames (16-bit LE, 16 kHz, mono)
  already specified in docs/05 §1 and implemented. M7 changes *which socket* the frames
  come from and go to.
- **`GET /api/v1/devices` and revocation UI do not exist.** docs/05 §6.4 promises
  "immediate per-device token revocation via settings"; today revocation is a DB fact with
  no route. F7.1 owns this.

## Carry-forwards this milestone must close

These are already assigned to M7 by earlier gates — they are not new scope, and F7.4 is
sized to hold all three:

| Item | Source | Where it lands |
|---|---|---|
| **CF-8** — the WS hub broadcasts every domain event (incl. `approval.requested` with its `exactEffect`, real arguments and a decision oracle) to **every** authenticated connection, and `replay_since` replays all outbox rows, with no per-user/session/device filter. Dormant only because the deployment is single-user loopback. | M2 gate / `M2-security-carryforward.md` (explicitly "**before M7**") | **F7.4** |
| **`broadcast_voice_transcript` fans live microphone transcripts to every connected socket** — should become scope- or socket-targeted once `voice-capture` / `display-agent` differentiation lands. | M5 gate §6 ("carry to M7") | **F7.4** |
| **A second device inherits the owner's full tool scope set** — `FIRST_DEVICE_SCOPES` is correct for the owner's device and wrong for a room satellite. | M6 gate B1 resolution (option 1, deferring differentiation to M7) | **F7.1** |

---

## Scope decisions (owner, 2026-08-12 — all three as recommended)

**1. FR-17 automations: in or out?** The M6 feature list parked it as "its own small
milestone **or the M7 opener**". **Recommendation: OUT.** M7's gate has to prove
pair/revoke/reconnect under a threat model about *impersonation*; mixing in missed-run and
trigger-evaluation evidence is exactly the mistake M6 avoided by parking it in the first
place. It is ~2 features of assembly over `timers.rs`/`scheduler.rs` and would likely
close D-M4-1 — it deserves its own gate, either before M7 (a 2-feature "M6.5") or after.

**2. Node authentication mechanism (ADR-031, drafted in F7.2).** docs/06 §5 says
"challenge-response pairing, per-device keys, **mTLS or signed tokens**, revocation,
capability scopes" — the "or" is the decision. **Recommendation: per-device Ed25519 keys +
challenge-response pairing + the existing opaque bearer token bound to the key, over
server TLS with a pinned certificate fingerprint.** mTLS is rejected for a single-owner
system: it needs a CA, per-device cert issuance, renewal, and a revocation channel (CRL
or short-lived certs) — an entire PKI lifecycle to authenticate what is already
authenticated by a key the node proves possession of at pairing and on every reconnect.
The ADR records that, so the choice is deliberate rather than incidental.

**3. Transport for remote audio.** docs/08 §6 defaults to "WS binary PCM frames" with
WebRTC/LiveKit as the M7 option. **Recommendation: stay on WS PCM frames.** LiveKit is
justified by unreliable networks, echo cancellation, and video/telephony — none of which
a same-LAN satellite needs, and all of which would dominate the milestone. Revisit when a
node crosses a network the owner does not control.

**Resolved 2026-08-12: all three as recommended.** FR-17 stays out of M7 and keeps its own
slice; ADR-031 will record Ed25519 device keys + challenge-response + a key-bound token
over pinned-fingerprint TLS, with mTLS considered and rejected for a single-owner system;
remote audio stays on WS binary PCM frames until a node crosses a network the owner does
not control.

---

## Features

- [ ] **F7.1 — Device classes, per-class scope sets, device list + revocation (FR-19)**
      · *strong model*
      The identity model stops assuming one device. Domain: a `DeviceClass` value type
      (`owner-ui`, `display-node`, `voice-node`, and the existing `display-agent` for the
      local Hyprland agent), each mapping to an explicit scope set — a room satellite gets
      presentation and capture scopes and **no tool scopes at all**; the owner's device
      keeps `FIRST_DEVICE_SCOPES`. Migration `0015_identity_device_class.sql` adds
      `device_class`, `capabilities`, `last_seen_at`, `revoked_reason` (existing rows
      backfill to the owner class). Contracts: device DTOs + `GET /api/v1/devices`,
      `POST /api/v1/devices/{id}/revoke` (R2 → approval + audit `device.revoked`).
      Revocation must **fail closed on the next request *and* the next WS frame** — an
      open socket is the interesting case, and it is the one the exit evidence tests.
      Tests: per-class scope coverage built from the **real** registration paths (extend
      `jarvisd::tools::scope_coverage_tests`, do not fork it); a `display-node` token is
      denied `home.control` by `policy::evaluate`; revoke-while-connected drops the socket.
      Refs: docs/05 §6.3/§6.4, docs/04 §2, docs/06 §2. Read: `crates/jarvisd/src/auth.rs`,
      `crates/jarvis-domain/src/identity.rs`, `jarvisd::tools` scope tests; skills
      `policy-grants`, `sqlx-data`, `ws-contracts`. Deps: none.

- [ ] **F7.2 — Challenge-response pairing with per-device keys (FR-19, ADR-031)**
      · *strong model*
      `POST /api/v1/devices/pair`, the route docs/05 §1 has reserved since M0. Flow: the
      node generates an Ed25519 keypair and posts `{ publicKey, deviceName, requestedClass,
      pairingCode }`; the server verifies the code against an **owner-opened pairing
      window** (`jarvisd pair --new`, TTL-bounded, one node per window, reusing the M0
      lockout), returns a random challenge; the node signs it; the server verifies the
      signature against the presented key, persists the key, and issues a device token
      **bound to that key** — the class decides the scopes, the node does not (a node
      requesting `owner-ui` is refused, not upgraded). Replay protection: challenges are
      single-use, TTL-bounded, and bound to the public key that requested them. Audit
      `device.paired` with class, key fingerprint, and the scopes granted. Draft **ADR-031**
      (decision 2 above) in this feature; it stays *Proposed* until the gate.
      Tests: happy path; wrong code; expired/replayed challenge; signature from a
      different key; class escalation attempt; concurrent pairing attempts on one window;
      pairing while the DB is unreachable fails closed.
      Refs: docs/05 §6.1/§6.5, docs/06 §5 ("remote node impersonation"), docs/04 §2.
      Read: F7.1's output, `crates/jarvisd/src/auth.rs`; skills `policy-grants`,
      `ws-contracts`. Deps: F7.1.

- [ ] **F7.3 — TLS listener + private-network binding (docs/06 §7)** · *strong model*
      Today `jarvisd` binds loopback and that is the entire network security model. This
      feature makes a non-loopback bind possible **and safe**: a rustls listener,
      `[network] bind` / `tls_cert` / `tls_key` config with validation, a
      self-signed-certificate provisioning path whose **fingerprint is returned in the
      pairing response** so the node pins it (this is what makes F7.2's challenge-response
      meaningful against a LAN attacker), and a fail-closed rule with no override:
      **a non-loopback bind without TLS refuses to start.** The unauthenticated health
      endpoint stays loopback-only when the listener is not loopback. Ops: docs/09 gains
      the firewall + private-overlay (Tailscale) section docs/06 §7 promises; public port
      forwarding is documented as never.
      Tests: config validation matrix (bind × TLS present/absent); startup refusal;
      health-endpoint exposure test; fingerprint stability across restarts; a plaintext
      client against the TLS listener fails.
      Refs: docs/06 §7, docs/09 §1, docs/02 §12. Read: `crates/jarvisd/src/main.rs`
      listener setup, `crates/jarvisd/src/config.rs`; skill `low-power` (a TLS stack is a
      resident-memory decision — measure it). Deps: F7.2 (fingerprint in the pair response).

- [ ] **F7.4 — Per-connection event scoping + node presence (CF-8, NFR-13)**
      · *strong model*
      The security heart of the milestone. Every WS delivery path becomes filtered by the
      connection's device: live fan-out, `replay_since`, and `broadcast_voice_transcript`.
      A `display-node` socket receives display commands addressed to it and the session
      events its surface needs — **never** `approval.requested` (payload + decision
      oracle), never another node's transcript, never memory or artifact-blob events. The
      node registry lands here too: presence events (`node.online` / `node.offline` with
      class and capabilities), `last_seen_at` persistence, and a bounded in-memory registry
      (perf-warden reviews the per-connection state).
      Tests: a table-driven matrix of (device class × event type) → delivered/not, run
      against the **real** hub, not a mock; the same matrix over `?since=` replay (a filter
      that only exists on the live path is the classic version of this bug); revoked
      device stops receiving mid-stream; presence transitions on connect/disconnect/kill.
      Refs: `M2-security-carryforward.md` CF-8, M5 gate §6, docs/05 §3, NFR-13.
      Read: `crates/jarvisd/src/ws.rs`; skill `ws-contracts`. Deps: F7.1.

- [ ] **F7.5 — Addressable surfaces: route a surface to a node (exit evidence 2)**
      · *Sonnet*
      `POST /api/v1/artifacts/{id}/open` gains an optional target: a device id or a **room
      name** resolved through config aliases (same shape as `[integrations.spotify]
      device_aliases`, docs/02 §11). Resolution order and failure honesty are the whole
      feature: an unknown room, an offline node, or a node lacking the display capability
      produces a clean, user-visible failure — never a silent local fallback that makes
      "put it on the kitchen screen" look like it worked. The M3a display-profile logic is
      extended, not replaced; a targeted command is delivered only to that node (F7.4's
      filter is what makes that true).
      Tests: route to node; unknown room; offline node; node without capability; targeted
      delivery asserted at the socket boundary (no other socket sees it).
      Refs: docs/05 §1 (`artifacts/{id}/open`), docs/02 §11/§13, docs/12 (surface cards).
      Read: `crates/jarvisd/src/display.rs`, `crates/jarvis-agent/src/handler.rs`.
      Deps: F7.4.

- [ ] **F7.6 — Remote voice node: capture and playback over the paired socket
      (exit evidence 3)** · *strong model*
      A `voice-node` streams PCM frames up its own socket and receives TTS frames back on
      it. The routing rule is the design decision: **the answer is spoken by the node that
      heard the request** (and its surface, if it has one, is where a card lands), with
      barge-in and cancellation propagating to the right node only. Scope-gated on
      `voice-capture`; a node without it that sends audio frames is disconnected and
      audited, not ignored. The M5 Wyoming pipeline and cancellation semantics are reused
      wholesale — this feature owns *routing and framing across sockets*, nothing about
      STT/TTS itself.
      Tests: frames from an unscoped socket rejected + audited; two nodes, only the
      capturing one hears the reply; barge-in cancels on the right node while the other is
      untouched; a mid-utterance revocation stops audio (this is exit evidence 4 meeting
      exit evidence 3); malformed/oversized frames rejected without killing the run.
      Refs: docs/05 §1 (binary frames), docs/02 §9, NFR-04. Read: `jarvis-adapters::wyoming`,
      the M5 voice path in `jarvisd::ws`; skill `ws-contracts`. Deps: F7.4, F7.5.

- [ ] **F7.7 — Node reconnect and resync (NFR-13, docs/01 §6 "reconnect tests")**
      · *Sonnet*
      What a satellite does after a gap. Durable domain events replay from the node's
      cursor **through F7.4's filter**; ephemeral state (the current surface, the current
      media/now-playing readout) is **re-asserted, not replayed** — a node that reconnects
      must end up showing what it should be showing now, without a backlog of stale
      display commands. Covers `broadcast::Lagged`, a `jarvisd` restart with nodes
      connected, a node restart, and a token revoked while the node was away (fails
      closed at reconnect, with the reason surfaced rather than an opaque 401 loop).
      Tests: gap → filtered replay → identical final state; restart on both sides;
      lagged consumer; revoked-while-away; cursor monotonicity under reconnect churn.
      Refs: NFR-13, docs/05 §3, docs/02 §12. Read: `ws.rs` replay/cursor code, F7.4's
      filter, `crates/jarvis-agent/src/client.rs`. Deps: F7.4, F7.5.

- [ ] **F7.8 — Second-node reference client + golden 11 + M7 acceptance scenarios**
      · *Sonnet*
      `jarvis-agent --node` : the reference satellite. It generates its keypair, pairs
      through the **real** route with a **real** pairing code against a **real** TLS
      listener, pins the fingerprint, stores its token in the keyring, connects, receives a
      surface, runs a voice turn, and dies clean on revocation. Then the executable
      milestone evidence: **golden 11** ("a second node pairs, receives a surface, performs
      a voice/display flow; revocation cuts it off mid-flow") wired into `cargo xtask
      golden`, and `docs/milestones/M7-acceptance.md` with one named, repeatable scenario
      per exit-evidence item. docs/07 §2 gains scenario 11 (the list stops at 10 today).
      **No fixture may stand in for the pairing route, the TLS handshake, or the scope
      set the node actually receives** — this is the milestone's answer to the
      fixture-vs-caller class, and the gate will look for exactly that.
      Refs: docs/07 §2, docs/08 §1 (M7 row), docs/01 §6 (FR-19 row). Read: existing
      `crates/xtask/src/main.rs` harness + an M5/M6 acceptance file for shape; skill
      `golden-traces`. Deps: F7.1–F7.7.

---

## Explicitly out of scope for M7

- **FR-17 automations** — scope decision 1 above; its own slice.
- **FR-20 / ADR-028 NanoClaw worker** — ADR still *Proposed*; separate requirement.
- **NATS JetStream** — docs/08 §6 gates it on "a second *machine* needs durable
  messaging". A second process on one host does not; the in-process outbox stands.
- **WebRTC / LiveKit** — scope decision 3.
- **Multi-*user*** — M7 is multi-*device*, single-owner. CF-8's filter is written with a
  `UserId` dimension so multi-user is a later population of the same seam, but no user
  management, invitations, or per-user policy lands here.
- **Wake word / room attribution** — docs/02 §9 orders it after M5's push-to-talk chain and
  the engine/licensing decision is still open (docs/08 §6). A node that hears the *whole*
  house is a different requirement from a node that hears *its* room.
- **Installer, backup/restore, policy UI, accessibility, diagnostics bundle, golden 10** — M8.
- **CF-2 audit atomicity** — open from M2, needs a port-signature change (human decision).

## Not triggered by this milestone (checked, recorded)

- **D-M5-4** (audit binds only the tool id) — **already closed** by M6's F6.5: orchestrator
  and capability bridge both bind `sha256(canonical_form(args))`, the same function the
  grant table binds (M6 gate report §"Deviations"). Nothing to schedule here; recorded
  because M5 filed it against "the next physical-effect tool" and M7 adds no new tool.
- **D-M4-1** (deferred summarization has no daemon-level driver) — belongs to the FR-17
  slice, which is where the driver naturally lands.
