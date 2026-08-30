# Install and packaging — design

**Date:** 2026-08-19 · **Status:** approved in brainstorming, awaiting spec review
**Proposed feature:** F10.9 — the install artifact (M10, before F10.7)

## The problem

There is no install. `docs/TRY-IT.md` documents a *developer* install: clone the repo,
`cargo build --release`, `npm run build`, `compose up`, then run `./target/release/jarvisd`
with eight environment variables exported into an interactive shell. That produced F10.1's
evidence and it was the right thing to do at the time, but it is not something that survives
a reboot, and it requires a source tree and two toolchains on the machine that runs Jarvis.

`docs/09` §2 already describes the intended runtime shape — a hybrid of native systemd
services and compose-managed containers. What is missing is everything between "CI builds
the workspace" and "that shape exists on a host": no artifact, no installer, no production
compose file, and a systemd ordering dependency that is wrong.

## Decisions taken during brainstorming

1. **Audience: the owner's own machines.** Not a public self-hosting project. This does not
   weaken `docs/08` §1's M10 exit evidence — a published release asset fetched with `curl`
   satisfies "no source tree, following written instructions" exactly as written. Only the
   audience narrows, and the audience does not change the artifact.
2. **Topology: build here or in CI, host elsewhere.** The host is a separate x86_64
   Debian/Ubuntu machine (the 8 GB ultrabook profile of `docs/01` §4.1).
3. **`jarvisd` is not containerized.** Reasons specific to this codebase, not preference:
   it spawns the Claude Code CLI as a child process with interactive login credentials; it
   launches tool workers as containers itself (containerizing it means docker-in-docker);
   secrets are keyring references (invariant 5) and a container has no session keyring; and
   `jarvis-agent` cannot be containerized at all — it needs Hyprland IPC sockets, audio
   devices and a microphone. Containerizing the daemon buys a second deployment model on the
   machine with the least RAM.
4. **Containers keep three jobs**, all of which they are already the right answer for:
   third-party dependencies (Postgres/pgvector, Wyoming STT/TTS, otel-collector), sandboxed
   tool workers where the isolation *is* the security boundary (`docs/06` §6), and the
   reproducible build that F10.7 will sign.

## Architecture

### Build

**Amended 2026-08-20: F10.7 landed first, and it changes this.**

The original plan was `cargo xtask dist` producing its own tarball, with F10.7 signing it
later. F10.7 (`0730883`) shipped ahead of this feature instead, and `infra/install/release.sh`
now builds, checksums, signs with `ssh-keygen -Y` and records an advisory-scan date — over a
payload of exactly two binaries.

Shipping a second `dist` tarball beside it would leave **`install.sh`, `prod.yml` and the
systemd units unsigned**, next to a valid signature covering the binaries. That is worse than
signing nothing: a root-executed installer is a better target than the binary it installs,
and the signature makes the whole directory look checked.

So there is **one artifact**. `cargo xtask dist --stage <dir>` owns *what ships* — a pure,
dependency-free function, so a forgotten file fails a millisecond test rather than an install.
`release.sh` calls it in place of its own `cargo build`, then checksums every file it finds
rather than a list it must remember to update. The GitHub Actions job on a `v*` tag runs
`release.sh` with an **ephemeral** key: that proves the pipeline produces a verifiable
release, not that the real key signed it — the real key never goes near a runner (docs/06 §9).

CI already runs `cargo build --workspace --release` on `ubuntu-latest` and discards the
binaries, so this is a small increment on an existing green path.

ABI: `ubuntu-latest` links against glibc 2.39; the host is the same family or newer, and
glibc is forward-compatible. Plain dynamic linking. No musl, no cross-build. The host needs
`libasound2` present, because `jarvis-agent` links it for capture (F8.2).

The repository is public, so release assets are fetchable with unauthenticated `curl`.

### Artifact layout

```
jarvis-<version>-x86_64-linux-gnu.tar.zst
├── bin/{jarvisd,jarvis-agent}
├── web/                              # web/dist/jarvis-shell/browser
├── migrations/
├── compose/prod.yml + postgres-init/
├── systemd/{jarvis-deps.service,jarvisd.service,jarvis-agent.service}
├── install/{install,first-run,backup,restore,update}.sh
├── jarvisd.toml.example
├── README.md                         # the install half, so the release is self-describing
├── SHA256SUMS                        # every file above (F10.7's manifest, widened)
├── RELEASE                           # version, build time, advisory-scan date
├── SIGNED-PAYLOAD + .sig             # ssh-keygen -Y over SHA256SUMS *and* RELEASE
└── signing-key.pub
```

`install/verify-release.sh` (F10.7) is what an owner runs first: it checks every file against
the manifest and **refuses an advisory scan older than 30 days**, because a signature proves
these are the bytes that were built and says nothing about whether anyone has looked at the
world since. It also states that it has verified integrity and not authenticity — the public
key ships inside the release, so agreement between the parts is all it can prove.

### Runtime shape on the host

Three units:

