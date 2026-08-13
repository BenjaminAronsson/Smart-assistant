-- Room attribution for timers (F8.5, FR-33, docs/02 §11e).
--
-- The bug this closes, found after M7: `timer_alert` plays on the daemon host's
-- audio device with no device notion at all, so a timer set in the kitchen
-- rings at the desk. The fire path cannot ring in the right room unless the row
-- remembers which room it was set in — and it has to be the *row*, because the
-- named acceptance is "a timer set on one node rings on it **after a restart**".
--
-- Nullable on purpose, and not backfilled. A timer set from the shell, or later
-- by an automation, was set by nobody standing anywhere; that is a real case,
-- not a missing value, and it falls back to the host player. Defaulting it to
-- some device would invent an attribution nobody made.
--
-- No foreign key to identity.devices. Revoking a device must not cascade-delete
-- the timers somebody set in that room, and a timer outliving the device it was
-- set on is exactly the situation the fallback exists for. The fire path
-- resolves the device at fire time and copes with it being gone.
ALTER TABLE timers.timers
    ADD COLUMN origin_device_id TEXT
        CHECK (origin_device_id IS NULL
               OR origin_device_id ~ '^[0-9A-HJKMNP-TV-Z]{26}$');

-- Provenance is frozen, like `created_at` and `kind` before it (0011): a timer
-- that could be re-homed after the fact could be made to ring in a room its
-- setter never chose. The 0011 trigger already refuses identity changes; extend
-- it rather than adding a second trigger, so there is one place that says what
-- is immutable about a timer.
CREATE OR REPLACE FUNCTION timers.timers_guard() RETURNS trigger AS $$
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
       OR NEW.note IS DISTINCT FROM OLD.note
       OR NEW.origin_device_id IS DISTINCT FROM OLD.origin_device_id THEN
        RAISE EXCEPTION 'timer % identity is immutable; only state, fire_at and updated_at move',
            OLD.id;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
