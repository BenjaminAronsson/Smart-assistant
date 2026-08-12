# M7 acceptance — repeatable exit evidence

Every item in docs/08 §1's M7 row, as a named scenario anyone can re-run. One command:

```bash
docker compose -f infra/compose/dev.yml up -d postgres
cargo xtask golden          # golden 11 is the M7 scenario
```

Or the scenario alone:

```bash
cargo test -p jarvisd --test golden11_node
```

## What golden 11 actually exercises

`crates/jarvisd/tests/golden11_node.rs` is one test that walks the whole milestone in the
order an owner lives it. It is deliberately built out of production parts:

| Piece | Real or substituted |
|---|---|
| Postgres, migrations, audit chain | **real** (`#[sqlx::test]`, throwaway database) |
| HTTP router, middleware, class gate | **real** (`jarvisd::api::router_with`) |
| TLS listener | **real** (`jarvisd::tls::serve`, certificate minted per run) |
| Certificate trust | **pinned** — the client trusts that certificate and nothing else |
| Pairing route + Ed25519 signature | **real** (`/devices/pair`, `/pair/complete`) |
| Device class → scopes | **real** (`jarvis_domain::identity::DeviceClass`) |
| WebSocket, delivery filter, revocation | **real** |
| Artifact store, placement audit sink | substituted — *which artifact exists* is not what M7 proves |
| Speech-to-text service | absent — M5's territory (see §3) |

That table is the point. This project's most expensive recurring bug is a fixture that
builds its inputs its own way and therefore agrees with nothing (three times in M5, again
at the M6 gate). M7's exit evidence **is** a claim about callers — "a second node pairs"
is a claim about the pairing route, the TLS handshake, and the scope set the node actually
receives — so anything substituted there would make the evidence worthless.

## 1. A second node pairs (exit evidence #1)

The owner pairs first over TLS, then opens a pairing window
(`POST /api/v1/devices/pairing-window`, `ui`-scoped). The node generates an Ed25519
keypair, presents its **public** key with the code, receives a single-use challenge, signs
it, and receives its token.

Asserted:

- the assigned class is `room-node` — **requested** by the node, **assigned** by the server;
- its scopes are exactly `["display-agent", "voice-capture"]` — a satellite is toolless by
  construction, with no tool scope anywhere in the list;
- `serverFingerprint` in the pairing response **equals the fingerprint of the certificate
  the listener is actually serving**, which is what makes pinning meaningful (ADR-031).

Related adversarial cases live in `crates/jarvisd/tests/pairing_api.rs`: wrong code (with
the 5-attempt lockout), replayed challenge, wrong-key signature, class escalation,
challenge flood, spent window, duplicate key, and pairing into a house with no owner.

## 2. It receives a surface (exit evidence #2)

The owner places an artifact on the node by device id
(`POST /api/v1/artifacts/{id}/open` with `node`). The node's socket receives
`display.place_surface` carrying `targetDeviceId` equal to its own id, on the monitor the
owner named.

Room-name aliases (`[display].node_aliases`) and every way a placement can fail —
unknown room, never-paired, revoked, screenless, not connected, each a visible
`display.node_unavailable` that dispatches nothing — are covered in
`crates/jarvisd/tests/display_api.rs::node_targeting`.

## 3. It performs a voice/display flow (exit evidence #3)

The node opens a capture stream and sends PCM. Because a `room-node` holds
`voice-capture`, the stream is **accepted** — no refusal comes back.

**What this does not show:** a transcript, an answer, or spoken audio. No speech service is
wired in the scenario, deliberately — fixture-driven over live services is the standing
rule (CLAUDE.md), and M5 already proves the STT/TTS round trip. What M7 owns is *routing
and framing across sockets*, and that is asserted separately:

- `ws_stream.rs::a_screen_only_node_cannot_open_a_voice_stream` — a display-only node is
  refused, told so, and the attempt is audited (`voice.capture_denied`);
- `ws_stream.rs::only_the_node_that_heard_the_request_hears_the_answer` — two room nodes
  connected, one speaks, the other receives nothing;
- `ws.rs::delivery_scope_tests` — the class × event matrix, including that no node class
  ever receives an approval card.

**Carried forward:** the NFR-04 end-to-end latency figure still needs real Wyoming services
on reference hardware (D-M5-3, unchanged by this milestone).

## 4. Revocation works (exit evidence #4)

Mid-flow — with the node's socket open and its capture stream running — the owner revokes
it. Asserted:

- the socket **closes on its own**, with close code 1008, without the node doing anything;
- its token is dead for HTTP on the next request (401).

Neighbouring cases: `ws_stream.rs::revoking_a_node_closes_its_live_socket` (the owner's own
socket survives someone else's revocation),
`a_socket_opened_after_revocation_is_closed_by_the_upgrade_recheck` (the subscribe-race),
`a_repeated_revocation_still_closes_a_surviving_socket` (re-revoking is not a no-op), and
`identity.rs::the_last_owner_guard_locks_the_owner_set_before_deciding` (the DB guard
really takes its lock).

## What this milestone cannot demonstrate on this hardware

The "second node" is a second **process**, not a second machine — stated up front in
`M7-features.md` and consistent with docs/02 §9 ("do not block the milestone on hardware")
and the D-M5-3 precedent. The wire path is real in every respect the code can observe:
real TLS, real pairing, real keys, real sockets, a real class boundary.

Not shown, and carried to the gate:

- real-network loss and latency characteristics;
- echo cancellation on satellite hardware;
- cross-machine clock skew;
- a `jarvisd` restart with nodes connected — surface memory is in-process by design (F7.7),
  so a restart legitimately forgets and the owner places again.
