-- lists schema seed (FR-34, docs/02 §11e, ADR-024, invariant #6). Named lists
-- (shopping, todo, …) and quick notes: the other deterministic, zero-LLM daily
-- utility, alongside timers (0011).
--
-- Why this is a schema of its own and not an artifact (ADR-024): "artifacts are
-- too heavyweight for a grocery line". These are **plain rows, exportable** — a
-- list is two tables and a `SELECT`, and stays readable by any tool the owner
-- points at their own database. When a list *does* grow into a document it is
-- promoted to a versioned artifact (FR-08) and `promoted_artifact_id` below
-- records which one, so the second promotion appends a version rather than
-- minting a rival document for the same list.
--
-- Boundary with the artifact schema: this module owns nothing in `artifact.*`
-- and there is no foreign key across that boundary — cross-module reads go
-- through the owning module's port (skill `sqlx-data` §2). The artifact id is
-- carried here as an opaque ULID reference, exactly as `ArtifactSource::Run`
-- carries a run id.

CREATE SCHEMA lists;

-- One row per named list, for its whole life.
CREATE TABLE lists.lists (
    id                    TEXT PRIMARY KEY CHECK (id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    -- The display name as the owner said it ("Shopping"). Untrusted text:
    -- sanitized and capped by the domain newtype before it ever gets here; the
    -- bounds are restated as CHECKs so a future writer cannot store something
    -- the reader would refuse.
    name                  TEXT NOT NULL CHECK (name <> '' AND octet_length(name) <= 120),
    -- The normalized lookup key ("shopping"), which is what uniqueness is
    -- enforced on. Without this, "Shopping", "shopping list" and "  SHOPPING  "
    -- would each create a rival list and the owner's milk would land on
    -- whichever one the grammar happened to key that day.
    name_key              TEXT NOT NULL UNIQUE
                              CHECK (name_key <> '' AND octet_length(name_key) <= 120),
    -- Set on the FIRST promotion and never changed (see the guard trigger).
    -- NULL means "this list has never been a document".
    promoted_artifact_id  TEXT CHECK (promoted_artifact_id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    created_at            TIMESTAMPTZ NOT NULL,
    updated_at            TIMESTAMPTZ NOT NULL
);

-- One row per line on a list. Removing a line is a real delete (a struck-out
-- grocery item is not history worth keeping) — the audit chain is what records
-- that it happened, which is why every write co-transacts its audit row.
CREATE TABLE lists.items (
    id         TEXT PRIMARY KEY CHECK (id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    list_id    TEXT NOT NULL REFERENCES lists.lists (id) ON DELETE CASCADE,
    -- Untrusted display text, sanitized and capped by the domain newtype.
    text       TEXT NOT NULL CHECK (text <> '' AND octet_length(text) <= 512),
    checked    BOOLEAN NOT NULL DEFAULT FALSE,
    -- Insertion order. Explicit rather than relying on ULID lexicography, so
    -- "the card and the promoted document read the way the list was built" is a
    -- property of the schema and not of how ids happen to be minted.
    added_at   TIMESTAMPTZ NOT NULL
);

-- The only item query: one list's lines, in the order they were added.
CREATE INDEX lists_items_by_list_idx ON lists.items (list_id, added_at, id);

-- A list is bounded (domain `MAX_ITEMS_PER_LIST`). Restated here as defence in
-- depth, the same stance as the 0009/0010/0011 guards: a runaway grammar or a
-- stuck client must not be able to turn a grocery list into an unbounded table,
-- even if it finds a path that skips the aggregate.
--
-- The parent row is locked FIRST, and that is not incidental. Counting rows in a
-- BEFORE INSERT trigger under READ COMMITTED is a read-then-write: two inserts
-- into the same list can both see 499 and both pass, and the "bound" quietly
-- stops being one. Taking `FOR UPDATE` on `lists.lists` serializes concurrent
-- inserts *per list*, so the count is taken against a state no other inserter can
-- be changing. Contention is per list and only between simultaneous appends to
-- the same list, which for a single-owner device is the rarest case there is —
-- a bound that holds only when nobody is racing is not a bound worth writing.
CREATE FUNCTION lists.items_bound() RETURNS trigger AS $$
BEGIN
    PERFORM 1 FROM lists.lists WHERE id = NEW.list_id FOR UPDATE;
    IF (SELECT count(*) FROM lists.items WHERE list_id = NEW.list_id) >= 500 THEN
        RAISE EXCEPTION 'list % already holds the maximum of 500 items (ADR-024)', NEW.list_id;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER lists_items_bounded
    BEFORE INSERT ON lists.items
    FOR EACH ROW EXECUTE FUNCTION lists.items_bound();

-- A promoted list keeps ONE document identity. Once `promoted_artifact_id` is
-- set it may never be cleared or repointed: re-promoting appends a *version* to
-- that artifact (FR-08, docs/04 §4), and a writer that swapped the pointer would
-- silently orphan the version chain the owner has been reading. Identity columns
-- are frozen for the same reason; only `name`, `name_key`, the promotion pointer
-- (once, NULL → set) and `updated_at` may ever move.
CREATE FUNCTION lists.lists_guard() RETURNS trigger AS $$
BEGIN
    IF OLD.promoted_artifact_id IS NOT NULL
       AND NEW.promoted_artifact_id IS DISTINCT FROM OLD.promoted_artifact_id THEN
        RAISE EXCEPTION 'list % is already promoted to artifact %; a later promotion adds a version',
            OLD.id, OLD.promoted_artifact_id;
    END IF;
    IF NEW.id <> OLD.id OR NEW.created_at <> OLD.created_at THEN
        RAISE EXCEPTION 'list % identity is immutable', OLD.id;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER lists_identity_is_immutable
    BEFORE UPDATE ON lists.lists
    FOR EACH ROW EXECUTE FUNCTION lists.lists_guard();

-- An item never changes list and never rewrites its own text: a check-off moves
-- `checked` and nothing else. Editing a line is remove + add, which leaves two
-- honest audit entries instead of one silent overwrite.
CREATE FUNCTION lists.items_guard() RETURNS trigger AS $$
BEGIN
    IF NEW.id <> OLD.id
       OR NEW.list_id <> OLD.list_id
       OR NEW.text <> OLD.text
       OR NEW.added_at <> OLD.added_at THEN
        RAISE EXCEPTION 'list item % is immutable; only checked may move', OLD.id;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER lists_items_immutable
    BEFORE UPDATE ON lists.items
    FOR EACH ROW EXECUTE FUNCTION lists.items_guard();
