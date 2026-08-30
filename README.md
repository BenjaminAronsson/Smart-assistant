# Jarvis

A personal, local-first assistant. It takes text or voice, understands what is on
screen and in the house, plans bounded work, asks before anything consequential,
runs typed tools, shows live results on one or more displays, and remembers only
what policy permits.

It runs on your hardware. Nothing the model says — and nothing any web page, tool
result or generated app says — grants authority; authority comes only from an
authenticated identity, policy rules, and exact expiring execution grants.

## Install

One x86_64 Debian or Ubuntu machine. It needs **Docker Engine specifically** (a
running `docker.service` unit — podman's `docker`-compatible shim registers no
such unit and `jarvis-deps.service` will not start), ALSA, and systemd. **No
Rust, no Node, no source tree.**

```bash
sudo apt install docker.io docker-compose-plugin libasound2t64   # libasound2 on older releases

VERSION=0.1.0
BASE=https://github.com/BenjaminAronsson/Smart-assistant/releases/download/v$VERSION
curl -LO $BASE/jarvis-$VERSION-x86_64-linux-gnu.tar.zst

# --no-same-owner --no-same-permissions: this archive has not been checked yet,
# and unpacking is the first thing that touches your disk.
tar --zstd --no-same-owner --no-same-permissions -xf jarvis-$VERSION-x86_64-linux-gnu.tar.zst
cd jarvis-$VERSION

# Verify BEFORE installing — install.sh runs as root. WITHOUT --signers this
# proves the release is internally consistent, NOT who built it: the public key
# it checks against travels inside the release. Read the next paragraph before
# you treat a green line here as trust.
./install/verify-release.sh .

sudo ./install/install.sh
```

`verify-release.sh` checks that every file matches the signed manifest and that
the advisory scan behind the release is less than 30 days old — a signature
proves these are the bytes that were built, not that anyone has looked at the
world since. It will also tell you, plainly, that by default it checks
**integrity and not authenticity**: the public key travels with the release, so
agreement between the parts is all that proves. Checking against a key you
already trust is your job (`./install/verify-release.sh . --signers
<allowed_signers>`), and `docs/06-security.md` §9 explains how.

`install.sh` re-runs that check itself before it copies anything, and refuses to
install a release that does not verify — the ordering of two lines in a README is
not a security control. It also prints a NOTE when the verifier it ran came from
inside the release being checked, which is the case in the flow above: for a
release you did not cut yourself, run `verify-release.sh` from a source checkout
against the unpacked directory, and pass `--signers`.

**Releases are cut locally, not by CI.** CI builds and installs the artifact on a
clean host every run, but signs it with a throwaway key, and does not publish it.
A signature that proves nothing about who produced it is worse than no signature,
because the check still comes out green.

That creates the `jarvis` service user, installs to `/usr/local/bin` and
`/var/lib/jarvis`, writes `/etc/jarvis/jarvisd.toml`, starts Postgres and the
voice services as containers, applies migrations, and enables two services.
It finishes by checking its own work and telling you what is wrong. Run it
again on a host that already has Jarvis and it upgrades instead — see
"Back up, update, roll back" below.

Then open **<http://127.0.0.1:8741/>** and pair:

```bash
journalctl -u jarvisd | grep -i pairing     # the one-time code
```

**A fresh install talks to nothing, and nothing talks to it.** Voice, web
search, Home Assistant, media and maps each stay switched off until
`/etc/jarvis/jarvisd.toml` names them — and the daemon binds **loopback only**,
so until you open it up on purpose (next section) the only thing that can reach
it is this machine. That file is commented throughout;
`docs/09-operations.md` §1 is the reference.

### Open it to the LAN (needed for satellites)

jarvisd refuses to bind anything but loopback without TLS — it will not serve
device tokens in the clear on a network. So the certificate comes first:

```bash
sudo ./install/generate-tls-cert.sh          # writes /var/lib/jarvis/tls
sudo nano /etc/jarvis/jarvisd.toml           # bind = "0.0.0.0:8741"; uncomment [server.tls]
sudo systemctl restart jarvisd
```

It is self-signed, and that is the design: there is no CA in a house, so each
node **pins** this certificate's fingerprint when it pairs (ADR-031 §4).
Generate it **once** — regenerating it invalidates every paired node, which is
why the script refuses to overwrite an existing one.

### Add a satellite

A node with a microphone and speakers that answers to "hey jarvis". It is a
**user** service — it needs your graphical session's audio devices, and it
needs the LAN listener above.

```bash
sudo install -m0755 bin/jarvis-agent /usr/local/bin/
jarvis-agent pair --server https://jarvis.lan:8741 --name kitchen
mkdir -p ~/.config/systemd/user && cp systemd/jarvis-agent.service ~/.config/systemd/user/
systemctl --user enable --now jarvis-agent
```

### Check it

```bash
sudo ./install/first-run.sh --check-only
curl -s http://127.0.0.1:8741/api/v1/diagnostics/health
```

## Back up, update, roll back

The database holds your sessions, timers, devices, automations, memories and the
audit trail; the artifact store holds the bytes those rows point at. **Back up one
without the other and the restore looks complete and is not** — so these scripts
cross-check the two and refuse when they disagree.

```bash
# nightly, via a systemd timer — run as root so it can read the installed secrets.
# JARVIS_PG_CONTAINER runs pg_dump inside Postgres's own container: an installed
# host has no postgresql-client, and the container's tools always match its server.
sudo bash -c '
  set -a; . /etc/jarvis/secrets.env; set +a
  DATABASE_URL="$JARVIS_DB_URL" JARVIS__STORAGE__ARTIFACTS_ROOT=/var/lib/jarvis/artifacts \
    JARVIS_PG_CONTAINER=jarvis-postgres-1 \
    ./install/backup.sh /var/backups/jarvis
'

sudo ./install/install.sh    # upgrade: takes its own backup, migrates, health-gates
```

