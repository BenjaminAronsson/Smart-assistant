#!/usr/bin/env bash
# Install Jarvis onto a Debian/Ubuntu host (F10.9, docs/09 §2).
#
#   sudo ./install.sh                 # install or upgrade
#   ./install.sh --destdir /tmp/x     # stage the layout somewhere else (tests)
#   ./install.sh --dry-run            # say what it would do, write nothing
#
# What lands where:
#   /usr/local/bin/{jarvisd,jarvis-agent}
#   /etc/jarvis/jarvisd.toml          config, NEVER overwritten once it exists
#   /etc/jarvis/secrets.env           0600 root:root — pg password + db url
#   /etc/jarvis/compose/prod.yml      dependencies
#   /var/lib/jarvis/{artifacts,claude-work,web,migrations}
#   /etc/systemd/system/jarvis-{deps,d}.service
#
# On a host that already has Jarvis, this DELEGATES to update.sh rather than
# doing its own thing: update.sh already takes a verified backup, migrates, and
# health-gates, and it is tested (crates/jarvis-infra/tests/backup_restore.rs).
# Two upgrade paths would mean one of them is the untested one.
set -euo pipefail

DESTDIR=""
DRY_RUN=0
SKIP_PREFLIGHT=0
SKIP_SYSTEMD=0
BACKUP_ROOT="${JARVIS_BACKUP_ROOT:-/var/backups/jarvis}"

# A `for arg in "$@"` loop pre-expands its list ONCE at loop entry; mixing it
# with `shift` (needed to pull --destdir's separate-word value) desyncs the
# loop's notion of "current argument" from the real positional parameters on
# every second `--destdir` or any argument after one. `while (( "$#" ))`
# reads $1 fresh each iteration, so shifting inside the loop can never lie
# about what argument comes next.
while (( "$#" )); do
    case "$1" in
        --destdir=*)      DESTDIR="${1#*=}"; shift ;;
        --destdir)
            DESTDIR="${2:-}"
            # Guard against --destdir being the last argument: a plain
            # `shift 2` there would ask bash to shift past the end of $@,
            # which is an error under `set -e`.
            shift; shift || true
            ;;
        --dry-run)        DRY_RUN=1; shift ;;
        --skip-preflight) SKIP_PREFLIGHT=1; shift ;;
        --skip-systemd)   SKIP_SYSTEMD=1; shift ;;
        -h|--help)        sed -n '2,20p' "$0"; exit 0 ;;
        *)                shift ;;
    esac
done

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Two shapes, both real: inside the unpacked tarball install.sh sits in
# install/ next to bin/, web/ and systemd/; in a source tree it sits in
# infra/install/ and the sources are one level up.
if [[ -d "$HERE/../bin" ]]; then
    SRC="$(cd "$HERE/.." && pwd)"          # unpacked tarball
    LAYOUT=tarball
else
    SRC="$(cd "$HERE/../.." && pwd)"       # source tree
    LAYOUT=source
fi

step() { printf '\n== %s\n' "$1"; }
ok()   { printf '   ok: %s\n' "$1"; }
die()  { printf '\nPROBLEM: %s\n' "$1" >&2; exit 1; }

run() {
    if (( DRY_RUN )); then printf '   would: %s\n' "$*"; else "$@"; fi
}

d() { printf '%s%s' "$DESTDIR" "$1"; }   # destdir-prefixed path

