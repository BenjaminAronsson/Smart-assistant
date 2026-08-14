#!/usr/bin/env bash
# Fresh-machine install check (F8.9, docs/09).
#
# Not an installer that hides what it did — a script that puts the steps in one
# place and then CHECKS each one, because "it starts on a fresh machine" is a
# claim that should fail loudly rather than be believed.
#
#   infra/install/first-run.sh            # check an existing install
#   infra/install/first-run.sh --with-voice
set -euo pipefail

WITH_VOICE=0
[[ "${1:-}" == "--with-voice" ]] && WITH_VOICE=1

BASE_URL="${JARVIS_BASE_URL:-http://127.0.0.1:8741}"
fail=0

step() { printf '\n== %s\n' "$1"; }
ok()   { printf '   ok: %s\n' "$1"; }
bad()  { printf '   PROBLEM: %s\n' "$1"; fail=1; }

step "database"
if docker compose -f infra/compose/dev.yml ps postgres 2>/dev/null | grep -q 'Up\|running'; then
    ok "postgres is running"
else
    bad "postgres is not running — 'docker compose -f infra/compose/dev.yml up -d postgres'"
fi

step "migrations"
if [[ -n "${DATABASE_URL:-}" ]] && command -v sqlx >/dev/null 2>&1; then
    sqlx migrate info 2>/dev/null | tail -3 || bad "could not read migration state"
    ok "migration state readable"
else
    printf '   skipped: set DATABASE_URL and install sqlx-cli to check\n'
fi

step "daemon health"
if curl -fsS "$BASE_URL/api/v1/diagnostics/health" >/dev/null 2>&1; then
    ok "jarvisd answers on $BASE_URL"
else
    bad "jarvisd is not answering on $BASE_URL"
fi

step "a paired device"
# The health endpoint is the only unauthenticated surface (docs/05 §6.2), so
# this is the honest limit of what a script can check without a token: whether
# the daemon believes it has an owner yet.
if curl -fsS "$BASE_URL/api/v1/diagnostics/health" 2>/dev/null | grep -q '"paired":true'; then
    ok "an owner device is paired"
else
    bad "no owner device yet — open the shell and pair, or POST /api/v1/auth/pair with the bootstrap code from the journal"
fi

if (( WITH_VOICE )); then
    step "voice services"
    for port in 10300 10200; do
        if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
            ok "wyoming service on $port"
        else
            bad "nothing listening on $port — 'docker compose -f infra/compose/voice.yml up -d'"
        fi
    done

    step "a microphone"
    # A node with no microphone still runs (F8.2) — this is a warning, not a
    # failure, for exactly that reason.
    if command -v arecord >/dev/null 2>&1 && arecord -l 2>/dev/null | grep -q card; then
        ok "an input device exists"
    else
        printf '   note: no input device found; a node will still run, it just cannot listen\n'
    fi
fi

printf '\n'
if (( fail )); then
    printf 'first-run check FAILED — see the problems above.\n'
    exit 1
fi
printf 'first-run check passed.\n'
