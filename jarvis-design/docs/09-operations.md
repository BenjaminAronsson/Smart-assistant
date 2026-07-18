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

[home_assistant]
enabled = false                          # flip at M5
base_url = "http://homeassistant.local:8123"
token_secret = "keyring:jarvis/ha-token"
entity_allowlist = ["light.office", "scene.evening"]

[voice]
enabled = false                          # flip at M5
wyoming_stt = "tcp://127.0.0.1:10300"
wyoming_tts = "tcp://127.0.0.1:10200"
audio = { sample_rate = 16000, channels = 1, format = "s16le" }

[integrations.media]
enabled = false                          # flip at M3 (MPRIS) / M5 (Spotify)
media_window_app_id = "jarvis-media"
default_display = "secondary"
max_volume_pct = 70                      # above this => R2 approval

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

## 3. Backup and restore (NFR-05, M8 gate but implemented early)

- **Nightly**: `pg_dump -Fc` of the database + hardlink snapshot of the artifact CAS
  (CAS is immutable content — rsync-friendly) + config copy. Keyring is *not* backed up
  automatically; document manual secret re-provisioning.
- **Restore test** is part of the M8 checklist and quarterly thereafter: restore into a
  scratch compose env, run the golden trace suite against it.
- Audit schema is included in dumps; hash-chain verification runs post-restore.
- Upgrade procedure: backup → apply migrations (`sqlx migrate run`) → health gate →
  on failure, rollback binary + `pg_restore`.

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