| Unit | Type | Ordering |
|---|---|---|
| `jarvis-deps.service` | system, `Type=oneshot`, `RemainAfterExit=yes` | `After=network-online.target docker.service` |
| `jarvisd.service` | system, `User=jarvis` | `After=jarvis-deps.service` |
| `jarvis-agent.service` | **user** unit | `After=graphical-session.target` |

```ini
# jarvis-deps.service
ExecStart=/usr/bin/docker compose -f /etc/jarvis/compose/prod.yml up -d --wait
ExecStop=/usr/bin/docker compose -f /etc/jarvis/compose/prod.yml down
```

`--wait` is load-bearing. It blocks until the healthchecks pass — `dev.yml` already defines
`pg_isready` for Postgres — so `After=jarvis-deps.service` means *Postgres is accepting
connections*, not *compose was invoked*.

**This fixes a live bug.** `infra/systemd/jarvisd.service` currently declares
`After=postgresql.service`. There is no `postgresql.service` on a host where Postgres is a
container, so the ordering constraint is vacuous and the daemon fail-fasts on every boot.

`prod.yml` is a new file. `docs/09` §2 cites it today but it does not exist, and `dev.yml`
cannot stand in: it hardcodes `jarvis-dev-only` as the password. `prod.yml` carries Postgres,
the otel-collector, and the voice services folded in — on a dedicated host, voice is not the
optional extra it is on a development laptop, so the `dev.yml`/`voice.yml` split stops
earning its keep.

### Secrets on the host — corrected after a blocker was found

The original sketch said "compose needs a password, so one `.env` lands on disk; `jarvisd`
keeps using the keyring". **The second half is false, and it would have failed on the first
boot.**

`jarvisd` resolves `keyring:jarvis/database-url` through `keyring` v3 with the
`async-secret-service` backend (`crates/jarvisd/Cargo.toml:69`) — that is **Secret Service
over D-Bus**. `jarvisd.service` is a *system* unit running `User=jarvis` with
`ProtectHome=true`: no login session, no session bus, no unlocked collection. The lookup
cannot succeed, and `config.rs` treats an unresolvable secret reference as a fatal config
error by design. The daemon would fail fast on every start, on a host where every other
check looked healthy.

**The fix, which the config layer already supports as a first-class scheme:** one root-owned
file, `/etc/jarvis/secrets.env`, mode 0600, holding both values:

```
JARVIS_PG_PASSWORD=…      # docker compose --env-file
JARVIS_DB_URL=postgres://jarvis:…@127.0.0.1:5432/jarvis
```

`jarvisd.toml` then sets `url_secret = "env:JARVIS_DB_URL"`, and `jarvisd.service` gains
`EnvironmentFile=/etc/jarvis/secrets.env`. systemd reads that file **as root, before dropping
to `User=jarvis`**, so the file need never be readable by the service account. The value
never appears in a command line, so invariant 5's "no secrets in CLI args" holds; it is
readable via `/proc/<pid>/environ`, which on this host means root only.

The keyring stays correct for `jarvis-agent` (a *user* unit, inside a graphical session,
where Secret Service genuinely works) and for interactively-provisioned secrets like the
Home Assistant token.

So: **one secret file on disk, not two, and it also carries the database URL.** It is
root-only and the database is loopback-only. **This trade needs explicit owner acceptance —
see the human-only decisions below.**

### Migrations without `sqlx-cli` on the host

`jarvis-infra` already embeds the migration stream (`sqlx::migrate!`, `lib.rs:29`) but
`docs/09` has operators run `sqlx migrate run`, which means installing `sqlx-cli` — a Rust
toolchain dependency on a machine that is supposed to need neither Rust nor a source tree.

Add a `jarvisd migrate` subcommand that runs the embedded `MIGRATOR`. `jarvisd` currently
parses no arguments at all (`main.rs:26`), so this is a small, contained change. `update.sh`
and `install.sh` then call `jarvisd migrate`, and the host's requirements drop to: a container
runtime, `libasound2`, and systemd.

### Install and update

`install.sh` runs as root on the host:

1. **Preflight, failing loudly**: glibc version, container runtime + compose plugin,
   `libasound2`, systemd present. A check that cannot fail is worse than no check
   (`first-run.sh` already argues this at length; the same standard applies here).
2. Create the `jarvis` system user, `/var/lib/jarvis`, `/etc/jarvis`.
3. Place `bin/*` → `/usr/local/bin`; `web/` → `/var/lib/jarvis/web`; `migrations/` →
   `/var/lib/jarvis/migrations`.
4. Place `compose/` → `/etc/jarvis/compose/`; generate `/etc/jarvis/secrets.env` (0600,
   root) with a random Postgres password and the matching database URL.
5. `jarvisd.toml.example` → `/etc/jarvis/jarvisd.toml`, only if absent — never overwrite a
   configured host.
6. Run `jarvisd migrate` against the freshly started database.
7. Install the three units; `systemctl enable --now jarvis-deps jarvisd`.
8. Run `first-run.sh --check-only` and report.

**Update and rollback already exist and are tested.** `update.sh` takes a verified backup,
applies forward migrations, health-gates the daemon, and prints the exact restore command on
failure; `restore.sh` is covered by `crates/jarvis-infra/tests/backup_restore.rs`. Packaging
calls them and does not reinvent them. A new tarball whose `install.sh` detects an existing
install delegates to `update.sh`. Rollback remains restore-from-backup, as F10.3 established
in writing.

