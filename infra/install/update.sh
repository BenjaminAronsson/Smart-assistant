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
#     infra/install/update.sh /var/backups/jarvis
#
# Stop jarvisd first. A daemon holding the old schema while migrations run is
# the one state neither this script nor the restore path can reason about.

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

BACKUP_ROOT="${1:-}"
if [[ -z "$BACKUP_ROOT" ]]; then
	echo "usage: DATABASE_URL=... JARVIS__STORAGE__ARTIFACTS_ROOT=... $0 <backup-directory>" >&2
	exit 2
fi
: "${DATABASE_URL:?DATABASE_URL must be set}"

HEALTH_URL="${JARVIS_HEALTH_URL:-http://127.0.0.1:8741/api/v1/diagnostics/health}"

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
echo "   start jarvisd now, then this script waits for it to report healthy."
HEALTHY=0
for _ in $(seq 1 60); do
	if curl -sf --max-time 2 "$HEALTH_URL" >/dev/null 2>&1; then
		HEALTHY=1
		break
	fi
	sleep 2
done

if ((HEALTHY == 0)); then
	cat >&2 <<EOF

The daemon did not report healthy within two minutes.

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
