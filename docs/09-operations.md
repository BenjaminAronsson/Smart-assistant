# 09 — Operations: configuration, deployment, backup

Fills the operational gap between the architecture (`02` §12) and a running system.
Everything here is v1 single-PC; M7/M8 evolutions noted inline.

## 1. Configuration reference

Layered via figment: `/etc/jarvis/jarvisd.toml` → `~/.config/jarvis/jarvisd.toml` →
environment (`JARVIS__…`) → keyring references. Validated at startup; invalid config is
fail-fast with a precise error. Secrets are **references**, never values.

```toml
# jarvisd.toml — annotated example (defaults shown where they exist)

[server]
bind = "127.0.0.1:8741"        # loopback only for M0–M2 (06 §7)
web_assets = "/usr/share/jarvis/web"

[database]
url_secret = "keyring:jarvis/db-url"   # postgres://… with password
max_connections = 8

[artifacts]
store_path = "/var/lib/jarvis/artifacts"
max_artifact_bytes = 52428800          # 50 MiB default budget ceiling

[providers.claude-cli]
enabled = true
binary = "claude"                       # resolved on PATH of the service user
workdir = "/var/lib/jarvis/claude-work" # controlled working directory (ADR-004)
reasoning_disable_builtin_tools = true
timeout_secs = 300
idle_timeout_secs = 60
single_flight = true                    # ADR-011; do not raise without a reason
backoff_initial_secs = 30
backoff_max_secs = 1800

[providers.embeddings]
model = "bge-small-en-v1.5"             # fastembed ONNX, CPU
cache_dir = "/var/lib/jarvis/models"

[policy]
default_risk_auto = "R1"                # auto-execute up to this tier when in scope
approval_ttl_secs = 300
grant_single_use = true                 # do not change; documented invariant

[budgets]
max_model_turns = 6
max_tool_calls = 12
max_run_duration_secs = 600
# NOTE: `[budgets]` is not yet read from config (M1) — `RunBudget::default_interactive()`
# in `jarvis-domain` hardcodes 8 model turns / 16 tool calls / 120 s duration / 8 MiB
# artifacts for the interactive path today; config-driven overrides land when the
# `[budgets]` section is wired (tracked for M2).

[home_assistant]
enabled = false                          # flip at M5
base_url = "http://homeassistant.local:8123"
token_secret = "keyring:jarvis/ha-token"
entity_allowlist = ["light.office", "scene.evening"]

[voice]
enabled = false                          # flip at M5
wyoming_stt = "tcp://127.0.0.1:10300"
# Optional (F5.2). ABSENT => no spoken responses: the round trip still works —
# the transcript starts a run and the answer streams as text — it is simply not
# synthesized. Same opt-in stance as the media/web/MCP capabilities.
wyoming_tts = "tcp://127.0.0.1:10200"
audio = { sample_rate = 16000, channels = 1, format = "s16le" }

[integrations.media]
enabled = false                          # MPRIS transport control (M3, F3a.7); Spotify at M5
max_volume_pct = 70                      # at/below => R1 auto; above => R2 media.volume_boost approval
# The media window's app-id is the fixed `jarvis.media` (Surface::MediaWindow) —
# the agent accepts only the `jarvis.` namespace, so it is not configurable — and
# its monitor comes from the ordinary display profile:
#   [display.profile] media_window = "HDMI-A-1"
# With no media_window assignment, `media.open_url` (cast-a-link) is not
# registered at all rather than casting onto an arbitrary screen.

[integrations.spotify]
enabled = false
client_id = "…"                          # own Spotify developer app
token_secret = "keyring:jarvis/spotify-refresh"
market = "from_token"
device_aliases = {}                      # room name -> Spotify Connect device id, e.g. { kitchen = "abc123" }

[integrations.youtube]
data_api_key_secret = ""                 # optional; empty => search via browser worker

[ui]
background = "none"                      # none | abstract | photo
background_photo = ""                    # path when background = "photo"
panel_ttl_hours = 2                      # FR-24; approvals exempt
deepdive_promote_after = 3               # FR-27; offer Research Notes artifact after N follow-ups on one thread
motion = "auto"                          # auto | reduced (auto honors OS setting + battery)

[maps]
pmtiles_path = "/var/lib/jarvis/maps/region.pmtiles"   # empty => OSM raster fallback (online only)

[news]
topics = []                              # e.g. ["technology", "formula 1", "local"] — resolves "what's the news"
sources = []                             # optional preferred outlets; empty => source-quality weighting picks
# with both empty, Jarvis asks once and offers to remember (ADR-019)

[location]
home_lat = 0.0                           # set to the owner's actual home coordinate
home_lon = 0.0
allow_device_gps = true                  # highest-priority source when a paired device grants it
allow_ip_geolocation_fallback = true     # coarse last resort; always labeled approximate

[timers]
alarm_sound = "default"                  # always-available playback path, independent of TTS
announce_reminders = true                # TTS "reminder — call Mom" when voice available

[apps]                                   # generated apps (FR-18, M6). Opt-in: unset ⇒ no app.generate tool.
enabled = false
worker_command = "node"                  # ops owns the launch profile (ADR-027)
worker_args = ["tools/app-builder/src/index.mjs"]
lockfile = "tools/app-builder/templates/dashboard-v1/package-lock.json"
                                         # hashed by the HOST into every bundle's build provenance
# worker_image = "jarvis-app-builder@sha256:…"
                                         # set ONLY for a profile that really isolates the network:
                                         # its presence is what lets the host attest
                                         # `network: disabled`. Unset ⇒ `enabled` is recorded, honestly
                                         # (D-M6-1). Requires `npm --prefix tools/app-builder run
                                         # install-templates` in the dev/CI fallback.

