-- identity schema seed (docs/04 §3). Users and paired devices; device tokens
-- are stored ONLY as hashes (docs/05 §6) — the value never touches the DB.

CREATE SCHEMA identity;

CREATE TABLE identity.users (
    id         TEXT PRIMARY KEY CHECK (id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    name       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE identity.devices (
    id          TEXT PRIMARY KEY CHECK (id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    user_id     TEXT NOT NULL REFERENCES identity.users (id),
    name        TEXT NOT NULL,
    -- sha256 hex of the opaque 256-bit bearer token (docs/05 §6).
    token_hash  TEXT NOT NULL UNIQUE,
    scopes      TEXT[] NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at  TIMESTAMPTZ
);
