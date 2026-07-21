-- tooling schema seed (docs/04 §3, docs/06 §4, invariant #1). Execution grants:
-- the single-use, fully-bound authorizations minted on human approval that are
-- the ONLY thing permitting an R2+ tool call. A grant is minted once, validated
-- and consumed exactly once immediately before execution, and never deleted —
-- it is a security record.
--
-- tool_invocations (the general idempotency/replay ledger for tool execution)
-- is deliberately NOT seeded here: nothing writes it until the executor lands
-- in F2.6, and migration 0006 set the precedent of not shipping speculative
-- schema for tables no code reads. For R2+ the grant's single-use consumption
-- IS the replay guard; F2.6 adds the R0/R1 invocation ledger with its writer.

CREATE SCHEMA tooling;

-- One row per minted grant. Every non-timestamp column is a binding: validation
-- re-derives the argument hash and re-checks the full binding against the row
-- (the source of truth), never trusting an in-memory ExecutionGrant. `consumed_at`
-- is the one-way single-use marker: NULL until the grant is spent, then frozen.
CREATE TABLE tooling.grants (
    grant_id                TEXT PRIMARY KEY CHECK (grant_id ~ '^[0-9a-f]{64}$'),
    user_id                 TEXT NOT NULL CHECK (user_id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    device_id               TEXT NOT NULL CHECK (device_id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    run_id                  TEXT NOT NULL CHECK (run_id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    tool_id                 TEXT NOT NULL,
    tool_version_major      BIGINT NOT NULL,
    tool_version_minor      BIGINT NOT NULL,
    tool_version_patch      BIGINT NOT NULL,
    -- sha256(canonical_form(normalized args)); the raw arguments are NEVER
    -- stored here (invariant #5 — they may carry sensitive payloads).
    normalized_args_sha256  TEXT NOT NULL CHECK (normalized_args_sha256 ~ '^[0-9a-f]{64}$'),
    target_resource         TEXT NOT NULL,
    expires_at              TIMESTAMPTZ NOT NULL,
    single_use              BOOLEAN NOT NULL,
    minted_at               TIMESTAMPTZ NOT NULL,
    -- NULL = unspent; set exactly once at validation time (one-way).
    consumed_at             TIMESTAMPTZ
);

CREATE INDEX grants_run_idx ON tooling.grants (run_id);

-- Grants are never deleted and their binding is immutable; the only permitted
-- mutation is the one-way consume (consumed_at NULL -> a timestamp). The DB
-- enforces this even against application bugs, mirroring the audit chain's
-- append-only trigger (docs/06 §5).
CREATE FUNCTION tooling.grants_guard() RETURNS trigger AS $$
BEGIN
    -- TRUNCATE is statement-level: OLD/NEW are unset, so this branch must come
    -- first (before any OLD access) to raise a clear message rather than a
    -- confusing plpgsql "record OLD is not assigned yet" error.
    IF TG_OP = 'TRUNCATE' THEN
        RAISE EXCEPTION 'tooling.grants is never truncated (invariant #1 record)';
    END IF;
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'tooling.grants rows are never deleted (invariant #1 record)';
    END IF;
    IF OLD.consumed_at IS NOT NULL THEN
        RAISE EXCEPTION 'grant % already consumed (single-use)', OLD.grant_id;
    END IF;
    IF NEW.grant_id <> OLD.grant_id
       OR NEW.user_id <> OLD.user_id
       OR NEW.device_id <> OLD.device_id
       OR NEW.run_id <> OLD.run_id
       OR NEW.tool_id <> OLD.tool_id
       OR NEW.tool_version_major <> OLD.tool_version_major
       OR NEW.tool_version_minor <> OLD.tool_version_minor
       OR NEW.tool_version_patch <> OLD.tool_version_patch
       OR NEW.normalized_args_sha256 <> OLD.normalized_args_sha256
       OR NEW.target_resource <> OLD.target_resource
       OR NEW.expires_at <> OLD.expires_at
       OR NEW.single_use <> OLD.single_use
       OR NEW.minted_at <> OLD.minted_at THEN
        RAISE EXCEPTION 'grant % binding is immutable', OLD.grant_id;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER grants_immutable_binding
    BEFORE UPDATE OR DELETE ON tooling.grants
    FOR EACH ROW EXECUTE FUNCTION tooling.grants_guard();

CREATE TRIGGER grants_no_truncate
    BEFORE TRUNCATE ON tooling.grants
    FOR EACH STATEMENT EXECUTE FUNCTION tooling.grants_guard();
