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
# Exit status: 0 installed and the first-run checks passed; 1 the install
# failed; 2 bad arguments; 3 installed, but the first-run checks did not pass.
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

USAGE='usage: install.sh [--destdir DIR] [--dry-run] [--skip-preflight] [--skip-systemd]'

# Defined here rather than beside step()/ok()/die() below, because the argument
# loop must be able to abort. THIS SCRIPT'S DOCUMENTED INVOCATION IS
# `sudo ./install.sh`, so a mis-parsed argument does not produce a wrong staging
# directory — it produces a real root install:
#
#   * `--destdir` with no value (last argument, or `--destdir "$STAGE"` with
#     STAGE unset) left DESTDIR empty, and an empty DESTDIR IS the host: the
#     dry-run above it printed `would: useradd … jarvis` and
#     `would: mkdir -p /var/lib/jarvis/artifacts`.
#   * the old `*) shift ;;` arm swallowed anything unrecognised, so a typo'd
#     `--skip-systmd` silently enabled and STARTED units on the host.
#
# Both are now refusals. Exit 2 (usage), distinct from 1 (install failed).
usage_error() {
    printf '\nPROBLEM: %s\n%s\n' "$1" "$USAGE" >&2
    exit 2
}

# A `for arg in "$@"` loop pre-expands its list ONCE at loop entry; mixing it
# with `shift` (needed to pull --destdir's separate-word value) desyncs the
# loop's notion of "current argument" from the real positional parameters on
# every second `--destdir` or any argument after one. `while (( "$#" ))`
# reads $1 fresh each iteration, so shifting inside the loop can never lie
# about what argument comes next.
while (( "$#" )); do
    case "$1" in
        --destdir=*)
            DESTDIR="${1#*=}"
            [[ -n "$DESTDIR" ]] || usage_error "--destdir= was given an empty directory; an empty DESTDIR installs onto this host"
            shift
            ;;
        --destdir)
            # `${2:-}` yields the empty string when --destdir is the last
            # argument — which is not "stage nowhere", it is "install for real".
            # A value starting with '-' is the other shape of the same mistake:
            # `--destdir --dry-run` consumed the flag as a path.
            (( $# >= 2 )) || usage_error "--destdir needs a directory; it was the last argument, and an empty DESTDIR installs onto this host"
            [[ -n "$2" ]] || usage_error "--destdir was given an empty directory; an empty DESTDIR installs onto this host"
            [[ "$2" != -* ]] || usage_error "--destdir was given '$2', which looks like a flag rather than a directory"
            DESTDIR="$2"
            shift 2
            ;;
        --dry-run)        DRY_RUN=1; shift ;;
        --skip-preflight) SKIP_PREFLIGHT=1; shift ;;
        --skip-systemd)   SKIP_SYSTEMD=1; shift ;;
        -h|--help)        sed -n '2,20p' "$0"; exit 0 ;;
        *)                usage_error "unknown argument '$1'" ;;
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

    # Needed by the docker.service check right below, so this must come first.
    command -v systemctl >/dev/null 2>&1 || die "systemd is required"

    # This must be a check for the DOCKER ENGINE UNIT specifically, not "a
    # docker CLI that answers". jarvis-deps.service (F10.9) hardcodes
    # ExecStart against /usr/bin/docker and, separately, declares
    # Requires=docker.service — those are two different failure modes, and
    # checking only the CLI misses the second one. The podman-docker
    # compatibility package installs a real, executable /usr/bin/docker that
    # shims to podman and answers `docker compose version` happily: that
    # passes a CLI-only check and then jarvis-deps.service hard-fails at
    # every boot, because there is still no docker.service unit for
    # `Requires=` to resolve — the same "install reports success, daemon can
    # never start" failure this check exists to close, one layer down. So
    # check for the unit systemd will actually try to start.
    #
    # `systemctl list-unit-files docker.service` exits 0 with an empty list
    # when the unit does not exist (it is a query, not a "get this unit"
    # lookup) — grepping its output rather than trusting its exit code is
    # what makes this safe to use as an `if` condition under `set -e`: an
    # absent unit is a false condition, not a script-ending error. The
    # `2>/dev/null` covers the case (seen in CI/sandbox containers) where
    # systemctl cannot reach the bus at all — that is "no evidence of
    # docker.service" too, and must fall through to the same die(), not
    # abort the script outright.
    if systemctl list-unit-files docker.service 2>/dev/null | grep -q '^docker\.service' \
        && command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
        ok "docker.service registered, docker compose available"
    elif command -v podman >/dev/null 2>&1; then
        die "podman found, but jarvis-deps.service requires Docker Engine \
specifically — it hardcodes /usr/bin/docker and Requires=docker.service \
(F10.9), and no docker.service unit is registered with systemd (the \
podman-docker compatibility shim provides a /usr/bin/docker binary but no \
unit). A podman-only host would pass a CLI-only check and then fail at every \
boot when jarvis-deps.service starts. Install docker.io + \
docker-compose-plugin, or point jarvis-deps.service at podman first."
    else
        die "no docker.service unit registered with systemd — install docker.io + docker-compose-plugin"
    fi

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
        printf '   would: systemctl stop jarvisd; %s/update.sh --payload %s %s\n' \
            "$HERE" "$SRC" "$BACKUP_ROOT"
        exit 0
    fi
    systemctl stop jarvisd || true
    # THE PAYLOAD IS NOT INSTALLED HERE. It is handed to update.sh with
    # --payload, which installs it in the one window where that is safe: after
    # the verified backup, before the migrations.
    #
    # This script used to overwrite /usr/local/bin/jarvisd first, so that
    # update.sh's health gate would judge the new daemon. But update.sh's very
    # first step is the backup, and its abort message on a failed backup says
    # "Your house is untouched" — which by then was a lie: jarvisd was stopped
    # and the old binary was gone, leaving an operator with neither a backup
    # nor the binary matching the schema on disk. Ordering, not wording, is the
    # fix: nothing is mutated until there is something to roll back to.
    set -a; . /etc/jarvis/secrets.env; set +a
    # backup.sh runs pg_dump inside the SERVER'S OWN CONTAINER when this is
    # set, and on a stock installed host that is the only way it can run at
    # all: `apt install docker.io docker-compose-plugin libasound2t64` (the
    # README's line) installs no postgresql-client, so a host pg_dump is simply
    # not found and the backup — and therefore the whole upgrade — fails.
    # Using the container also guarantees the client/server version match that
    # backup.sh already refuses to proceed without. prod.yml sets
    # `name: jarvis` and the service is `postgres`, hence jarvis-postgres-1.
    DATABASE_URL="$JARVIS_DB_URL" \
        JARVIS_PG_CONTAINER="${JARVIS_PG_CONTAINER:-jarvis-postgres-1}" \
        JARVIS__STORAGE__ARTIFACTS_ROOT=/var/lib/jarvis/artifacts \
        "$HERE/update.sh" --payload "$SRC" "$BACKUP_ROOT"
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
# `cp -r SRC DEST` NESTS SRC inside DEST when DEST already exists as a
# directory, instead of overwriting in place — the same trap the web-assets
# copy above already avoids with `rm -rf` first. Without it, a second run of
# this script (exactly what the idempotence test does, and what a real
# re-install does) produces migrations/migrations/. No `2>/dev/null || true`
# here: jarvisd migrate embeds its own migration stream at compile time and
# does not read this directory, but update.sh's sqlx-cli fallback (F10.9 task
# 6) resolves it as a real candidate on a host with no jarvisd binary yet — a
# swallowed failure here would leave that fallback failing on a confusing
# "directory not found" instead of the loud install-time error that actually
# explains it.
run rm -rf "$(d /var/lib/jarvis/migrations)"
run cp -r "$SRC/migrations" "$(d /var/lib/jarvis/migrations)"