# --- preflight ---------------------------------------------------------------
#
# Fails loudly and early. Every check here names a way the install looks fine
# and is not: a missing compose plugin leaves jarvis-deps starting forever; a
# missing libasound2 leaves jarvis-agent unable to open a microphone with a
# link error nobody reads; an old glibc produces "No such file or directory"
# on a binary that plainly exists.
if (( ! SKIP_PREFLIGHT )); then
    step "preflight"

    [[ "$(uname -m)" == "x86_64" ]] || die "this artifact is x86_64 only; host is $(uname -m)"

    # This must be a check for DOCKER ENGINE specifically, not "any compose
    # runtime". jarvis-deps.service (F10.9) hardcodes ExecStart against
    # /usr/bin/docker and declares Requires=docker.service — on a podman-only
    # host neither exists, so a preflight that accepted `podman compose` here
    # would let the install report success onto a host where jarvis-deps can
    # never start at boot. If podman is all that is present, say so plainly
    # instead of passing.
    if [[ -x /usr/bin/docker ]] && command -v docker >/dev/null 2>&1 \
        && docker compose version >/dev/null 2>&1; then
        ok "docker compose available"
    elif command -v podman >/dev/null 2>&1; then
        die "podman found, but jarvis-deps.service requires Docker Engine \
specifically — it hardcodes /usr/bin/docker and Requires=docker.service \
(F10.9). A podman-only host would pass this check and then fail at every \
boot when jarvis-deps.service starts. Install docker.io + \
docker-compose-plugin, or point jarvis-deps.service at podman first."
    else
        die "no compose runtime — install docker.io + docker-compose-plugin"
    fi

    command -v systemctl >/dev/null 2>&1 || die "systemd is required"

    if ldconfig -p 2>/dev/null | grep -q libasound; then
        ok "libasound present"
    else
        die "libasound2 missing — jarvis-agent links it for capture: apt install libasound2"
    fi

    if [[ "$LAYOUT" == tarball ]]; then
        have="$(ldd --version | head -1 | grep -oE '[0-9]+\.[0-9]+$')"
        ok "glibc $have"
        # Built on ubuntu-latest (glibc 2.39). Older is a hard stop, and the
        # symptom without this check is an ENOENT on a file that exists.
        awk -v have="$have" 'BEGIN { exit !(have + 0 >= 2.35) }' \
            || die "glibc $have is older than 2.35; this artifact will not run"
    fi
fi

# --- upgrade path ------------------------------------------------------------
if [[ -z "$DESTDIR" && -f /etc/jarvis/jarvisd.toml && -x /usr/local/bin/jarvisd ]]; then
    step "existing install detected"
    ok "delegating to update.sh (backup → migrate → health gate)"
    printf '   backups: %s\n' "$BACKUP_ROOT"
    if (( DRY_RUN )); then
        printf '   would: systemctl stop jarvisd; %s/update.sh %s\n' "$HERE" "$BACKUP_ROOT"
        exit 0
    fi
    systemctl stop jarvisd || true
    # Binaries first: update.sh's health gate must judge the NEW daemon.
    install -m 0755 "$SRC/bin/jarvisd" "$SRC/bin/jarvis-agent" /usr/local/bin/
    rm -rf /var/lib/jarvis/web && cp -r "$SRC/web" /var/lib/jarvis/web
    set -a; . /etc/jarvis/secrets.env; set +a
    DATABASE_URL="$JARVIS_DB_URL" \
        JARVIS__STORAGE__ARTIFACTS_ROOT=/var/lib/jarvis/artifacts \
        "$HERE/update.sh" "$BACKUP_ROOT"
    exit $?
fi

# --- user and directories ----------------------------------------------------
step "service user and directories"
if [[ -z "$DESTDIR" ]] && ! id jarvis >/dev/null 2>&1; then
    run useradd --system --home-dir /var/lib/jarvis --shell /usr/sbin/nologin jarvis
    ok "created the jarvis system user"
fi

for dir in /var/lib/jarvis/artifacts /var/lib/jarvis/claude-work /var/lib/jarvis/tls \
           /etc/jarvis/compose; do
    run mkdir -p "$(d "$dir")"
done
ok "directories in place"

# --- payload -----------------------------------------------------------------
step "binaries, web assets, migrations"
if [[ "$LAYOUT" == tarball ]]; then
    run mkdir -p "$(d /usr/local/bin)"
    run install -m 0755 "$SRC/bin/jarvisd" "$(d /usr/local/bin/jarvisd)"
    run install -m 0755 "$SRC/bin/jarvis-agent" "$(d /usr/local/bin/jarvis-agent)"
    run rm -rf "$(d /var/lib/jarvis/web)"
    run cp -r "$SRC/web" "$(d /var/lib/jarvis/web)"
    ok "binaries and web assets installed"
else
    ok "source layout: skipping binaries (cargo build --release first)"
fi
run cp -r "$SRC/migrations" "$(d /var/lib/jarvis/migrations)" 2>/dev/null || true

