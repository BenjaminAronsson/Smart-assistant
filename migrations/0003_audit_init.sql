-- audit schema seed (docs/04 §3, docs/06 §7). Append-only, hash-chained
-- (CLAUDE.md invariant 6): hash = sha256(prev_hash || canonical_json(event)),
-- computed by the application inside the same transaction as the change the
-- event describes. seq gives the chain its total order.

CREATE SCHEMA audit;

CREATE TABLE audit.audit_events (
    seq            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    occurred_at    TIMESTAMPTZ NOT NULL,
    actor          TEXT NOT NULL,
    event_type     TEXT NOT NULL,
    target         TEXT NOT NULL,
    correlation_id TEXT,
    -- Canonical JSON payload; the exact bytes fed to the chain hash.
    payload        JSONB NOT NULL,
    prev_hash      TEXT NOT NULL, -- sha256 hex of predecessor ('' for seq 1)
    hash           TEXT NOT NULL UNIQUE
);

-- Application code never updates or deletes audit rows (invariant 6); the
-- database enforces it even against bugs. Migrations marked DESTRUCTIVE with
-- a backup gate are the only sanctioned exception (docs/04 §3).
--
-- DEFERRED (docs/06 §5 "append-only permissions"): the dedicated INSERT-only
-- application role is a prod-deployment concern (docs/09 provisioning) and
-- ships with the packaging work; until then jarvisd connects as the schema
-- owner and these triggers are the enforcement layer.
CREATE FUNCTION audit.forbid_mutation() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'audit.audit_events is append-only (invariant 6)';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER audit_events_append_only
    BEFORE UPDATE OR DELETE ON audit.audit_events
    FOR EACH ROW EXECUTE FUNCTION audit.forbid_mutation();

-- Row triggers do not fire on TRUNCATE — close that path too.
CREATE TRIGGER audit_events_no_truncate
    BEFORE TRUNCATE ON audit.audit_events
    FOR EACH STATEMENT EXECUTE FUNCTION audit.forbid_mutation();