[integrations.caldav]
enabled = false                          # flip at M4
server_url = ""
username = ""
password_secret = "keyring:jarvis/caldav"

[integrations.smtp]
enabled = false                          # flip at M4
host = ""
port = 587
username = ""
password_secret = "keyring:jarvis/smtp"
from_address = ""

[integrations.web_search]
enabled = false                          # flip at M2
provider = "brave"                       # swappable adapter; any keyed/self-hosted provider fits the port
api_key_secret = "keyring:jarvis/websearch-key"
fetch_timeout_secs = 8
max_fetch_bytes = 2000000                # untrusted content — hard cap before extraction (06 §5)

[observability]
otlp_endpoint = "http://127.0.0.1:4317"
diagnostics_redact = true                # never ship bundles with secrets/prompts

[automations]
allow_unattended_llm = false             # ADR-011: reasoning automations defer by default
```

`jarvis-agent.toml` (user service) holds: `jarvisd` URL, device token keyring reference,
Chromium binary + app-id map, application launch allowlist, display profile
(surface → monitor/workspace), PTT hotkey.

## 2. Deployment units

| Unit | File | Notes |
|---|---|---|
| `jarvisd.service` | system unit, `User=jarvis` | `DynamicUser=no`, dedicated user; `ProtectSystem=strict`, `ReadWritePaths=/var/lib/jarvis`; `Restart=on-failure`; `MemoryMax=512M` guard. |
| `jarvis-agent.service` | **user** unit | Graphical session; `After=graphical-session.target`; access to Hyprland sockets + audio. |
| `postgres` | compose (`infra/compose/dev.yml`, `prod.yml`) | pgvector image, local volume, no published ports beyond loopback. |
| `otel-collector` | compose | Loopback OTLP in, local export. |
| voice services | compose (M5) | Wyoming ports on a private compose network + loopback. |
| tool workers | compose, per-trust profiles (M2/M3) | Read-only mounts, `network_mode` restricted, CPU/mem/pids limits. |

Claude CLI runs as the `jarvis` service user; authenticate once interactively
(`sudo -u jarvis claude login` or equivalent) so the daemon's spawned processes inherit
valid credentials. Document the re-auth runbook (§5) — expired CLI auth is the most
likely "mystery outage".

## 3. Backup and restore (NFR-05, FR-30 — implemented in F10.2)

Two scripts, and the second one is the point:

```bash
export DATABASE_URL=postgres://jarvis:...@127.0.0.1:5432/jarvis
export JARVIS__STORAGE__ARTIFACTS_ROOT=/var/lib/jarvis/artifacts