step "compose"
compose_src="$SRC/compose"; [[ -d "$compose_src" ]] || compose_src="$SRC/infra/compose"
run cp "$compose_src/prod.yml" "$(d /etc/jarvis/compose/prod.yml)"
run cp "$compose_src/otel-collector.yml" "$(d /etc/jarvis/compose/otel-collector.yml)"
run cp -r "$compose_src/postgres-init" "$(d /etc/jarvis/compose/postgres-init)"
ok "compose files installed"

# --- secrets -----------------------------------------------------------------
#
# Generated once and NEVER rotated on re-run. Postgres reads POSTGRES_PASSWORD
# at initdb time only; rotating it here would leave the existing pgdata volume
# with the old password and nothing to say so — the database would simply stop
# authenticating after an upgrade.
step "secrets"
secrets="$(d /etc/jarvis/secrets.env)"
if [[ -f "$secrets" ]]; then
    ok "keeping the existing $secrets (rotating would orphan the pgdata volume)"
elif (( DRY_RUN )); then
    printf '   would: generate %s\n' "$secrets"
else
    password="$(head -c 32 /dev/urandom | base64 | tr -d '/+=' | head -c 32)"
    umask 077
    cat > "$secrets" <<EOF
# Written by install.sh. Root-only, mode 0600 (F10.9).
#
# Read by systemd (EnvironmentFile= in jarvisd.service) AS ROOT, before it
# drops to User=jarvis — the service account never needs read access here.
# Also passed to compose with --env-file.
#
# JARVIS_PG_PASSWORD is baked into the Postgres volume at initdb time. Changing
# it here does NOT change the database; it only breaks authentication.
JARVIS_PG_PASSWORD=$password
JARVIS_DB_URL=postgres://jarvis:$password@127.0.0.1:5432/jarvis
EOF
    chmod 0600 "$secrets"
    ok "generated $secrets"
fi

# --- config ------------------------------------------------------------------
step "config"
config="$(d /etc/jarvis/jarvisd.toml)"
if [[ -f "$config" ]]; then
    ok "keeping the existing $config"
else
    example="$SRC/jarvisd.toml.example"; [[ -f "$example" ]] || example="$SRC/infra/jarvisd.toml.example"
    run cp "$example" "$config"
    if (( ! DRY_RUN )); then
        printf '\n[server]\nweb_assets = "/var/lib/jarvis/web"\n' >> "$config"
    fi
    ok "wrote $config from the packaged example"
fi

# --- systemd -----------------------------------------------------------------
step "systemd units"
units_src="$SRC/systemd"; [[ -d "$units_src" ]] || units_src="$SRC/infra/systemd"
run mkdir -p "$(d /etc/systemd/system)"
run cp "$units_src/jarvis-deps.service" "$(d /etc/systemd/system/jarvis-deps.service)"
run cp "$units_src/jarvisd.service" "$(d /etc/systemd/system/jarvisd.service)"
ok "units installed (jarvis-agent.service is a USER unit — see the README)"

if (( SKIP_SYSTEMD )) || [[ -n "$DESTDIR" ]]; then
    ok "not touching systemd (--skip-systemd or --destdir)"
else
    run chown -R jarvis:jarvis /var/lib/jarvis
    run systemctl daemon-reload
    run systemctl enable --now jarvis-deps

    step "migrations"
    if (( ! DRY_RUN )); then
        set -a; . /etc/jarvis/secrets.env; set +a
        JARVIS_CONFIG=/etc/jarvis/jarvisd.toml /usr/local/bin/jarvisd migrate \
            || die "migrations failed — jarvisd was NOT started"
        ok "migrations applied"
    fi

    run systemctl enable --now jarvisd
fi

# --- verify ------------------------------------------------------------------
if [[ -z "$DESTDIR" ]] && (( ! DRY_RUN )) && (( ! SKIP_SYSTEMD )); then
    step "verifying"
    "$HERE/first-run.sh" --check-only || true
    printf '\nOpen http://127.0.0.1:8741/ and pair with the code from:\n'
    printf '  journalctl -u jarvisd | grep -i pairing\n'
fi

printf '\ndone.\n'
