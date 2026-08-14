-- automations schema seed (FR-17, docs/02 §11, docs/04 §3, F8.6).
--
-- The requirement docs/05 §1 has advertised routes for since M0 and that has
-- been parked twice.
--
-- The one idea this table is shaped around: **an automation is a stored
-- intention, not a stored authorization.** There is deliberately NO scopes
-- column, no cached policy decision, and no `approved` flag. What is stored is
-- who asked (`created_by_device_id`); what they are *allowed* is resolved at
-- fire time, every time, from that device's authority as it stands then.
--
-- Cache the decision instead and revoking a device leaves behind a row that
-- still turns on the heating at 6am with its authority, forever — a durable
-- privilege escalation with a friendly name.
--
-- Boundary with timers (ADR-023, migration 0011), from the other side: a timer
-- means "make a noise at T" and needs no policy at all. Anything needing policy
-- re-evaluated or a model consulted at fire time is an automation and lives
-- here — which is why this table HAS an action and 0011 deliberately does not.

CREATE SCHEMA automations;

CREATE TABLE automations.automations (
    id             TEXT PRIMARY KEY CHECK (id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    -- Human label. Sanitized and capped by the domain newtype before it gets
    -- here; the bounds are restated so a future writer cannot store something
    -- the reader would refuse.
    name           TEXT NOT NULL CHECK (name <> '' AND octet_length(name) <= 64),
    -- Closed trigger vocabulary. An open-ended predicate — anything a model
    -- could author — would be a code path from model output to a tool call,
    -- which invariant #1 forbids.
    trigger_kind   TEXT NOT NULL CHECK (trigger_kind IN ('daily_at', 'ha_state')),
    -- `daily_at`: minutes since midnight. Not an instant: "07:00" means seven
    -- tomorrow as well as today, and a stored timestamp fires once and never
    -- again.
    trigger_minute INTEGER CHECK (trigger_minute IS NULL
                                  OR (trigger_minute >= 0 AND trigger_minute < 1440)),
    -- `ha_state`: the entity and the state it must enter.
    trigger_entity TEXT CHECK (trigger_entity IS NULL OR octet_length(trigger_entity) <= 255),
    trigger_state  TEXT CHECK (trigger_state IS NULL OR octet_length(trigger_state) <= 64),
    -- What it proposes. A proposal in exactly the sense a model's is: it goes
    -- through policy::evaluate like anything else.
    tool_id        TEXT NOT NULL CHECK (tool_id <> '' AND octet_length(tool_id) <= 128),
    arguments_json TEXT NOT NULL,
    enabled        BOOLEAN NOT NULL DEFAULT TRUE,
    -- WHOSE authority is consulted at fire time — never what that authority is.
    -- No foreign key to identity.devices, deliberately: revoking a device must
    -- not delete the automations somebody created, it must make them *fail
    -- closed and say so*. The row outliving the device is the case the fire-time
    -- lookup exists to handle.
    created_by_device_id TEXT NOT NULL
        CHECK (created_by_device_id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    created_at     TIMESTAMPTZ NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL,
    -- Rate limit state. A flapping presence sensor is the ordinary case.
    last_fired_at  TIMESTAMPTZ,
    -- The trigger columns each kind needs, enforced both ways so a row cannot
    -- read back as a different trigger than it was written as.
    CONSTRAINT daily_has_a_minute
        CHECK ((trigger_kind = 'daily_at') = (trigger_minute IS NOT NULL)),
    CONSTRAINT ha_state_has_an_entity_and_state
        CHECK ((trigger_kind = 'ha_state')
               = (trigger_entity IS NOT NULL AND trigger_state IS NOT NULL))
);

-- The scheduler's query: everything enabled, cheapest first. Partial so the
-- index is the size of what is live rather than of everything ever created —
-- this is read on every sweep on an 8 GB target (docs/09 §5).
CREATE INDEX automations_enabled_idx
    ON automations.automations (trigger_kind)
    WHERE enabled;

-- Execution history (FR-17). Append-only by construction: there is no update
-- path and nothing here is ever rewritten.
--
-- A DENIAL is the most important row in this table. "The automation ran and
-- nothing happened" and "the automation was refused" look identical from the
-- sofa, and only this distinguishes them.
CREATE TABLE automations.executions (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    automation_id  TEXT NOT NULL
        REFERENCES automations.automations (id) ON DELETE CASCADE,
    occurred_at    TIMESTAMPTZ NOT NULL,
    outcome        TEXT NOT NULL
        CHECK (outcome IN ('executed', 'needs_approval', 'denied', 'failed')),
    -- Why it was refused, or how it failed. Closed-vocabulary policy reasons and
    -- adapter-neutral failure text — never a raw provider string (docs/06 §5).
    detail         TEXT CHECK (detail IS NULL OR octet_length(detail) <= 512)
);

CREATE INDEX automations_executions_history_idx
    ON automations.executions (automation_id, occurred_at DESC);

-- History is append-only (invariant #6's spirit): an execution record that
-- could be edited is not a record.
CREATE FUNCTION automations.executions_are_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'automation execution history is append-only';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER automations_executions_immutable
    BEFORE UPDATE OR DELETE ON automations.executions
    FOR EACH ROW EXECUTE FUNCTION automations.executions_are_append_only();