infra/install/backup.sh  /var/backups/jarvis
infra/install/restore.sh /var/backups/jarvis/jarvis-<timestamp>
```

**A house is two stores.** Postgres holds sessions, runs, devices, timers,
automations, memories, the audit trail, and artifact *manifests*; the CAS holds the
blob *bytes* those manifests point at. Back up one without the other and the restore
looks complete — every artifact listed, none readable. Both scripts therefore
cross-check the two stores and exit non-zero if they disagree.

**Order is load-bearing, and it is the opposite of the obvious one.** The database is
dumped *first*, then the blobs. Blobs are content-addressed and never deleted, so a
manifest captured at t0 still has its blob at t1. Copy the blobs first and a manifest
written in between points at bytes the copy never saw.

**Client and server versions must match.** A dump written by a newer `pg_dump` can be
rejected by an older server (`pg_dump` 18 emits `SET transaction_timeout`, which a 16
server refuses) — and you find out on the day you need it. Both scripts compare
versions and **refuse before writing anything** rather than producing an archive that
looks fine. Point them at matching tools with either:

```bash
JARVIS_PG_CONTAINER=jarvis-dev-postgres-1 infra/install/backup.sh ...  # the server's own
PGDUMP=/usr/lib/postgresql/16/bin/pg_dump  infra/install/backup.sh ...  # or explicit paths
```

- **Restoring over a populated database is refused** unless `--force` is passed; it is
  not reversible and the mistake is expensive. Normally restore into a *fresh* database.
- **The keyring is not backed up** — secrets are re-provisioned manually after a restore
  (`infra/install/first-run.sh` reports what is missing).
- **Restore is tested, not assumed**: `crates/jarvis-infra/tests/backup_restore.rs` runs
  these exact scripts, restores into a throwaway database and a different blob root, and
  asserts devices are still paired, automations still hold their `created_by`, timers are
  still armed, and artifacts still resolve to their bytes. Verified by mutation — with
  `pg_restore` stubbed out, those tests fail.
- Nightly via systemd timer; a quarterly restore drill remains the operator's job.
- Upgrade procedure: backup → `sqlx migrate run` → health gate → on failure, roll back
  the binary and `restore.sh` (a repeatable procedure is F10.3).

## 4. Configuration profiles (operational presets)

| Profile | Providers | Tools | Network |
|---|---|---|---|
| **Degraded / offline** | deterministic + embeddings only | R0/R1 within grants; HA rule-based intents | No external egress. |
| **Default personal** | claude-cli → degraded queue | Curated tools; cloud-context filtering visible | Anthropic + approved integrations only. |
| **Coding sandbox** | claude-cli coding profile | read/edit/git/test inside disposable worktree only | Dependency registry allowlist if approved. |
| **Home control** | deterministic intents first; claude-cli only for ambiguity | HA curated commands | Local HA only. |

Profiles are selectable per session and per automation; the active profile is always
visible in the UI provider indicator.

## 5. Low-power / ultrabook tuning

Applies when the host matches the "Ultrabook v1" profile (`01` §4). Goal: invisible at
idle, bounded at peak, cool and quiet.

- **PostgreSQL small tuning:** `shared_buffers=128MB`, `max_connections=20`,
  `work_mem=8MB`, `effective_cache_size=1GB`, autovacuum defaults. Single-owner load is
  trivial; do not cargo-cult server-class settings.
- **OTel collector: off by default.** `jarvisd` writes OTLP to a rotating local file
  exporter; run the collector + viewer only when actively debugging. Traces are still
  produced — just not continuously shipped.
- **Embeddings lifecycle:** lazy-load on first retrieval, unload after 10 min idle
  (config `[providers.embeddings] idle_unload_secs`). Cold load ≈ 1–2 s — acceptable for
  memory retrieval.
- **Worker serialization:** the scheduler never runs Playwright, a coding-profile CLI
  run, and voice concurrently on a low-power profile
  (`[budgets] max_concurrent_workers = 1`).
- **Voice:** faster-whisper `tiny`/`base` int8, beam size 1; Piper low/medium voice. Pin
  STT to performance cores where the kernel exposes them.
- **Chromium clients:** app-mode with `--disable-background-networking`; on RAM-tight
  hosts prefer one window with surface tabs over many windows; 8 GB hosts enable zram.
- **Build performance (dev on the same machine):** `cargo check` inner loop, mold linker,
  sccache, dev profile `opt-level=0`. Clean workspace builds run 5–15 min on a U-class
  CPU; incremental check is seconds. CI does the expensive release builds.
- **Thermal sanity check:** after M1, `jarvisd` + Postgres at idle must not appear among
  `powertop`'s top consumers; if they do, treat it as a bug (usually a polling loop that
  should be event-driven).

## 6. Runbooks (docs/runbooks/, written with the feature they cover)

Minimum set before M8: Claude CLI re-authentication; quota-exhausted behavior and reset
window; database restore drill; artifact CAS integrity check (`jarvisd verify-cas`);
device token revocation; adding an HA entity to the allowlist; collecting a redacted
diagnostics bundle; full-disk recovery (Postgres + CAS on same volume — alert at 85%).

## 11. Network exposure: loopback, LAN, remote (F7.3, docs/06 §7)

`jarvisd` binds **loopback** by default and needs nothing else. Any other bind requires
`[server.tls]`, and **startup refuses without it** — there is no override flag, because a
daemon serving device tokens in the clear on a LAN is the one configuration mistake with
no recovery: the credential is gone the moment it is used.

```toml
[server]
bind = "0.0.0.0:8443"          # anything non-loopback ⇒ TLS required
tls = { cert_path = "/etc/jarvis/tls/cert.pem", key_path = "/etc/jarvis/tls/key.pem" }
```

Both paths must be absolute — a relative path resolves against whatever directory the
service happened to start in.

**The certificate is self-signed, and that is fine.** There is no CA in a house. What makes
it meaningful is the **fingerprint**: `jarvisd` logs it at startup and returns it in the
node pairing response (`serverFingerprint`, ADR-031), delivered inside the ceremony the
owner already trusted enough to read a one-time code across. The node pins it and refuses
anything else afterwards. Generate one with:

```bash
openssl req -x509 -newkey ed25519 -nodes -days 3650 \
  -subj "/CN=jarvis.lan" -addext "subjectAltName=DNS:jarvis.lan,IP:192.168.1.10" \
  -keyout /etc/jarvis/tls/key.pem -out /etc/jarvis/tls/cert.pem
