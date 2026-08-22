#!/usr/bin/env bash
# Upgrade a Jarvis house (F10.3, docs/09 §3a).
#
# Upgrading must not be an act of faith. This script does the three things an
# operator would otherwise have to remember in the right order, and refuses to
# continue when any of them fails:
#
#   1. take a backup, and verify it — an upgrade with no way back is a gamble;
#   2. apply forward migrations;
#   3. check the daemon comes back healthy, and say what to do if it does not.
#
# ROLLBACK IS RESTORE FROM BACKUP. THERE IS NO `down` MIGRATION.
#
# Stated here rather than implied, because the implication would be a lie: all
# 21 migrations in `migrations/` are forward-only, there is not one `.down.sql`
# in the tree, and `sqlx migrate revert` therefore has nothing to revert. A
# schema change that has run cannot be un-run.
#
# That is a deliberate position, not an oversight. A `down` migration that
# silently drops the column a failed upgrade just populated destroys data the
# operator still had; restoring the backup taken in step 1 does not. The cost is
# that rollback loses everything written since the backup — which is why the
# backup is taken here, seconds before the migration, rather than nightly.
#
# Usage:
#   DATABASE_URL=postgres://... \
#   JARVIS__STORAGE__ARTIFACTS_ROOT=/var/lib/jarvis/artifacts \
#     infra/install/update.sh [--payload <unpacked-tarball>] /var/backups/jarvis
#
# Stop jarvisd first. A daemon holding the old schema while migrations run is
# the one state neither this script nor the restore path can reason about.
#
# --payload <dir> is how install.sh delegates an upgrade: this script then
# installs the new binaries, web assets and migrations itself, between step 1
# and step 2. That placement is the whole point — see the comment at the
# payload step below. Without it, this script only migrates and health-gates
# whatever binary is already on the host, which is the standalone use above.
#
# Environment:
#   JARVIS_PG_CONTAINER  passed through to backup.sh; on an installed host set
#                        it to jarvis-postgres-1 (there is no host pg_dump).
#   JARVIS_HEALTH_URL    where step 3 polls; defaults to loopback plaintext,
#                        matching the packaged jarvisd.toml.
#   JARVIS_START_CMD     how step 3 starts the daemon. Defaults to
#                        `systemctl start jarvisd` when running as root on a
#                        host that has the unit; empty means "ask the human".

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Same two shapes install.sh detects, and for the same reason: this script
# lives at install/update.sh inside the unpacked tarball (sibling of bin/,
# web/, systemd/) but at infra/install/update.sh in a source tree, one level
# deeper. Staying consistent with install.sh's own detection here — rather
# than inventing a second convention — is what lets the migrations-directory
# fallback below resolve correctly in both shapes without hardcoding either.
if [[ -d "$HERE/../bin" ]]; then
	SRC="$(cd "$HERE/.." && pwd)"          # unpacked tarball
else
	SRC="$(cd "$HERE/../.." && pwd)"       # source tree
fi

BACKUP_ROOT=""
PAYLOAD=""
# `while (( "$#" ))` reading $1 fresh each iteration, not `for arg in "$@"`,
# for the same reason install.sh's parser does: a loop over a pre-expanded "$@"
# desyncs from the real positional parameters the moment an option shifts an
# extra word for its value.
while (( "$#" )); do
	case "$1" in
		--payload=*) PAYLOAD="${1#*=}"; shift ;;
		--payload)
			PAYLOAD="${2:-}"
			# `shift 2` when --payload is the last argument is an error under
			# set -e, so shift twice and tolerate the second failing.
			shift; shift || true
			;;
		-*)
			echo "unknown option: $1" >&2
			exit 2
			;;
		*) BACKUP_ROOT="$1"; shift ;;
	esac
done

if [[ -z "$BACKUP_ROOT" ]]; then
	echo "usage: DATABASE_URL=... JARVIS__STORAGE__ARTIFACTS_ROOT=... $0 [--payload <dir>] <backup-directory>" >&2
	exit 2
fi
if [[ -n "$PAYLOAD" && ! -d "$PAYLOAD/bin" ]]; then
	echo "ABORT: --payload $PAYLOAD does not look like an unpacked release (no bin/)." >&2
	echo "Nothing has been backed up or migrated." >&2
	exit 2
fi
: "${DATABASE_URL:?DATABASE_URL must be set}"

HEALTH_URL="${JARVIS_HEALTH_URL:-http://127.0.0.1:8741/api/v1/diagnostics/health}"
# Two minutes at the default, which is the budget an upgrade gets. Overridable
# only so the gate itself is testable: a regression test for "does this script
# start the daemon at all" must not have to wait out the real timeout, and a
# test that cannot be written is a check that never runs.
HEALTH_ATTEMPTS="${JARVIS_HEALTH_ATTEMPTS:-60}"

