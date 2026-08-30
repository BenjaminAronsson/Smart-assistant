#!/usr/bin/env bash
# Back up a Jarvis house (F10.2, FR-30, docs/09).
#
# A house is **two** stores and neither is sufficient alone:
#
#   * Postgres — sessions, runs, devices, timers, automations, memories, the
#     append-only audit trail, and artifact *manifests*;
#   * the artifact CAS — the blob *bytes* those manifests point at, on the
#     filesystem under `[storage] artifacts_root`.
#
# A backup of one without the other restores a database full of dangling
# references: every artifact listed, none of them readable.
#
# ORDER MATTERS, AND IT IS THE OPPOSITE OF THE OBVIOUS ONE.
#
# The database is dumped *first*, then the blobs. Blobs are content-addressed
# and, once committed, never deleted (`artifact_cas.rs` writes to a temp file
# and renames; nothing removes a committed blob). So:
#
#   * DB at t0, blobs at t1>t0 — every manifest in the dump references a blob
#     that existed at t0 and therefore still exists at t1. Extra blobs written
#     in between are harmless: they are bytes nothing points at yet.
#   * blobs at t0, DB at t1 — a manifest written at t0.5 lands in the dump
#     pointing at a blob the copy never saw. **Dangling.**
#
# Usage:
#   DATABASE_URL=postgres://... infra/install/backup.sh /path/to/backups
#
# Produces /path/to/backups/jarvis-<timestamp>/ containing db.dump, blobs/ and
# MANIFEST.

set -euo pipefail

DEST_ROOT="${1:-}"
if [[ -z "$DEST_ROOT" ]]; then
	echo "usage: DATABASE_URL=postgres://... $0 <destination-directory>" >&2
	exit 2
fi
: "${DATABASE_URL:?DATABASE_URL must be set}"

ARTIFACTS_ROOT="${JARVIS__STORAGE__ARTIFACTS_ROOT:-${ARTIFACTS_ROOT:-}}"
if [[ -z "$ARTIFACTS_ROOT" ]]; then
	echo "ABORT: set JARVIS__STORAGE__ARTIFACTS_ROOT (or ARTIFACTS_ROOT) to the artifact store." >&2
	echo "  Backing up the database alone would produce a restore with unreadable artifacts," >&2
	echo "  which is worse than no backup because it looks like one." >&2
	exit 2
fi

# --- postgres client tools ------------------------------------------------
#
# A dump is only restorable by a server at least as new as the client that
# wrote it. This is not pedantry: a pg_dump 18 dump emits `SET
# transaction_timeout`, and a 16 server refuses it — so an operator with newer
# client tools than their server takes backups for months and discovers on the
# day it matters that none of them restore. Backups that only *look* like
# backups are worse than none.
#
# So the tools are resolved, not assumed:
#
#   JARVIS_PG_CONTAINER=<name>  run pg_dump/pg_restore/psql inside that
#                               container, which is the surest way to match the
#                               server. The archive still lands on the host, by
#                               streaming over stdin/stdout.
#   PGDUMP= PGRESTORE= PSQL=    explicit paths, if you keep matching binaries.
#   (neither)                   the ones on PATH, with a version check that
#                               refuses rather than producing a useless dump.
PG_CONTAINER="${JARVIS_PG_CONTAINER:-}"