**Rollback is restore from backup. There is no `down` migration**, and that is a
decision rather than an omission: a `down` that drops the column a failed upgrade
just populated destroys data you still had. Restoring the backup the upgrade took
seconds earlier does not — but restoring the database alone is not enough; run
the matching **old** binary against it too, or you reproduce the failure.

```bash
sudo bash -c '
  set -a; . /etc/jarvis/secrets.env; set +a
  DATABASE_URL="$JARVIS_DB_URL" JARVIS__STORAGE__ARTIFACTS_ROOT=/var/lib/jarvis/artifacts \
    JARVIS_PG_CONTAINER=jarvis-postgres-1 \
    ./install/restore.sh /var/backups/jarvis/jarvis-<timestamp> --force
'
```

`--force` is required because restoring over a populated, live database is
refused otherwise — it is not reversible and the mistake is expensive. See
`docs/09-operations.md` §3 and §3a for the full procedure, including why the
database is dumped before the blobs and not after.

## What it does, and what it does not

It answers questions, searches and reads the web, sets timers and alarms, keeps
lists, controls Home Assistant devices, plays and casts media, writes code into
patch artifacts, builds small generated apps, remembers what you tell it to, and
speaks answers aloud on whichever node you spoke to.

It does **not**: support more than one owner (single-owner, multi-device by
design), run a local reasoning model (it drives the Claude Code CLI), execute
anything at risk tier R2 or above without an explicit approval that mints a
scoped, expiring grant, or monetise a recommendation — ever (ADR-021).

## Building it yourself

Rust is pinned by `rust-toolchain.toml`; Node 24 for the shell.

```bash
docker compose -f infra/compose/dev.yml up -d postgres
cargo test --workspace
cargo xtask arch-test && cargo xtask golden
JARVIS_RELEASE_KEY=~/.ssh/id_ed25519 \
  infra/install/release.sh dist/   # advisory-scans, builds, stages, signs
```

Publishing one is the same command plus an upload, and it is deliberately a
local step — the signing key never goes near a CI runner (`docs/06` §9):

```bash
tar --zstd -cf jarvis-$VERSION-x86_64-linux-gnu.tar.zst -C dist jarvis-$VERSION
gh release create v$VERSION jarvis-$VERSION-x86_64-linux-gnu.tar.zst --generate-notes
```

`cargo xtask dist --stage <dir>` (the `xtask` alias is defined in
`.cargo/config.toml`) stages the same installable payload alone, without
signing — useful for inspecting what ships, but `release.sh` is the command
that produces something `verify-release.sh` will accept.

`docs/TRY-IT.md` runs the whole stack from a source tree without installing it.
`CLAUDE.md` carries the conventions and the invariants.

## Working on it with Claude Code

The `.claude/` tree ships the subagents, skills and slash commands the project is
built with. `/milestone` decomposes a milestone, `/feature` drives one vertical
feature, `/gate` produces the exit evidence for sign-off, `/adr` records a
decision. The loops are defined in `docs/11-development-process.md`.

## Document map

| File | Contents |
|---|---|
| [`CLAUDE.md`](CLAUDE.md) | Conventions, commands, and the non-negotiable invariants. |
| [`docs/00-vision.md`](docs/00-vision.md) | Problem, product definition, principles, non-goals. |
| [`docs/01-requirements.md`](docs/01-requirements.md) | Requirements, acceptance criteria, hardware sizing. |
| [`docs/02-architecture.md`](docs/02-architecture.md) | Architecture, crate boundaries, runtime flows, deployment. |
| [`docs/03-tech-stack.md`](docs/03-tech-stack.md) | The Rust stack in detail. |
| [`docs/04-data-model.md`](docs/04-data-model.md) | Entities, PostgreSQL schemas, artifact store. |
| [`docs/05-api-contracts.md`](docs/05-api-contracts.md) | REST endpoints, WebSocket protocol, core contracts. |
| [`docs/06-security.md`](docs/06-security.md) | Trust zones, threat model, risk tiers, execution grants, signed releases. |
| [`docs/07-testing.md`](docs/07-testing.md) | Test pyramid, golden traces, definition of done. |
| [`docs/08-roadmap.md`](docs/08-roadmap.md) | Milestones and their exit evidence, deferred decisions, risks. |
| [`docs/09-operations.md`](docs/09-operations.md) | Configuration, deployment units, backup/restore, runbooks. |
| [`docs/11-development-process.md`](docs/11-development-process.md) | The four build loops and the human decision points. |
| [`docs/12-ui-design.md`](docs/12-ui-design.md) | UI design (normative): the HUD, card grammar, backgrounds, maps. |
| [`docs/13-use-case-catalog.md`](docs/13-use-case-catalog.md) | ~50 validated interactions; source for golden traces. |
| [`docs/adr/README.md`](docs/adr/README.md) | Architecture decision records — 34 on this branch. |
| [`docs/TRY-IT.md`](docs/TRY-IT.md) | Running the whole stack from a source tree. |

## Status

Milestone M10, "product hardening", in progress. Install, verification, backup,
restore and upgrade are built and tested (`cargo test -p xtask --test install`).
CI has a job that cuts a signed release and installs it on a container that has
never had Jarvis (`.github/workflows/ci.yml`'s `install-artifact`); it has not
run yet, so that claim is untested rather than false.

Licenses, provider terms and model capabilities change; re-verify external
references before redistribution. This is technical guidance, not legal advice.
