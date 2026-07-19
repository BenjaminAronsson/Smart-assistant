-- conversation: idempotency key for session creation (docs/05 §2 — every
-- side-effecting command carries an idempotency key; NFR-13). Nullable:
-- internal/system creates have no client key.

ALTER TABLE conversation.sessions
    ADD COLUMN idempotency_key TEXT;

CREATE UNIQUE INDEX sessions_idempotency_key_idx
    ON conversation.sessions (idempotency_key)
    WHERE idempotency_key IS NOT NULL;