# --- keeping the password out of argv (invariant 5) ------------------------
#
# `$DATABASE_URL` carries the Postgres password, and every one of these tools
# used to receive the whole DSN as a COMMAND-LINE ARGUMENT. /proc/<pid>/cmdline
# is world-readable, so the credential was visible to any local account for the
# life of the dump. That was survivable while this ran by hand against a dev
# database; F10.9 made install.sh feed it the real generated production
# credential on every upgrade, and the README documents a nightly timer doing
# the same.
#
# So the password is split out and passed in the ENVIRONMENT via PGPASSWORD,
# which libpq reads and which does not appear in argv. The rest of the DSN
# (user, host, port, database) is not secret and stays where it was.
#
# Percent-decoded because a password containing '@' or ':' MUST be
# percent-encoded to be a valid URI, and libpq decodes it — PGPASSWORD does not.
DSN="$DATABASE_URL"
if [[ "$DATABASE_URL" =~ ^([a-zA-Z][a-zA-Z0-9+.-]*://)([^:@/]+):([^@]*)@(.*)$ ]]; then
	PGPASSWORD="$(printf '%b' "${BASH_REMATCH[3]//%/\\x}")"
	export PGPASSWORD
	DSN="${BASH_REMATCH[1]}${BASH_REMATCH[2]}@${BASH_REMATCH[4]}"
fi

pg_run() { # pg_run <tool> [args...]
	local tool="$1"; shift
	if [[ -n "$PG_CONTAINER" ]]; then
		# `-e PGPASSWORD` with NO value: docker/podman copy it from this
		# process's environment. Writing `-e "PGPASSWORD=$PGPASSWORD"` would
		# put the password straight back into argv — docker's this time.
		if [[ -n "${PGPASSWORD:-}" ]]; then
			docker exec -i -e PGPASSWORD "$PG_CONTAINER" "$tool" "$@"
		else
			docker exec -i "$PG_CONTAINER" "$tool" "$@"
		fi
	else
		"$tool" "$@"
	fi
}

pgsql() { pg_run "${PSQL:-psql}" "$@"; }

check_versions() {
	local client server
	client="$(pg_run "${PGDUMP:-pg_dump}" --version | grep -oE '[0-9]+' | head -1)"
	server="$(pgsql "$DSN" -tAc 'SHOW server_version' | grep -oE '^[0-9]+')"
	if [[ -z "$client" || -z "$server" ]]; then
		echo "   note: could not determine client/server versions; continuing" >&2
		return 0
	fi
	echo "   client tools: $client, server: $server"
	if ((client > server)); then
		cat >&2 <<EOF

ABORT: your pg_dump is version $client but the server is $server.

A dump written by a newer client can fail to restore into an older server — and
you would not find out until the day you needed it. Refusing now rather than
handing you an archive that looks fine.

Fix it with either:
  JARVIS_PG_CONTAINER=jarvis-dev-postgres-1 $0 ...   # use the server's own tools
  PGDUMP=/usr/lib/postgresql/$server/bin/pg_dump ... # or matching binaries
EOF
		exit 2
	fi
}

# Before creating anything: an abort must not leave a directory that looks
# like a backup.
echo "== postgres client tools"
check_versions

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
DEST="$DEST_ROOT/jarvis-$STAMP"
mkdir -p "$DEST"

echo "== database"
# Custom format: restorable with pg_restore into a differently-named database,
# which is what makes "restore into a clean database and check" possible.
# Streamed to stdout so the archive lands on the host even when the tools run
# inside the server's container.
pg_run "${PGDUMP:-pg_dump}" --format=custom --no-owner --no-privileges "$DSN" >"$DEST/db.dump"
echo "   ok: $(du -h "$DEST/db.dump" | cut -f1)"

echo "== artifact blobs"
mkdir -p "$DEST/blobs"
if [[ -d "$ARTIFACTS_ROOT" ]]; then
	# -a preserves the two-level fan-out; the store is addressed by path.
	cp -a "$ARTIFACTS_ROOT/." "$DEST/blobs/"
	BLOB_COUNT="$(find "$DEST/blobs" -type f | wc -l)"
else
	BLOB_COUNT=0
	echo "   note: $ARTIFACTS_ROOT does not exist yet — no artifacts to back up"
fi
echo "   ok: $BLOB_COUNT blobs"

echo "== checking the backup describes a whole house"
# Verified at backup time as well as restore time, because a backup that is
# already inconsistent should be found now, while the source is still there.
MISSING=0
while read -r hex; do
	[[ -z "$hex" ]] && continue
	if [[ ! -f "$DEST/blobs/${hex:0:2}/${hex:2:2}/$hex" ]]; then
		echo "   PROBLEM: manifest references blob $hex, which is not in the backup" >&2
		MISSING=$((MISSING + 1))
	fi
done < <(pgsql "$DSN" -tA -c "SELECT DISTINCT sha256 FROM artifacts.manifests" 2>/dev/null || true)

cat >"$DEST/MANIFEST" <<EOF
jarvis-backup 1
taken_at=$STAMP
database_dump=db.dump
blob_count=$BLOB_COUNT
missing_blobs=$MISSING
artifacts_root=$ARTIFACTS_ROOT
EOF

if ((MISSING > 0)); then
	echo >&2
	echo "BACKUP INCOMPLETE: $MISSING manifest(s) point at blobs that are not present." >&2
	echo "The dump is kept so you can inspect it, and MANIFEST records the count —" >&2
	echo "but restoring it would give you artifacts that cannot be read." >&2
	exit 1
fi

echo
echo "backup complete: $DEST"
echo "restore it with: infra/install/restore.sh $DEST"
