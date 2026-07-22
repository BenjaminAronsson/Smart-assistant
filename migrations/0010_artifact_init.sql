-- artifacts schema seed (docs/04 §3/§4, FR-08, ADR-008, invariant #6). One row
-- per artifact *version*: a durable, versioned output described by an immutable
-- manifest. The blob itself lives in the content-addressed file store keyed by
-- `sha256` (docs/04 §1) — this table holds only the manifest metadata and
-- provenance; the two are joined by the hash.
--
-- Immutability (docs/04 §4 "Manifests are immutable; a new version is a new
-- row + new CAS entry") is enforced by the DB, not just the application: a
-- manifest row is INSERT-only — never updated, deleted, or truncated. A new
-- version is a new row with the same artifact_id and version+1. The eventual
-- FR-16 "forget" flow (delete manifest + CAS blob + embeddings together) is a
-- later, coordinated operation that will relax the delete guard deliberately;
-- until it exists, deletion is forbidden so provenance cannot be rewritten.
--
-- Schema shape vs docs/04 §3: that sketch lists `artifacts, artifact_versions,
-- manifests, render_jobs`. This seed ships a single `manifests` table keyed
-- (artifact_id, version) — artifact identity and the version chain are the set
-- of rows sharing an artifact_id, and render_jobs has no writer yet (the 0009
-- precedent: no speculative schema for tables no code reads). docs/04 §3 to be
-- reconciled to the single-table design at /sync-docs.

CREATE SCHEMA artifacts;

CREATE TABLE artifacts.manifests (
    artifact_id          TEXT NOT NULL CHECK (artifact_id ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    -- Monotonic version, 1-based (domain ArtifactVersion is NonZeroU32). The
    -- PK makes a duplicate (artifact_id, version) a conflict, so versions are
    -- append-only and a concurrent double-create of the same version loses.
    version              BIGINT NOT NULL CHECK (version >= 1),
    created_by_run       TEXT NOT NULL CHECK (created_by_run ~ '^[0-9A-HJKMNP-TV-Z]{26}$'),
    -- Content address of the blob (lowercase hex sha256). Not unique: two
    -- versions (or two artifacts) with identical bytes legitimately share a blob.
    sha256               TEXT NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    -- Non-empty MIME type containing a subtype separator (defence in depth; the
    -- domain MediaType newtype enforces this too). The renderer is NOT stored:
    -- it is derived from `kind` (domain ArtifactKind::renderer_id), 1:1 in v1, so
    -- persisting it would be a snapshot that could silently drift from the code.
    media_type           TEXT NOT NULL CHECK (media_type ~ '/'),
    kind                 TEXT NOT NULL
        CHECK (kind IN ('markdown_html', 'code_text', 'image', 'chart', 'bundle')),
    sensitivity          TEXT NOT NULL CHECK (sensitivity IN ('normal', 'sensitive')),
    -- Build provenance (docs/04 §4 `build`). Never carries secrets (invariant
    -- #5) — only image references and hashes.
    build_worker_image   TEXT,
    build_lockfile_hash  TEXT CHECK (build_lockfile_hash IS NULL
                                     OR build_lockfile_hash ~ '^[0-9a-f]{64}$'),
    build_network        TEXT NOT NULL CHECK (build_network IN ('disabled', 'enabled')),
    -- Ordered provenance: a JSON array of {"kind": "message|run|web", "ref": "..."}.
    -- Stored inline (matching the docs/04 §4 manifest shape) rather than a child
    -- table because M3a only ever reads it whole with the manifest.
    sources              JSONB NOT NULL,
    -- Declared capabilities (docs/04 §4). Provenance metadata in M3; the
    -- capability bridge that enforces it for generated apps is M6.
    capabilities         TEXT[] NOT NULL,
    created_at           TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (artifact_id, version)
);

-- A manifest is immutable and append-only (docs/04 §4). The DB refuses any
-- UPDATE/DELETE/TRUNCATE, mirroring the grants and audit guards (docs/06 §5) so
-- provenance is tamper-evident even against an application bug.
CREATE FUNCTION artifacts.manifests_guard() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'TRUNCATE' THEN
        RAISE EXCEPTION 'artifacts.manifests is never truncated (immutable manifests, docs/04 §4)';
    END IF;
    IF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION 'artifacts.manifests rows are immutable; a new version is a new row (docs/04 §4)';
    END IF;
    -- DELETE
    RAISE EXCEPTION 'artifacts.manifests rows are never deleted (FR-16 forget is a later coordinated op)';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER manifests_immutable
    BEFORE UPDATE OR DELETE ON artifacts.manifests
    FOR EACH ROW EXECUTE FUNCTION artifacts.manifests_guard();

CREATE TRIGGER manifests_no_truncate
    BEFORE TRUNCATE ON artifacts.manifests
    FOR EACH STATEMENT EXECUTE FUNCTION artifacts.manifests_guard();
