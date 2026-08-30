#!/usr/bin/env bash
# Restore a Jarvis house from a backup (F10.2, FR-30, docs/09).
#
# The half that matters. Taking a backup is easy and gives no information; the
# only thing that tells you a backup was real is putting it back and finding the
# house still works.
#
# This restores **both** stores and then checks they agree — every artifact
# manifest in the restored database must have its blob on disk. A restore that
# half-works is the dangerous outcome, because it looks like a working house
# until somebody opens the one artifact that mattered.
#
# Usage:
#   DATABASE_URL=postgres://.../jarvis_restored \
#   JARVIS__STORAGE__ARTIFACTS_ROOT=/var/lib/jarvis/artifacts \
#     infra/install/restore.sh /path/to/backups/jarvis-<timestamp>
#
# DATABASE_URL should normally point at a **fresh, empty** database — restoring
# over a live house is refused unless you pass --force, because it is not
# reversible and the mistake is expensive.

set -euo pipefail

SRC="${1:-}"
FORCE=0
[[ "${2:-}" == "--force" ]] && FORCE=1
if [[ -z "$SRC" ]]; then
	echo "usage: DATABASE_URL=... JARVIS__STORAGE__ARTIFACTS_ROOT=... $0 <backup-dir> [--force]" >&2
	exit 2
fi
: "${DATABASE_URL:?DATABASE_URL must be set (point it at a fresh database)}"

ARTIFACTS_ROOT="${JARVIS__STORAGE__ARTIFACTS_ROOT:-${ARTIFACTS_ROOT:-}}"
if [[ -z "$ARTIFACTS_ROOT" ]]; then
	echo "ABORT: set JARVIS__STORAGE__ARTIFACTS_ROOT — a database without its blobs is not a house." >&2
	exit 2
fi
[[ -f "$SRC/db.dump" ]] || { echo "ABORT: $SRC/db.dump not found" >&2; exit 2; }

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
# Same reason as backup.sh: `$DATABASE_URL` carries the Postgres password, and
# passing the whole DSN as a command-line argument publishes it in
# /proc/<pid>/cmdline, which is world-readable. A restore runs on the worst day
# an operator has; it should not also be the day the production credential is
# readable by every local account.
#
# Percent-decoded because a password containing '@' or ':' MUST be
# percent-encoded to be a valid URI, and libpq decodes it — PGPASSWORD does not.
DSN="$DATABASE_URL"
if [[ "$DATABASE_URL" =~ ^([a-zA-Z][a-zA-Z0-9+.-]*://)([^:@/]+):([^@]*)@(.*)$ ]]; then
	# Only when there is something to decode, and with literal backslashes
	# protected first: `printf %b` interprets \n, \t and friends, so a password
	# containing a backslash came out as a different password and Postgres
	# answered "password authentication failed" for a credential that was right.
	# A bare `%` not followed by two hex digits is not a valid URI password to
	# begin with (it MUST be percent-encoded to appear here at all), so leaving
	# that case to produce nonsense is the same answer libpq gives.
	raw="${BASH_REMATCH[3]}"
	if [[ "$raw" == *%* ]]; then
		esc="${raw//\\/\\\\}"
		PGPASSWORD="$(printf '%b' "${esc//%/\\x}")"
	else
		PGPASSWORD="$raw"
	fi
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

echo "== postgres client tools"
check_versions

echo "== target database"
EXISTING="$(pgsql "$DSN" -tA -c \
	"SELECT count(*) FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog','information_schema')" \
	2>/dev/null || echo 0)"
if [[ "$EXISTING" != "0" ]] && ((FORCE == 0)); then
	echo "   PROBLEM: the target database already has $EXISTING tables." >&2
	echo "   Restoring over a live house is not reversible. Create a fresh database," >&2
	echo "   or pass --force if you are certain this one should be replaced." >&2
	exit 1
fi
echo "   ok: $EXISTING existing tables"

echo "== database"
# --clean --if-exists so --force is genuinely idempotent rather than
# half-overwriting; --no-owner so a restore works under a different role.
RESTORE_FLAGS=(--dbname="$DSN" --no-owner --no-privileges)
((FORCE == 1)) && RESTORE_FLAGS+=(--clean --if-exists)
pg_run "${PGRESTORE:-pg_restore}" "${RESTORE_FLAGS[@]}" <"$SRC/db.dump"
echo "   ok: restored"

echo "== artifact blobs"
mkdir -p "$ARTIFACTS_ROOT"
if [[ -d "$SRC/blobs" ]]; then
	cp -a "$SRC/blobs/." "$ARTIFACTS_ROOT/"
fi
echo "   ok: $(find "$ARTIFACTS_ROOT" -type f 2>/dev/null | wc -l) blobs in place"

echo "== do the two stores agree?"
# The whole point of this script. Anything else is a file copy.
MISSING=0
CHECKED=0
while read -r hex; do
	[[ -z "$hex" ]] && continue
	CHECKED=$((CHECKED + 1))
	if [[ ! -f "$ARTIFACTS_ROOT/${hex:0:2}/${hex:2:2}/$hex" ]]; then
		echo "   PROBLEM: artifact blob $hex is missing" >&2
		MISSING=$((MISSING + 1))
	fi
done < <(pgsql "$DSN" -tA -c "SELECT DISTINCT sha256 FROM artifacts.manifests")

if ((MISSING > 0)); then
	echo >&2
	echo "RESTORE INCOMPLETE: $MISSING of $CHECKED artifact blobs are missing." >&2
	echo "The database is restored but $MISSING artifact(s) cannot be read. Failing" >&2
	echo "loudly rather than handing you a house that looks fine until you open one." >&2
	exit 1
fi
echo "   ok: $CHECKED artifact blobs all present"

echo
echo "restore complete. Start jarvisd against this database and run:"
echo "  infra/install/first-run.sh"
