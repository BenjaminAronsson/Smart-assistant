#!/usr/bin/env bash
# Fresh-machine install: provision what a user-run install needs, then CHECK it
# (F8.9, docs/09).
#
# Not an installer that hides what it did — a script that puts the steps in one
# place and then checks each one, because "it starts on a fresh machine" is a
# claim that should fail loudly rather than be believed.
#
# Every check here earns its place by having been green while the system was
# broken. A check that cannot fail is worse than no check: it converts an
# unknown into a false assurance. In particular:
#
#   - "is something listening on 10300" stays green while the STT container
#     crash-loops, because the port-forwarder binds the port either way. So we
#     speak Wyoming to it and require a real `info` response.
#   - `"paired":true` in the health body is a property of the DATABASE, not of
#     the caller. It stays green when your browser holds no token and every
#     authenticated route 401s. So we say what it does and does not prove, and
#     verify for real when a token is available.
#   - the model provider was never checked at all, so a workdir it cannot
#     create — the packaged default lives under /var/lib — read as a healthy
#     provider until the first run failed with `network_error`.
#
#   infra/install/first-run.sh                 # provision + check
#   infra/install/first-run.sh --check-only    # never write anything
#   infra/install/first-run.sh --with-voice    # also check STT/TTS + mic
#
# Set JARVIS_DEVICE_TOKEN to let the authenticated checks run end to end.
set -euo pipefail

WITH_VOICE=0
CHECK_ONLY=0
for arg in "$@"; do
    case "$arg" in
        --with-voice) WITH_VOICE=1 ;;
        --check-only) CHECK_ONLY=1 ;;
        *) printf 'unknown argument: %s\n' "$arg" >&2; exit 2 ;;
    esac
done

BASE_URL="${JARVIS_BASE_URL:-http://127.0.0.1:8741}"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/jarvis/jarvisd.toml"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/jarvis"
fail=0

step() { printf '\n== %s\n' "$1"; }
ok()   { printf '   ok: %s\n' "$1"; }
bad()  { printf '   PROBLEM: %s\n' "$1"; fail=1; }
note() { printf '   note: %s\n' "$1"; }

# Compose runtime. Checking for `docker` alone reported "postgres is not
# running" on every podman host, which is a false failure — as misleading as a
# false pass.
COMPOSE=""
if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
    COMPOSE="docker compose"
elif command -v podman >/dev/null 2>&1 && podman compose version >/dev/null 2>&1; then
    COMPOSE="podman compose"
elif command -v podman-compose >/dev/null 2>&1; then
    COMPOSE="podman-compose"
fi

RUNTIME=""
for candidate in podman docker; do
    command -v "$candidate" >/dev/null 2>&1 && { RUNTIME="$candidate"; break; }
done

# --- provisioning ------------------------------------------------------------
#
# jarvisd ships PRODUCTION defaults: /var/lib/jarvis/claude-work and
# /var/lib/jarvis/artifacts. Those are right for a system service and impossible
# for a jarvisd run as an ordinary user, which cannot create anything under
# /var/lib. The adapter reports that failure as
# `network_error: claude workdir unavailable` — a permission fault wearing a
# network fault's name, which is a genuinely hard thing to debug from the HUD,
# where it surfaces only as a provider badge going `degraded`.
#
# Running as root, the packaged defaults are correct and we leave them alone.
step "writable paths"
if (( CHECK_ONLY )); then
    note "--check-only: not creating directories or editing $CONFIG"
elif [[ "$(id -u)" -eq 0 ]]; then
    mkdir -p /var/lib/jarvis/claude-work /var/lib/jarvis/artifacts
    ok "root install: provisioned /var/lib/jarvis"