echo "== 1/3 backup"
# Not optional, and not "best effort". The whole rollback story is this backup;
# an upgrade that proceeded without one would be an upgrade with no way back.
if ! "$HERE/backup.sh" "$BACKUP_ROOT"; then
	echo >&2
	echo "ABORT: the backup failed, so nothing has been migrated." >&2
	echo "Your house is untouched. Fix the backup before upgrading — an upgrade" >&2
	echo "without one is not recoverable." >&2
	exit 1
fi
BACKUP="$(find "$BACKUP_ROOT" -maxdepth 1 -type d -name 'jarvis-*' | sort | tail -1)"
echo "   rollback point: $BACKUP"

# --- payload (only when install.sh delegated one) ----------------------------
#
# HERE, and nowhere else. This window — after a verified backup, before any
# migration — is the only place the binaries can be replaced safely:
#
#   * earlier (install.sh used to do it before calling this script) means a
#     failed backup aborts with "your house is untouched" over a host whose
#     daemon is stopped and whose old binary is already overwritten;
#   * later means step 2 migrates with the OLD binary's embedded migration
#     stream and step 3 health-gates the OLD daemon — an upgrade that reports
#     success without having upgraded anything.
#
# Everything below this point is therefore judging the NEW binary, which is
# what the health gate is for.
if [[ -n "$PAYLOAD" ]]; then
	echo "== payload"
	install -m 0755 "$PAYLOAD/bin/jarvisd" "$PAYLOAD/bin/jarvis-agent" /usr/local/bin/
	rm -rf /var/lib/jarvis/web && cp -r "$PAYLOAD/web" /var/lib/jarvis/web
	# /var/lib/jarvis/migrations is one of the sqlx-cli fallback candidates
	# below (used when jarvisd is not on PATH). Refreshing it here means that
	# candidate matches the version being installed NOW; it is otherwise
	# written once, at the original install, and never touched again.
	rm -rf /var/lib/jarvis/migrations && cp -r "$PAYLOAD/migrations" /var/lib/jarvis/migrations
	# cp as root leaves root-owned trees under a directory the daemon writes to
	# as User=jarvis. Cheap to redo, and the alternative is an upgrade that
	# quietly loses write access to its own artifact store.
	if id jarvis >/dev/null 2>&1; then
		chown -R jarvis:jarvis /var/lib/jarvis
	fi
	echo "   ok: binaries, web assets and migrations replaced from $PAYLOAD"
fi

# Both migration paths below (jarvisd migrate, and the sqlx-cli fallback) hit
# this same abort on failure — factored into one place so the two copies
# cannot drift and silently start giving different rollback instructions.
# Writes to stderr and exits non-zero, same as the inline `cat` it replaces;
# nothing about set -euo pipefail behavior changes at the call sites.
abort_migration_failed() {
	cat >&2 <<EOF

MIGRATION FAILED. The database may be part-migrated.

Roll back by restoring the backup taken moments ago:

  JARVIS__STORAGE__ARTIFACTS_ROOT=$JARVIS__STORAGE__ARTIFACTS_ROOT \\
    $HERE/restore.sh $BACKUP --force

There is no 'down' migration to run instead — see the header of this script.
EOF
	exit 1
}

echo "== 2/3 migrations"
# Task 1 of this feature added `jarvisd migrate` so a host needs neither
# sqlx-cli nor a Rust toolchain to apply the embedded migration stream — the
# same subcommand install.sh's fresh-install path already uses. Prefer it
# here for the same reason: it is the mechanism actually tested end-to-end,
# and it can never drift from what the daemon about to run actually embeds
# (jarvis-infra's `sqlx::migrate!` macro bakes migrations/ into the binary at
# compile time — there is no `--source` for it to disagree with).
#
# Fall back to sqlx-cli only when jarvisd is not on PATH: a source checkout
# where the daemon has not been built yet, which is exactly the shape
# sqlx-cli was always meant to cover, and the one this script's own usage
# comment has documented since F10.3.
if command -v jarvisd >/dev/null 2>&1; then
	# jarvisd resolves its database URL from [database].url_secret, which the
	# shipped config points at env:JARVIS_DB_URL (infra/jarvisd.toml.example)
	# — never DATABASE_URL, and never a CLI argument (invariant 5). This
	# script's own contract is DATABASE_URL (see the usage comment above), so
	# bridge the one name into the other as an environment assignment; it
	# never becomes an argv entry either way.
	if ! JARVIS_DB_URL="$DATABASE_URL" jarvisd migrate; then
		abort_migration_failed
	fi
	# `jarvisd migrate` reports only success or failure, not a before/after
	# count (it did not exist before this feature, and adding a second
	# subcommand just to report a count is out of scope here) — printing a
	# number anyway would be invented, not observed, so this path says only
	# what actually happened.
	echo "   ok: migrations applied (jarvisd migrate; no before/after count available)"