step "compose"
compose_src="$SRC/compose"; [[ -d "$compose_src" ]] || compose_src="$SRC/infra/compose"
run cp "$compose_src/prod.yml" "$(d /etc/jarvis/compose/prod.yml)"
run cp "$compose_src/otel-collector.yml" "$(d /etc/jarvis/compose/otel-collector.yml)"
# Same `cp -r` nesting trap as migrations above: rm -rf first so a re-run
# overwrites in place instead of producing postgres-init/postgres-init/.
run rm -rf "$(d /etc/jarvis/compose/postgres-init)"
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
    # The umask is scoped to a SUBSHELL. A bare `umask 077` here leaked into
    # everything the rest of this script created — jarvisd.toml and both unit
    # files came out 0600 root-only, and jarvisd (User=jarvis) then failed to
    # read its own config on every boot.
    #
    # It still has to be a umask and not just the chmod below: umask closes the
    # window between creat() and chmod() during which the password would be
    # world-readable on a multi-user host.
    (
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
    )
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
        # Set web_assets INSIDE the [server] table the example already
        # declares. This used to append a whole second `[server]` header, which
        # TOML forbids ("Cannot declare ('server',) twice") — and since
        # Config::load() runs before the subcommand match in jarvisd's main(),
        # the `jarvisd migrate` step below died and EVERY fresh install aborted
        # on `migrations failed — jarvisd was NOT started`.
        #
        # Rewriting the packaged (commented) line rather than inserting one
        # keeps the anchor visible in the example file, so this cannot silently
        # start writing into the wrong table after an edit up there. If the
        # anchor is gone, stop: a config with no web_assets serves no UI, and
        # discovering that from a blank page later is far worse than here.
        grep -qE '^[[:space:]]*#?[[:space:]]*web_assets[[:space:]]*=' "$config" \
            || die "the packaged jarvisd.toml.example has no web_assets line under [server] to set"
        sed -i -E 's|^[[:space:]]*#?[[:space:]]*web_assets[[:space:]]*=.*|web_assets = "/var/lib/jarvis/web"|' "$config"
    fi
    ok "wrote $config from the packaged example"
