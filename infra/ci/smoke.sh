#!/usr/bin/env bash
# M0 integration smoke (docs/03 §6 Integration stage; exit evidence docs/08 §1):
# jarvisd starts → health works → pair → one persisted session round-trips →
# restart → session survives (NFR-05). Requires: postgres from
# infra/compose/dev.yml reachable, migrations applied, jarvisd built,
# web assets built (optional — health-only if absent).
set -euo pipefail

export JARVIS_DB_URL=${JARVIS_DB_URL:-postgres://jarvis:jarvis-dev-only@127.0.0.1:5432/jarvis}
BIN=${JARVISD_BIN:-target/debug/jarvisd}
BASE=http://127.0.0.1:8741
WEB_ASSETS=${WEB_ASSETS:-$PWD/web/dist/jarvis-shell/browser}
LOG=$(mktemp)

start_daemon() {
  JARVIS__SERVER__WEB_ASSETS="$WEB_ASSETS" "$BIN" >> "$LOG" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 40); do
    curl -sf "$BASE/api/v1/diagnostics/health" > /dev/null 2>&1 && return 0
    sleep 0.25
  done
  echo "jarvisd did not become healthy; log:"; cat "$LOG"; exit 1
}

stop_daemon() {
  kill -TERM "$DAEMON_PID" 2>/dev/null || true
  wait "$DAEMON_PID" 2>/dev/null || true
}
trap stop_daemon EXIT

start_daemon
echo "smoke: health OK"

CODE=$(curl -sf "$BASE/api/v1/diagnostics/health" | python3 -c "import json,sys; print(json.load(sys.stdin).get('pairingCode',''))")
[ -n "$CODE" ] || { echo "smoke: no pairing window (database not clean?)"; exit 1; }

TOKEN=$(curl -sf -X POST "$BASE/api/v1/auth/pair" -H 'content-type: application/json' \
  -d "{\"pairingCode\":\"$CODE\",\"deviceName\":\"ci-smoke\"}" \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['deviceToken'])")
echo "smoke: paired"

CREATED=$(curl -sf -X POST "$BASE/api/v1/sessions" \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -H 'idempotency-key: ci-smoke-1' -d '{"title":"ci smoke"}')
SID=$(echo "$CREATED" | python3 -c "import json,sys; print(json.load(sys.stdin)['id'])")
echo "smoke: session created ($SID)"

REPLAY=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/v1/sessions" \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -H 'idempotency-key: ci-smoke-1' -d '{"title":"ci smoke"}')
[ "$REPLAY" = "200" ] || { echo "smoke: idempotent replay returned $REPLAY, want 200"; exit 1; }

NOAUTH=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/api/v1/sessions")
[ "$NOAUTH" = "401" ] || { echo "smoke: unauthenticated list returned $NOAUTH, want 401"; exit 1; }

stop_daemon
start_daemon
FETCHED=$(curl -sf "$BASE/api/v1/sessions/$SID" -H "authorization: Bearer $TOKEN" \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['id'])")
[ "$FETCHED" = "$SID" ] || { echo "smoke: session lost across restart"; exit 1; }
echo "smoke: session survived restart (NFR-05)"
echo "smoke: PASS"
