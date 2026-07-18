---
name: ws-contracts
description: Wire contracts - DTOs, WS event envelope, JSON schema codegen to TypeScript, versioning, error codes. Use whenever touching jarvis-contracts or adding endpoints/events.
---

# Contracts and codegen

Spec: docs/05.

1. Define DTOs in `jarvis-contracts` with serde + schemars derives; discriminated unions
   via `#[serde(tag = "type", rename_all = "snake_case")]`; IDs as ULID newtypes.
2. Classify every new WS event at the type level: the event enum splits into
   `DomainEvent` (outbox-published, replayable via `since`) and `TransientEvent`
   (direct broadcast, never replayed) - the type system carries the docs/05 §3
   classification.
3. Envelope fields (v, seq, channel, type, occurredAt, traceId, resourceVersion) are
   attached by the hub, never by payload authors.
4. Run `cargo xtask codegen` => JSON Schemas => TypeScript into `web/src/generated/`;
   commit generated output. Angular imports ONLY from generated - hand-written
   duplicates are a blocking review finding.
5. Evolution: additive within a version (new optional fields with defaults; clients
   tolerate unknown enum variants). Anything else bumps `v` with a dual-emit shim
   window and a docs note.
6. Tests: serde round-trip per DTO + committed schema snapshot (CI diffs it).
7. New failure modes register a code in `jarvis-contracts::errors` AND docs/05 §7 in the
   same PR; the gateway maps through one `IntoProblem` impl - no inline problem bodies.
8. Any new persisted event type must also appear in the timeline snapshot response
   (and its test), or reconnecting clients will miss it.
