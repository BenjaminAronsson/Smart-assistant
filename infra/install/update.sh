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

echo "== 2/3 migrations"
BEFORE="$(sqlx migrate info --source "$HERE/../../migrations" 2>/dev/null | grep -c installed || true)"
if ! sqlx migrate run --source "$HERE/../../migrations"; then
	cat >&2 <<EOF

MIGRATION FAILED. The database may be part-migrated.

Roll back by restoring the backup taken moments ago:

  JARVIS__STORAGE__ARTIFACTS_ROOT=$JARVIS__STORAGE__ARTIFACTS_ROOT \\
    $HERE/restore.sh $BACKUP --force

There is no 'down' migration to run instead — see the header of this script.
EOF
	exit 1
fi
AFTER="$(sqlx migrate info --source "$HERE/../../migrations" 2>/dev/null | grep -c installed || true)"
echo "   ok: $BEFORE -> $AFTER migrations applied"

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
