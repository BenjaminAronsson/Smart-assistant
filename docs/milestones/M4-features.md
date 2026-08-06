# M4 — Memory & quota-smart intelligence

Status: implementation complete on `feat/m4-complete`, including a follow-up hardening pass; full database-backed validation now passes against a live Postgres/pgvector service on the dev host.

This branch contains the M4 vertical slices:

- CPU-only, lazy FastEmbed retrieval with bounded owner-scoped context, secret-shaped memory rejection, forget/expiry handling, and idle unload.
- Atomic memory writes: `EmbeddedMemoryStore`/`MemoryWriteService` embed and persist the vector + audit row in one transaction, so a provider failure cannot leave a stale/missing embedding searchable.
- Memory context provenance: retrieval hits actually included in a run's assembled prompt are recorded to `memory.context_provenance` (docs/02 §7 "records which memories influenced a run"), best-effort and non-blocking.
- Deterministic math and conservative home/calendar grammars; `DeterministicFirstProvider` now routes recognized home-intent utterances before opening the reasoning provider.
- CalDAV read-only agenda retrieval with HTTPS, same-origin enforcement, cancellation, byte/event bounds, and sensitivity-safe `card.agenda` rendering.
- SMTP `message.send` as an opt-in R2 external tool with exact approval arguments, grant-required execution (now including a grant/invocation argument-fingerprint match), bounded transport, keyring-bound credentials, and per-grant send idempotency (a replayed approval no longer double-sends).
- Bounded deferrable work scheduling (`DeferrableScheduler` + a cancellable, single-flight `DeferredWorkExecutor`) gated on healthy provider state and quota windows, provider health scores, and deterministic evaluation fixtures.

## Validation evidence

Passed on the branch, against a live Postgres/pgvector service (`docker compose -f infra/compose/dev.yml up -d postgres`):

```text
cargo fmt --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask arch-test
cargo xtask codegen --check
cargo xtask golden
web: npm run lint
web: npm run build
```

All SQLx DB-backed tests now run and pass, including a new integration suite for `PgMemoryStore` (`crates/jarvis-infra/tests/memory.rs`) covering create/get/list/replace/forget/embeddings/context-provenance — this store previously had zero live-database coverage. That work also caught and fixed two real bugs: a missing `rank` column on `memory.context_provenance` (migration 0014) that would have failed every `record_context` call at runtime, and a pre-existing double-escaped `LIKE ... ESCAPE` clause in `PgMemoryStore::list` that broke every text-query memory search.

## Known carryforward (not a gate blocker, flagged for owner review)

`DeferredWorkExecutor` is implemented and unit-tested but **not yet wired into `jarvisd`**: there is no background task driving it, no concrete `DeferredWorkHandler` for real quota-windowed work (e.g. episodic summarization per docs/03 §4), and no existing mechanism that derives a `QuotaWindow` from provider health signals. `MemoryWriteService`/`EmbeddedMemoryStore` are similarly tested but have no production call site yet, since memory creation is intentionally not exposed as a feature in M4 (see `crates/jarvisd/src/memories.rs` header comment — creation is deferred to a future explicit-confirmation/candidate-extraction feature). Both are left as tested, intentionally-unwired application-layer seams (the same pattern the codebase already uses for `RunState::UnwiredInM1`), rather than inventing an unspecified trigger/design. This means the roadmap's "deferred summarization runs in a healthy-quota window" exit evidence is demonstrated today only at the scheduler-mechanism/unit-test level, not end-to-end in the running daemon — an M2-style deviation, for the owner to accept or scope into a follow-up milestone at gate time.