else
    mkdir -p "$DATA_DIR/claude-work" "$DATA_DIR/artifacts" "$(dirname "$CONFIG")"
    ok "provisioned $DATA_DIR"

    if [[ ! -f "$CONFIG" ]]; then
        printf '# Written by infra/install/first-run.sh\n' > "$CONFIG"
        ok "created $CONFIG"
    fi

    # Idempotent and additive: only ever appends a section that is absent, and
    # backs the file up first. An owner who set these deliberately keeps them.
    added=0
    if ! grep -q '^\[storage\]' "$CONFIG"; then
        [[ -f "$CONFIG.bak" ]] || cp "$CONFIG" "$CONFIG.bak"
        cat >> "$CONFIG" <<EOF

[storage]
# Packaged default is /var/lib/jarvis/artifacts, which a user-run jarvisd
# cannot create (ADR-008).
artifacts_root = "$DATA_DIR/artifacts"
EOF
        added=1
    fi
    if ! grep -q '^\[providers\.claude-cli\]' "$CONFIG"; then
        [[ -f "$CONFIG.bak" ]] || cp "$CONFIG" "$CONFIG.bak"
        cat >> "$CONFIG" <<EOF

[providers.claude-cli]
# Packaged default is /var/lib/jarvis/claude-work (ADR-004). A user-run jarvisd
# cannot create it, and the resulting failure is reported as a network error.
workdir = "$DATA_DIR/claude-work"
EOF
        added=1
    fi
    if (( added )); then
        ok "added the missing keys to $CONFIG (backup at $CONFIG.bak)"
        note "restart jarvisd to pick them up"
    else
        ok "$CONFIG already sets storage + claude-cli paths"
    fi
fi

# Whatever the config says, prove the workdir is actually usable.
step "provider workdir"
workdir="$(sed -n '/^\[providers\.claude-cli\]/,/^\[/p' "$CONFIG" 2>/dev/null \
    | sed -n 's/^ *workdir *= *"\(.*\)"/\1/p' | head -1)"
workdir="${workdir:-/var/lib/jarvis/claude-work}"
if mkdir -p "$workdir" 2>/dev/null && [[ -w "$workdir" ]]; then
    ok "$workdir is writable"
else
    bad "cannot create or write $workdir — the provider will report 'network_error: claude workdir unavailable'"
fi

if command -v claude >/dev/null 2>&1; then
    ok "claude CLI on PATH ($(command -v claude))"
else
    bad "claude CLI not found on PATH — the reasoning provider cannot spawn"
fi

step "database"
if [[ -z "$COMPOSE" ]]; then
    bad "no compose runtime found (docker compose / podman compose)"
elif $COMPOSE -f infra/compose/dev.yml ps postgres 2>/dev/null | grep -q 'Up\|running\|healthy'; then
    ok "postgres is running"
else
    bad "postgres is not running — '$COMPOSE -f infra/compose/dev.yml up -d postgres'"
fi

step "migrations"
if [[ -n "${DATABASE_URL:-}" ]] && command -v sqlx >/dev/null 2>&1; then
    sqlx migrate info 2>/dev/null | tail -3 || bad "could not read migration state"
    ok "migration state readable"
else
    note "skipped: set DATABASE_URL and install sqlx-cli to check"
fi

step "daemon health"
health="$(curl -fsS "$BASE_URL/api/v1/diagnostics/health" 2>/dev/null || true)"
if [[ -n "$health" ]]; then
    ok "jarvisd answers on $BASE_URL"
else
    bad "jarvisd is not answering on $BASE_URL"
fi

step "an owner device"
# The health flag counts rows in identity.devices. It says the daemon has an
# owner SOMEWHERE; it cannot say this browser, or this script, can talk to it.
# Worth stating, because the difference is a whole day of debugging: the shell
# renders "paired" from the same flag while every authenticated call 401s.
if grep -q '"paired":true' <<<"$health"; then
    ok "the daemon has at least one paired device"
else
    bad "no owner device yet — open the shell and pair with the bootstrap code from the journal"
fi

