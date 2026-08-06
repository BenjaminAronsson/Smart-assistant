# M4 — Memory & quota-smart intelligence

Status: implementation complete on `feat/m4-complete`; final database-backed gate execution requires a PostgreSQL/pgvector service.

This branch contains the M4 vertical slices:

- CPU-only, lazy FastEmbed retrieval with bounded owner-scoped context, secret-shaped memory rejection, forget/expiry handling, and idle unload.
- Deterministic math and conservative home/calendar grammars.
- CalDAV read-only agenda retrieval with HTTPS, same-origin enforcement, cancellation, byte/event bounds, and sensitivity-safe `card.agenda` rendering.
- SMTP `message.send` as an opt-in R2 external tool with exact approval arguments, grant-required execution, bounded transport, and keyring-bound credentials.
- Bounded deferrable work scheduling gated on healthy provider state and quota windows, provider health scores, and deterministic evaluation fixtures.

## Validation evidence

Passed on the branch:

```text
cargo fmt --check
cargo test --workspace --no-run
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask arch-test
cargo xtask codegen --check
web: npm run lint
web: npm run build
```

Focused adapter, application, contract, daemon, and agenda-card tests pass. Full `cargo test --workspace` reaches the complete suite, but two existing SQLx tests require `DATABASE_URL` and a running Postgres service. The local Podman-backed Compose service could not start because the environment exposes `/run/user/1000/libpod` read-only; those database-backed assertions remain to be rerun on a host with Postgres/pgvector available.

The milestone must not be marked signed off until those integration tests and the SMTP approval-to-fake-server trace are rerun in that environment.
