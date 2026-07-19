-- outbox schema seed (docs/04 §3, docs/02 §2). Domain events insert here in
-- the same transaction as the state change; the event-driven dispatcher
-- (Postgres NOTIFY, no polling) lands with the WS hub in M1.

CREATE SCHEMA outbox;

CREATE TABLE outbox.outbox_events (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    event_type    TEXT NOT NULL,
    payload       JSONB NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    dispatched_at TIMESTAMPTZ
);

CREATE INDEX outbox_undispatched_idx
    ON outbox.outbox_events (id)
    WHERE dispatched_at IS NULL;