fi
# Unconditionally, including on the keep-the-existing branch above: jarvisd
# runs as User=jarvis and reads this file itself. An earlier install.sh leaked
# `umask 077` out of the secrets heredoc and left this file 0600 root-only —
# /etc/jarvis is 0755 so the path resolves, then figment's read returns EACCES,
# an unreadable config is a fatal error, and the daemon fail-fasts on every
# boot. Re-running the installer must heal a host in that state.
run chmod 0644 "$config"

# --- systemd -----------------------------------------------------------------
step "systemd units"
units_src="$SRC/systemd"; [[ -d "$units_src" ]] || units_src="$SRC/infra/systemd"
run mkdir -p "$(d /etc/systemd/system)"
run cp "$units_src/jarvis-deps.service" "$(d /etc/systemd/system/jarvis-deps.service)"
run cp "$units_src/jarvisd.service" "$(d /etc/systemd/system/jarvisd.service)"
# Same reason as the config above: these were 0600 root-only under the leaked
# umask. systemd reads them as root so it did boot, but `systemctl cat jarvisd`
# as the owner did not, and the mode is wrong for a world-readable unit file.
run chmod 0644 "$(d /etc/systemd/system/jarvis-deps.service)"
run chmod 0644 "$(d /etc/systemd/system/jarvisd.service)"
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
#
# The installer's own verification used to end in `|| true`, so it could never
# affect the exit status: install.sh printed every check as PROBLEM and then
# `done.` and exited 0. An installer whose verification cannot change its
# verdict is not verifying, it is decorating.
#
# The output is still printed in full — the individual PROBLEM lines are what an
# owner acts on — and the two outcomes stay distinguishable, because they need
# different actions:
#
#   exit 1  install FAILED     — die(); files may be half-written, re-run it
#   exit 3  installed, UNHEALTHY — files are in place, something else is wrong
#                                 (no database yet, nothing paired); fix that
#                                 and re-run first-run.sh, not the installer
VERIFY_STATUS=0
if [[ -z "$DESTDIR" ]] && (( ! DRY_RUN )) && (( ! SKIP_SYSTEMD )); then
    step "verifying"
    "$HERE/first-run.sh" --check-only || VERIFY_STATUS=$?
    printf '\nOpen http://127.0.0.1:8741/ and pair with the code from:\n'
    printf '  journalctl -u jarvisd | grep -i pairing\n'
fi

if (( VERIFY_STATUS != 0 )); then
    printf '\ninstalled, but the first-run checks did NOT pass.\n'
    printf 'The files are in place — this is "installed but unhealthy", not "install failed".\n'
    printf 'Fix the PROBLEM lines above, then re-check with:\n'
    printf '  sudo %s/first-run.sh --check-only\n' "$HERE"
    exit 3
fi

printf '\ndone.\n'