else
	command -v sqlx >/dev/null 2>&1 || {
		echo >&2
		echo "ABORT: neither jarvisd nor sqlx-cli is on PATH — nothing here can apply migrations." >&2
		echo "Install one of them: jarvisd (this host's own binary) or sqlx-cli (cargo install sqlx-cli)." >&2
		exit 1
	}
	# Two real candidates. $SRC/migrations goes FIRST: it lives relative to THIS
	# script in either layout install.sh itself detects (the tarball root, or the
	# source tree root), so it is always the migrations shipped alongside the
	# update.sh actually being run right now - the tarball an operator is
	# currently using to upgrade.
	#
	# /var/lib/jarvis/migrations is written exactly once, at the ORIGINAL install,
	# and install.sh's upgrade branch delegates to this script and then exits
	# immediately (it never reaches its own payload step that would refresh that
	# directory) - so on an installed host that directory can be arbitrarily
	# stale relative to the tarball actually being run. Checking $SRC first means
	# a standalone `update.sh` invocation (the usage this script's own header
	# documents) on a host where jarvisd is not on PATH picks the CURRENT
	# migrations, not a stale set - silently applying old migrations and printing
	# a false "ok: N -> M migrations applied" is exactly the failure this
	# ordering prevents.
	MIGRATIONS_DIR=""
	for candidate in "$SRC/migrations" /var/lib/jarvis/migrations; do
		if [[ -d "$candidate" ]]; then
			MIGRATIONS_DIR="$candidate"
			break
		fi
	done
	if [[ -z "$MIGRATIONS_DIR" ]]; then
		echo >&2
		echo "ABORT: could not find a migrations/ directory (looked in" >&2
		echo "$SRC/migrations and /var/lib/jarvis/migrations)." >&2
		exit 1
	fi

	BEFORE="$(sqlx migrate info --source "$MIGRATIONS_DIR" 2>/dev/null | grep -c installed || true)"
	if ! sqlx migrate run --source "$MIGRATIONS_DIR"; then
		abort_migration_failed
	fi
	AFTER="$(sqlx migrate info --source "$MIGRATIONS_DIR" 2>/dev/null | grep -c installed || true)"
	echo "   ok: $BEFORE -> $AFTER migrations applied"
fi

echo "== 3/3 health"
# NOTHING USED TO START THE DAEMON HERE. This step was written for a human
# driving the script by hand, and the message below was the whole mechanism.
# install.sh's upgrade path runs it non-interactively, after
# `systemctl stop jarvisd`, and never started it again — so the documented
# upgrade (`sudo ./install/install.sh`) polled a stopped daemon for 120 s and
# then printed restore instructions over a freshly migrated database. The
# migration had worked; only the report of it had failed.
#
# So: start it ourselves when we plainly can, and keep the human-driven path
# for when we cannot — this script is documented as standalone, and a source
# checkout has no unit to start.
#
# `systemctl list-unit-files jarvisd.service` exits 0 with an empty list when
# the unit does not exist (it is a query, not a lookup), so its OUTPUT is
# grepped rather than its exit code — an absent unit must be a false condition
# here, not a script-ending error under set -e. `2>/dev/null` covers a
# systemctl that cannot reach the bus at all (containers, CI): also "no
# evidence of a unit", also the human-driven path.
START_CMD="${JARVIS_START_CMD-}"
if [[ -z "${JARVIS_START_CMD+set}" ]] \
	&& [[ "$(id -u)" -eq 0 ]] \
	&& command -v systemctl >/dev/null 2>&1 \
	&& systemctl list-unit-files jarvisd.service 2>/dev/null | grep -q '^jarvisd\.service'; then
	START_CMD="systemctl start jarvisd"
fi

if [[ -n "$START_CMD" ]]; then
	echo "   starting the daemon: $START_CMD"
	# Not fatal on its own. A failed start is diagnosed by the health gate
	# below, which already prints the journal hint and the restore command —
	# duplicating that here would give two different answers to one failure.
	if ! $START_CMD; then
		echo "   note: '$START_CMD' returned non-zero; waiting for health anyway" >&2
	fi
else
	echo "   start jarvisd now, then this script waits for it to report healthy."
fi
HEALTHY=0
for _ in $(seq 1 "$HEALTH_ATTEMPTS"); do
	if curl -sf --max-time 2 "$HEALTH_URL" >/dev/null 2>&1; then
		HEALTHY=1
		break
	fi
	sleep 2
done

if ((HEALTHY == 0)); then
	cat >&2 <<EOF

The daemon did not report healthy within $((HEALTH_ATTEMPTS * 2)) seconds ($HEALTH_URL).

The schema is migrated; the daemon is not serving. Check its journal first —
a daemon that fails to start after an upgrade usually says why, and the fault
is more often config than schema.

If you need to go back:

  JARVIS__STORAGE__ARTIFACTS_ROOT=$JARVIS__STORAGE__ARTIFACTS_ROOT \\
    $HERE/restore.sh $BACKUP --force

and run the OLD binary against it. Restoring the database without also going
back to the binary that matches it just reproduces this state.
EOF
	exit 1
fi

echo "   ok: healthy"
echo
echo "upgrade complete."
echo "Rollback point kept at: $BACKUP"
echo "Once you are satisfied, it is an ordinary backup — keep or prune it as usual."
