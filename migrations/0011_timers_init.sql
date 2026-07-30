-- timers schema seed (FR-33, docs/02 §11e, ADR-023, invariant #6). Timers,
-- alarms and one-shot reminders: the deterministic, zero-LLM utility that has
-- to keep working offline, in degraded mode, and across a restart.
--
-- Why this is persisted at all (ADR-023, NFR-05): a timer whose moment passes
-- while jarvisd is stopped must NOT be silently swallowed. The row survives, is
-- still `pending`, and the restart sweep fires it with a "missed" notice. v1 is
-- honest about the bound: it fires on restart, it does not pretend to be a
-- hardware clock.
--
-- Boundary with FR-17 automations: rows here mean "make a noise at time T".
-- Anything needing policy re-evaluation or model reasoning at fire time is an
-- automation and belongs to that module's schema, not this one — which is why
-- there is no action, condition, or tool column here and never should be.

CREATE SCHEMA timers;

-- One row per timer, for its whole life. Cancelled and dismissed rows are kept:
-- they are the counterpart of the audit chain's `timer.cancel`/`timer.dismiss`
-- entries, and "what did I have set yesterday?" is a real question.
CREATE TABLE timers.timers (
    id             TEXT PRIMARY KEY CHECK (id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    -- Human-facing label ("pasta timer"). Sanitized and capped by the domain
    -- newtype before it ever gets here; the bounds are restated as CHECKs so a
    -- future writer cannot store something the reader would refuse.
    name           TEXT NOT NULL CHECK (name <> '' AND octet_length(name) <= 64),
    kind           TEXT NOT NULL CHECK (kind IN ('countdown', 'alarm', 'reminder')),
    -- Countdowns keep their original span so the card can say "10 minutes" and a
    -- re-armed timer can describe itself; alarms and reminders have none.
    duration_secs  BIGINT CHECK (duration_secs IS NULL OR duration_secs >= 0),
    -- The spoken line of a reminder ("call Mom"). Personal content: it lives
    -- here and on the card, and is deliberately NOT copied into audit payloads.
    note           TEXT CHECK (note IS NULL OR (note <> '' AND octet_length(note) <= 512)),
    state          TEXT NOT NULL
        CHECK (state IN ('pending', 'fired', 'snoozed', 'dismissed', 'cancelled')),
    -- When it goes off. Moves exactly once per snooze and never otherwise.
    fire_at        TIMESTAMPTZ NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL,
    -- The kind decides which optional column is populated, both ways: a
    -- countdown without a duration, or an alarm carrying a note, is a bug in the
    -- writer and is refused here rather than read back as a different timer.
    CONSTRAINT countdown_has_a_duration
        CHECK ((kind = 'countdown') = (duration_secs IS NOT NULL)),
    CONSTRAINT reminder_has_a_note
        CHECK ((kind = 'reminder') = (note IS NOT NULL))
);

-- The scheduler's only query: the live set (armed, or ringing unanswered),
-- earliest first. Partial so the index stays the size of what is outstanding
-- rather than of every timer ever set — this table is read on every wakeup on an
-- 8 GB target (docs/09 §5).
CREATE INDEX timers_live_idx
    ON timers.timers (fire_at)
    WHERE state IN ('pending', 'snoozed', 'fired');

-- Terminal is terminal, enforced by the database and not only by the domain's
-- transition table (defence in depth, same stance as the grants and manifest
-- guards in 0009/0010): once a timer is dismissed or cancelled, nothing may
-- resurrect it — in particular nothing may set it back to a state that FIRES.
-- Identity and provenance columns are frozen for the same reason; only `state`,
-- `fire_at` (a snooze) and `updated_at` may ever move.
CREATE FUNCTION timers.timers_guard() RETURNS trigger AS $$
BEGIN
    IF OLD.state IN ('dismissed', 'cancelled') THEN
        RAISE EXCEPTION 'timer % is already % and cannot change (ADR-023 terminal state)',
            OLD.id, OLD.state;
    END IF;
    IF NEW.id <> OLD.id
       OR NEW.kind <> OLD.kind
       OR NEW.name <> OLD.name
       OR NEW.created_at <> OLD.created_at
       OR NEW.duration_secs IS DISTINCT FROM OLD.duration_secs
       OR NEW.note IS DISTINCT FROM OLD.note THEN
        RAISE EXCEPTION 'timer % identity is immutable; only state, fire_at and updated_at move',
            OLD.id;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER timers_terminal_is_terminal
    BEFORE UPDATE ON timers.timers
    FOR EACH ROW EXECUTE FUNCTION timers.timers_guard();