chmod 600 /etc/jarvis/tls/key.pem     # readable only by the service user
openssl x509 -in /etc/jarvis/tls/cert.pem -noout -fingerprint -sha256
```

Rotating the certificate **breaks every paired node's pin**; they re-pair. Plan a rotation
the way you would plan re-pairing the house.

**Cast-a-link with more than one screen.** `media.open_url` is R1 — it executes without an
approval — and carries a URL verbatim that model output can influence. With room nodes
paired, name the screen it belongs on, or the media window opens on all of them:

```toml
[display]
media_window_device = "kitchen"   # a room alias from node_aliases, or a device id
```

Unset keeps the single-screen behaviour every earlier milestone shipped.

**The health endpoint follows the bind.** `GET /api/v1/diagnostics/health` is
unauthenticated only while the listener is loopback (docs/05 §6.2). On any other bind it
moves behind authentication automatically — off loopback it is an unauthenticated readout
of adapter state and, while a window is open, of the bootstrap pairing code.

**Firewall.** Expose only the jarvisd port, and only to the subnet that holds the
satellites:

```bash
firewall-cmd --permanent --new-zone=jarvis
firewall-cmd --permanent --zone=jarvis --add-source=192.168.1.0/24
firewall-cmd --permanent --zone=jarvis --add-port=8443/tcp
firewall-cmd --reload
```

Postgres, Wyoming, MCP servers, the browser/coding/app workers and any model server stay
on loopback or a private container network — none of them gain a LAN listener because
jarvisd did.

**Remote (outside the house).** Use a private overlay — Tailscale or WireGuard — and bind
jarvisd to the overlay interface. **Never** port-forward jarvisd from a router: the pairing
window, the health page and every device token would then be reachable from the open
internet, and a 6-digit pairing code is ~20 bits. This is a standing rule, not a default
(docs/06 §7).
