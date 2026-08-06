-- M4: vectors are populated atomically with the memory mutation. Keep the
-- lookup index local to the owner/model so re-embedding remains bounded and
-- old model rows can be identified without scanning unrelated users.
CREATE INDEX memory_embeddings_model_idx
    ON memory.embeddings (model_id, memory_id);

CREATE TABLE memory.context_provenance (
    user_id    TEXT NOT NULL CHECK (user_id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    run_id     TEXT NOT NULL CHECK (run_id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    memory_id  TEXT NOT NULL REFERENCES memory.memories (id) ON DELETE CASCADE,
    rank       INTEGER NOT NULL CHECK (rank >= 0 AND rank <= 8),
    similarity REAL NOT NULL CHECK (similarity >= -1 AND similarity <= 1),
    used_at    TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (run_id, memory_id)
);

CREATE INDEX memory_context_provenance_user_used_idx
    ON memory.context_provenance (user_id, used_at DESC);
