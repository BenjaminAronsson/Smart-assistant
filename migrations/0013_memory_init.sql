-- Memory records and derived data (FR-16, docs/02 §7).
--
-- The memory module owns its rows. Source/provenance and embeddings cascade
-- from the memory id so forget cannot leave a searchable ghost behind. There
-- are deliberately no foreign keys into conversation/orchestration schemas;
-- cross-module identity is carried as an opaque source id at the application
-- port boundary.

CREATE SCHEMA memory;

CREATE TABLE memory.memories (
    id             TEXT PRIMARY KEY CHECK (id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    user_id        TEXT NOT NULL CHECK (user_id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    layer          TEXT NOT NULL CHECK (layer IN ('working', 'episodic', 'semantic', 'procedural')),
    text           TEXT NOT NULL CHECK (text <> '' AND octet_length(text) <= 2000),
    confidence     REAL NOT NULL CHECK (confidence >= 0 AND confidence <= 1),
    sensitivity    TEXT NOT NULL CHECK (sensitivity IN ('normal', 'sensitive')),
    scope_kind     TEXT NOT NULL CHECK (scope_kind IN ('user', 'session', 'project')),
    scope_value    TEXT,
    retention_kind TEXT NOT NULL CHECK (retention_kind IN ('until_forgotten', 'expires_at', 'session')),
    expires_at     TIMESTAMPTZ,
    pinned         BOOLEAN NOT NULL DEFAULT FALSE,
    created_at     TIMESTAMPTZ NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL,
    CHECK ((scope_kind = 'user' AND scope_value IS NULL)
        OR (scope_kind IN ('session', 'project') AND scope_value IS NOT NULL AND scope_value <> '')),
    CHECK ((retention_kind = 'expires_at' AND expires_at IS NOT NULL)
        OR (retention_kind <> 'expires_at' AND expires_at IS NULL))
);

CREATE INDEX memory_memories_user_updated_idx
    ON memory.memories (user_id, updated_at DESC, id DESC);

CREATE TABLE memory.memory_sources (
    memory_id   TEXT NOT NULL REFERENCES memory.memories (id) ON DELETE CASCADE,
    source_kind TEXT NOT NULL CHECK (source_kind IN ('explicit', 'message', 'run')),
    source_id   TEXT,
    -- The first slice records one immutable origin per memory. Keeping the
    -- memory id as the key also permits the explicit source's id to be NULL;
    -- a composite primary key would make every column NOT NULL in Postgres.
    PRIMARY KEY (memory_id),
    CHECK ((source_kind = 'explicit' AND source_id IS NULL)
        OR (source_kind <> 'explicit' AND source_id IS NOT NULL))
);

-- pgvector is provided by the development/prod Postgres image. The table is
-- versioned by model_id and text hash so a changed model or edited memory can
-- never silently reuse an incompatible vector. Retrieval code is added behind
-- the application port; this schema is safe to migrate before that adapter is
-- enabled.
CREATE TABLE memory.embeddings (
    memory_id   TEXT PRIMARY KEY REFERENCES memory.memories (id) ON DELETE CASCADE,
    model_id    TEXT NOT NULL,
    dimensions  INTEGER NOT NULL CHECK (dimensions > 0),
    text_sha256 TEXT NOT NULL CHECK (text_sha256 ~ '^[0-9a-f]{64}$'),
    embedding   vector(384) NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL
);

CREATE TABLE memory.retention_jobs (
    memory_id   TEXT PRIMARY KEY REFERENCES memory.memories (id) ON DELETE CASCADE,
    expires_at  TIMESTAMPTZ NOT NULL,
    processed_at TIMESTAMPTZ
);

CREATE INDEX memory_retention_jobs_due_idx
    ON memory.retention_jobs (expires_at, memory_id)
    WHERE processed_at IS NULL;
