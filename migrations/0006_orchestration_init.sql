-- orchestration schema seed (docs/04 §3, docs/02 §4). Runs and their recovery
-- checkpoints; the state machine's durable side. plan_steps + cancellations
-- (docs/04 §3) land with tools/policy in M2, when their writers exist — seeding
-- them now would be speculative schema for tables nothing reads yet.

CREATE SCHEMA orchestration;

-- One row per run. `state` is the RunState (docs/02 §4) as snake_case text,
-- matching the wire `RunStateDto`. Budget + used-so-far are persisted so a run
-- resumes with its accounting intact after a restart (NFR-05); elapsed time is
-- recomputed from `created_at`, and artifact bytes are an M3 concern.
CREATE TABLE orchestration.runs (
    id                  TEXT PRIMARY KEY CHECK (id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    session_id          TEXT NOT NULL REFERENCES conversation.sessions (id),
    state               TEXT NOT NULL,
    max_model_turns     INTEGER NOT NULL,
    max_tool_calls      INTEGER NOT NULL,
    max_duration_secs   BIGINT  NOT NULL,
    max_artifact_bytes  BIGINT  NOT NULL,
    used_model_turns    INTEGER NOT NULL DEFAULT 0,
    used_tool_calls     INTEGER NOT NULL DEFAULT 0,
    -- NULL until the run is terminal; then one of completed/failed/cancelled.
    outcome_kind        TEXT,
    outcome_detail      TEXT,
    created_at          TIMESTAMPTZ NOT NULL,
    updated_at          TIMESTAMPTZ NOT NULL
);

CREATE INDEX runs_session_created_idx
    ON orchestration.runs (session_id, created_at);

-- Append-only checkpoints: the safe boundaries a restart resumes from. Each
-- transition writes one in the same transaction as the run-row update and the
-- outbox event (docs/02 §2), so the durable state and the published event can
-- never disagree.
CREATE TABLE orchestration.checkpoints (
    seq        BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run_id     TEXT NOT NULL REFERENCES orchestration.runs (id),
    state      TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX checkpoints_run_seq_idx
    ON orchestration.checkpoints (run_id, seq);