## The README

The current `README.md` is a handover document for *building* Jarvis, not a README for a
thing you install, and it has drifted: it says "ready for M0" at M10, "Milestones M0–M8" when
M9 and M10 exist, and "ADR-001 … ADR-026" when there are 34.

Rewrite it so the first screen answers what Jarvis is and how to install it. Target shape:

1. **What it is** — three or four sentences, kept from the existing opening paragraph, which
   is good.
2. **Install** — the whole path, short enough to read at once: download the asset, verify the
   checksum, untar, `sudo ./install.sh`, open the HUD, pair. Links to `docs/TRY-IT.md` for
   the from-source path and to `docs/09` for configuration.
3. **What it can do** — a short honest list, including what it cannot.
4. **Backup, update, rollback** — three commands, because a house that cannot be restored is
   not finished, and the owner should not have to find that in `docs/09` §3.
5. **Building it yourself / working on it** — the current "How to use this with Claude Code"
   and the document map, moved *below* the install, with the stale facts corrected.

The install half is duplicated into the tarball as its own `README.md`, so the artifact is
self-describing when it is sitting on a host with no repository.

Accuracy is a requirement of this section, not a nicety: the README is the one document an
owner reads before anything works, and a wrong instruction there costs more than a wrong
instruction anywhere else.

## Testing

- **`xtask dist` unit test**: the produced tarball contains every path the layout above
  names, and every `MANIFEST` checksum verifies against the file it describes.
- **CI clean-install test**: the release job untars its own artifact in a fresh
  `ubuntu:24.04` container and runs `install.sh` followed by `first-run.sh --check-only`.
  This proves preflight and placement. It does **not** prove boot ordering — systemd in a
  container is unreliable enough that asserting on it would be the kind of check that stays
  green while the system is broken. The reboot path is a human check and is named as one.
- **Golden 10 / F10.8** already scopes install → talk → break → restore → upgrade → roll
  back; this feature supplies the install leg.
- **Regression for the ordering bug**: a test asserting `jarvisd.service` does not reference
  `postgresql.service` and does order after `jarvis-deps.service`.
- **Regression for the keyring blocker**: a test asserting the shipped `jarvisd.toml.example`
  resolves its database URL through an `env:` reference, not a `keyring:` one — the failure
  mode it guards is a daemon that cannot start on any host with no login session.

## Out of scope

- Signing and provenance attestation — that is F10.7, and it depends on this.
- ARM / Raspberry Pi satellites. `jarvis-agent` on aarch64 needs a cross-build and arm64
  Wyoming images; not needed for an x86_64 host.
- Static musl binaries. Unnecessary given the confirmed host family.
- Publishing container images or a release channel. Not a self-hosting project.
- A `Dockerfile` for the tool-worker `worker_image`. Noted as a real gap during
  brainstorming — `app_builder.rs:425` refuses to attest `network: Disabled` without one, and
  no Dockerfile exists in the tree — but it is a `docs/06` §6 sandboxing concern, not a
  packaging one, and it should be scoped on its own rather than smuggled in here.

## Human-only decisions this needs (`docs/11` §3)

1. **Approve F10.9 and its position in M10** (before F10.7). Adding to an approved feature
   list is human-only.
2. **Accept the `/etc/jarvis/secrets.env` secret-at-rest trade** described above — one
   root-only file holding the Postgres password and the database URL, replacing a keyring
   lookup that cannot work in a system service.

---

## Amended 2026-08-30, after the F10.9 review — who publishes, and who verifies

Two things in this spec were changed by the review of PR #82, and the design is written down
here rather than only in the diff, because both are security decisions.

**CI does not publish a release.** This document assumed a GitHub Actions job on a `v*` tag
would run `release.sh` with an ephemeral key and upload the result, and argued the ephemeral
key was fine because it proves the pipeline rather than the provenance. That is true of the
signature and false of the *upload*: publishing it to the URL the README tells an owner to
`curl` means the documented `verify-release.sh` step prints `release verified` over an
artifact signed by a key that no longer exists, and for which no `--signers` file can ever
be produced. A green check that proves nothing about who built the bytes is worse than no
check. CI still cuts a release and installs it on a clean host every run — that is the part
CI can prove. Publishing is a local step with the real key (`docs/06` §9).

**`install.sh` verifies the release itself.** The flow above put `verify-release.sh` before
`install.sh` in the README and left it there. The README's line order is not a security
control: the script that runs as root is the one that has to check, and it now refuses a
release that fails, with `--skip-verify` for the out-of-band case, which announces itself.

**And the `EnvironmentFile` has a cost this spec did not name.** Moving the database URL out
of the keyring and into the daemon's environment (the correction above, which was right) means
every child process the daemon spawns inherits the credential, `claude` included. Resolved at
the same boundary the secret is resolved at: `jarvis_adapters::host_env::scrub_secrets` on
every `Command`, with a test that fails the build when a new spawn site forgets.