if [[ -n "${JARVIS_DEVICE_TOKEN:-}" ]]; then
    code="$(curl -s -o /dev/null -w '%{http_code}' \
        -H "Authorization: Bearer $JARVIS_DEVICE_TOKEN" \
        "$BASE_URL/api/v1/devices" 2>/dev/null || true)"
    if [[ "$code" == "200" ]]; then
        ok "JARVIS_DEVICE_TOKEN authenticates"
    else
        bad "JARVIS_DEVICE_TOKEN rejected (HTTP $code) — revoked, or from another database"
    fi
else
    note "set JARVIS_DEVICE_TOKEN to verify a token actually works; 'paired' above does not prove it"
fi

step "reasoning provider"
if [[ -n "${JARVIS_DEVICE_TOKEN:-}" ]]; then
    providers="$(curl -fsS -H "Authorization: Bearer $JARVIS_DEVICE_TOKEN" \
        "$BASE_URL/api/v1/providers" 2>/dev/null || true)"
    if grep -q '"state":"healthy"' <<<"$providers"; then
        ok "provider reports healthy"
    elif [[ -n "$providers" ]]; then
        bad "provider is not healthy: $providers"
    else
        bad "could not read /api/v1/providers"
    fi
    note "'healthy' is also the state before any run has been attempted"
else
    note "skipped: needs JARVIS_DEVICE_TOKEN"
fi

if (( WITH_VOICE )); then
    step "voice services"
    # A real Wyoming handshake. Connecting proves only that a port-forwarder is
    # bound — it stays bound while the container behind it crash-loops, which is
    # exactly how a broken STT reads as healthy for a day.
    for port in 10300 10200; do
        response=""
        connected=0
        if exec 3<>"/dev/tcp/127.0.0.1/$port" 2>/dev/null; then
            connected=1
            printf '{"type": "describe"}\n' >&3 2>/dev/null || true
            response="$(timeout 5 head -n 1 <&3 2>/dev/null || true)"
            exec 3<&- 2>/dev/null || true
            exec 3>&- 2>/dev/null || true
        fi
        if grep -q '"type"' <<<"$response"; then
            ok "wyoming service on $port answered describe"
        elif [[ -n "$response" ]]; then
            bad "port $port replied but not with a Wyoming frame: $response"
        elif (( connected )); then
            # The failure this check exists for: the port-forwarder accepts and
            # the container behind it is dead or restarting.
            bad "port $port accepts connections but never answers — the service behind it is down or crash-looping (try '$RUNTIME logs compose-wyoming-whisper-1')"
        else
            bad "nothing listening on $port — '$COMPOSE -f infra/compose/voice.yml up -d'"
        fi
    done

    step "whisper tokenizer"
    # The asset whose absence causes that crash-loop on a host without container
    # DNS. See infra/install/fetch-wake-assets.sh for why it is needed.
    volume_dir=""
    if [[ -n "$RUNTIME" ]]; then
        volume_dir="$($RUNTIME volume inspect compose_whisper-models \
            --format '{{.Mountpoint}}' 2>/dev/null || true)"
    fi
    if [[ -z "$volume_dir" || ! -d "$volume_dir" ]]; then
        note "no whisper model volume yet — nothing to check"
    elif find "$volume_dir" -name tokenizer.json -path '*faster-whisper*' 2>/dev/null | grep -q .; then
        ok "tokenizer.json is seeded — STT starts without network"
    else
        bad "no tokenizer.json in the whisper model — STT will try to download one at startup and crash-loop offline; run infra/install/fetch-wake-assets.sh"
    fi

    step "a microphone"
    # A node with no microphone still runs (F8.2) — this is a warning, not a
    # failure, for exactly that reason.
    if command -v arecord >/dev/null 2>&1 && arecord -l 2>/dev/null | grep -q card; then
        ok "an input device exists"
    else
        note "no input device found; a node will still run, it just cannot listen"
    fi
fi

printf '\n'
if (( fail )); then
    printf 'first-run check FAILED — see the problems above.\n'
    exit 1
fi
printf 'first-run check passed.\n'
