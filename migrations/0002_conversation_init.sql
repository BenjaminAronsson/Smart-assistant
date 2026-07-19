-- conversation schema seed (docs/04 §3): sessions now, messages seeded for M1.

CREATE SCHEMA conversation;

CREATE TABLE conversation.sessions (
    id         TEXT PRIMARY KEY CHECK (id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    title      TEXT,
    status     TEXT NOT NULL CHECK (status IN ('active', 'archived')),
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

-- Messages are immutable blocks (docs/04 §2); M1 populates them. Seeded here
-- so the schema boundary exists from the start.
CREATE TABLE conversation.messages (
    id         TEXT PRIMARY KEY CHECK (id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    session_id TEXT NOT NULL REFERENCES conversation.sessions (id),
    role       TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'system')),
    -- Discriminated content blocks as JSON (docs/05 §2).
    content    JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX messages_session_created_idx
    ON conversation.messages (session_id, created_at);
