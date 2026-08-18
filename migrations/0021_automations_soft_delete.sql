-- Automations are retired, not erased (S5 from the M8 security audit).
--
-- `DELETE /api/v1/automations/{id}` could not succeed for any automation that
-- had ever fired. 0018 gives `automations.executions` an `ON DELETE CASCADE` to
-- its parent *and* a `BEFORE UPDATE OR DELETE` trigger that raises
-- unconditionally; Postgres fires row-level triggers on cascaded deletes, so the
-- parent delete aborted with "automation execution history is append-only" and
-- the route returned 503. Confirmed by execution, not by reading.
--
-- The tempting fix is to drop the cascade or exempt the trigger. Both are wrong
-- in the same way: they make an append-only audit-adjacent table deletable in
-- order to make a convenience endpoint work. History is the only durable answer
-- to "why did the lights come on at 6am", and a delete path through it is a
-- delete path through the evidence.
--
-- So the automation gets a `deleted_at` instead. A retired automation stops
-- being listed and stops firing — which is everything the owner asked for — and
-- its executions remain exactly as immutable as they were. Nothing in the schema
-- ever needs to remove a row from `executions`, which is the property worth
-- keeping.
ALTER TABLE automations.automations
    ADD COLUMN deleted_at TIMESTAMPTZ;

-- Partial index: every sweep and every listing filters on this, and the
-- retired rows are exactly the ones none of them ever want.
CREATE INDEX automations_live_idx
    ON automations.automations (enabled)
    WHERE deleted_at IS NULL;
