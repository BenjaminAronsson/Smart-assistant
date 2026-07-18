---
name: sqlx-data
description: Database work - sqlx migrations, schema-per-module ownership, repositories, transactional outbox, audit writes. Use whenever touching jarvis-infra persistence or migrations.
---

# Persistence with sqlx

Spec: docs/04, ADR-008.

1. Migrations: single ordered stream in `migrations/`, filename prefix names the module
   (`0007_tools_add_grant_consumed_at.sql`). Never edit an applied migration.
   Destructive ones carry a `-- DESTRUCTIVE:` header and the backup-gate note.
2. Schema-per-module: a repository in module X touches only X's schema; cross-module
   reads go through the owning module's port. Arch-test greps enforce this - don't
   fight them with views.
3. Queries compile-time checked (`sqlx::query!`/`query_as!`); run `cargo sqlx prepare`
   after query changes and commit `.sqlx/`; CI runs `--check`.
4. Repositories implement `jarvis-application::ports` traits and return domain types,
   never rows; mapping lives in infra. Every repo method takes `impl PgExecutor` so use
   cases control transaction scope.
5. **Transactional outbox**: domain events insert into `outbox.outbox_events` IN THE
   SAME TRANSACTION as the state change; the dispatcher is event-driven (Postgres
   NOTIFY, not polling - perf-warden checks) and publishes to the WS hub, then marks
   dispatched. Never publish before commit.
6. **Audit**: insert into `audit.audit_events` in the same transaction as the side
   effect; hash chain = sha256(prev_hash || canonical_json(event)); application role has
   INSERT only (dedicated DB role in prod config). Tests: chain verification over a
   seeded sequence; attempted UPDATE fails.
7. pgvector: embeddings under memory schema with an hnsw index; memory forget cascades
   to embeddings (test the cascade - FR-16).
