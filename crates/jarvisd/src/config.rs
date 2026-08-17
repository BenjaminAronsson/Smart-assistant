//! Layered configuration (docs/09 §1): file → env (`JARVIS__…`) → secret
//! references. Validated at startup; invalid config fails fast with a precise
//! error. Secrets are references (`env:` / `keyring:`), never values —
//! CLAUDE.md invariant 5.

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use jarvis_domain::secrecy::Redacted;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub observability: ObservabilityConfig,
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub integrations: IntegrationsConfig,
    #[serde(default)]
    pub location: LocationConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub display: DisplayConfig,
    #[serde(default)]
    pub maps: MapsConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub voice: VoiceConfig,
    pub timers: TimersConfig,
    #[serde(default)]
    pub lists: ListsConfig,
    #[serde(default)]
    pub apps: AppsConfig,
}

/// `[apps]` (FR-18, docs/06 §6, ADR-027/ADR-029/ADR-030). Generated apps.
///
/// `enabled` defaults to **false**, the same opt-in stance as every other
/// capability that spawns a process: an unconfigured host registers no
/// `app.generate` tool and can build nothing. Enabling it is the operator
/// saying they have installed the template's dependencies
/// (`npm --prefix tools/app-builder run install-templates`).
///
/// `worker_image` is the honest half of **D-M6-1**. Set it only for a launch
/// profile that really isolates the network (a container, per ADR-027); the host
/// then attests `network: disabled` in every bundle's provenance. Left unset —
/// ADR-027's dev/CI process fallback — the host attests `network: enabled`,
/// because that is what is true. The builder *refuses* to claim isolation it
/// did not have.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The command that runs the worker. Ops owns the launch profile.
    #[serde(default = "default_app_worker_command")]
    pub worker_command: String,
    /// Arguments to that command; the default runs the in-repo worker directly.
    #[serde(default = "default_app_worker_args")]
    pub worker_args: Vec<String>,
    /// The lockfile whose hash is recorded as build provenance. It is read and
    /// hashed by the **host** at startup — provenance a worker could report is
    /// provenance a worker could forge (docs/06 §5/§6).
    #[serde(default = "default_app_lockfile")]
    pub lockfile: PathBuf,
    /// Builder image reference. Presence is what lets the host attest network
    /// isolation; see the type docs.
    #[serde(default)]
    pub worker_image: Option<String>,
}

impl Default for AppsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            worker_command: default_app_worker_command(),
            worker_args: default_app_worker_args(),
            lockfile: default_app_lockfile(),
            worker_image: None,
        }
    }
}

fn default_app_worker_command() -> String {
    "node".to_owned()
}

fn default_app_worker_args() -> Vec<String> {
    vec!["tools/app-builder/src/index.mjs".to_owned()]
}

fn default_app_lockfile() -> PathBuf {
    PathBuf::from("tools/app-builder/templates/dashboard-v1/package-lock.json")
}

/// `[ui]` (docs/09 §1, docs/12 §4/§5/§6). HUD presentation and lifecycle knobs.
///
/// Every documented key is modelled, even where the behaviour currently lives
/// client-side (`background`, `motion`, `panel_ttl_hours` are F3b.4/F3b.2
/// settings the shell applies): `Config` is `deny_unknown_fields`, so a section
/// that models only *some* of what docs/09 documents would reject an operator's
/// perfectly correct config file. The section is entirely optional and every
/// key has the documented default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
    /// `none | abstract | photo` (docs/12 §5).
    #[serde(default = "default_background")]
    pub background: String,
    /// Path to the wallpaper when `background = "photo"`.
    #[serde(default)]
    pub background_photo: String,
    /// Panels self-expire silently after this many hours (FR-24, docs/12 §4).
    /// Approvals are exempt.
    #[serde(default = "default_panel_ttl_hours")]
    pub panel_ttl_hours: u32,
    /// Offer to keep a deep-dive thread as a Research Notes artifact after this
    /// many follow-ups on one thread (FR-27, ADR-017, docs/12 §2.5). **Zero
    /// disables the offer** rather than making it every turn — that is the
    /// documented way to turn the feature off, so it is a supported value, not
    /// a degenerate one.
    #[serde(default = "default_deepdive_promote_after")]
    pub deepdive_promote_after: u32,
    /// `auto | reduced` (docs/12 §6; `auto` honours the OS setting and battery).
    #[serde(default = "default_motion")]
    pub motion: String,
}

/// `[voice]` (docs/09 §1, FR-13). Voice is opt-in: the browser may render its
/// push-to-talk affordance without this service-side pipeline, but no daemon
/// connection to an external speech service is created until enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_wyoming_stt")]
    pub wyoming_stt: String,
    /// Piper (or any Wyoming TTS) endpoint for the spoken response leg (F5.2).
    /// **Absent means no TTS**, deliberately: the round trip still works — the
    /// transcript starts a run and the answer streams as text — it is simply not
    /// spoken. The stricter default, matching every other outbound capability in
    /// this config (media, web search, MCP): opt in by naming the service.
    #[serde(default)]
    pub wyoming_tts: Option<String>,
    #[serde(default)]
    pub audio: VoiceAudioConfig,
    /// The word nodes answer to (ADR-032 §1/§4). Configuration rather than
    /// code; the shell may change it, but only to one of
    /// [`Self::wake_words_available`].
    #[serde(default = "default_wake_word")]
    pub wake_word: String,
    /// The words this household has **provisioned models for**.
    ///
    /// Declared here rather than discovered, because the models live on the
    /// satellites and the daemon cannot see their filesystems. It is what the
    /// settings surface offers, so a word absent from this list cannot be
    /// chosen — the failure it prevents is a house that silently goes deaf
    /// because somebody picked a word nothing has a model for.
    ///
    /// Defaults to what `infra/install/fetch-wake-assets.sh` installs, which
    /// includes the default word — so a fresh install answers to its name
    /// rather than reporting a missing model (ADR-032 §1, amended 2026-08-17).
    /// A word outside this list is a legitimate owner choice that costs a model
    /// training run; the settings surface will not offer one.
    #[serde(default = "default_wake_words_available")]
    pub wake_words_available: Vec<String>,
    /// `[voice.elevenlabs]` (F8.11, ADR-033). Absent means never.
    #[serde(default)]
    pub elevenlabs: ElevenLabsConfig,
}

fn default_wake_word() -> String {
    // Must stay in step with `jarvis_agent::wake::DEFAULT_WAKE_WORD` — the
    // daemon serves this to nodes, so a disagreement would have a node
    // answering to one word while the shell reported another.
    "hey jarvis".to_owned()
}

fn default_wake_words_available() -> Vec<String> {
    ["alexa", "hey jarvis", "hey mycroft"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// `[voice.elevenlabs]` — a third-party voice, off unless switched on.
///
/// **Switching this on is the consent** (ADR-033 §2): it is the moment the
/// house's spoken output starts leaving the house, so it is one deliberate,
/// reversible act rather than a per-utterance prompt. Everything the local
/// voice does keeps working when it is off, and when it fails.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElevenLabsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// A **keyring reference** (`keyring:service/entry`), never a literal key
    /// (invariant 5). Resolved at the adapter boundary.
    #[serde(default)]
    pub api_key_ref: Option<String>,
    #[serde(default)]
    pub voice_id: Option<String>,
    #[serde(default = "default_elevenlabs_model")]
    pub model_id: String,
    /// Characters per process lifetime. A ceiling that makes runaway spend
    /// impossible; exhaustion falls back to the local voice rather than
    /// failing a turn (ADR-033 §5).
    #[serde(default = "default_elevenlabs_budget")]
    pub character_budget: u64,
}

/// Written by hand rather than derived: `Config::from_figment` serializes the
/// defaults in as a base layer, so a derived `Default` would put an explicit
/// `character_budget = 0` underneath the `serde(default = …)` and win — which
/// showed up as a fully-configured file being refused for having no budget.
impl Default for ElevenLabsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key_ref: None,
            voice_id: None,
            model_id: default_elevenlabs_model(),
            character_budget: default_elevenlabs_budget(),
        }
    }
}

fn default_elevenlabs_model() -> String {
    "eleven_flash_v2_5".to_owned()
}

fn default_elevenlabs_budget() -> u64 {
    100_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceAudioConfig {
    #[serde(default = "default_voice_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_voice_channels")]
    pub channels: u16,
    #[serde(default = "default_voice_format")]
    pub format: String,
}

fn default_wyoming_stt() -> String {
    "tcp://127.0.0.1:10300".to_owned()
}

fn default_voice_sample_rate() -> u32 {
    16_000
}

fn default_voice_channels() -> u16 {
    1
}

fn default_voice_format() -> String {
    "s16le".to_owned()
}

impl Default for VoiceAudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: default_voice_sample_rate(),
            channels: default_voice_channels(),
            format: default_voice_format(),
        }
    }
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            wyoming_stt: default_wyoming_stt(),
            wyoming_tts: None,
            audio: VoiceAudioConfig::default(),
            // Hand-written to match the `serde(default = …)` above, for the
            // reason recorded on `ElevenLabsConfig::default`: `from_figment`
            // serializes these defaults in as a base layer, so a field left out
            // here would put an empty list *under* the annotation and win.
            wake_word: default_wake_word(),
            wake_words_available: default_wake_words_available(),
            elevenlabs: ElevenLabsConfig::default(),
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            background: default_background(),
            background_photo: String::new(),
            panel_ttl_hours: default_panel_ttl_hours(),
            deepdive_promote_after: default_deepdive_promote_after(),
            motion: default_motion(),
        }
    }
}

fn default_background() -> String {
    "none".to_owned()
}

fn default_panel_ttl_hours() -> u32 {
    2
}

fn default_deepdive_promote_after() -> u32 {
    3
}

fn default_motion() -> String {
    "auto".to_owned()
}

/// `[maps]` (ADR-013, docs/09 §1, docs/12 §3). The locally served PMTiles
/// region extract.
///
/// Absent (or an empty path) ⇒ **no map endpoints are registered at all**, the
/// same opt-in stance as `[integrations.*]`: a host without an extract has no
/// local map surface rather than a broken one, and the HUD takes the documented
/// coverage fallback (online raster, or a coordinates-only card offline).
///
/// The extract itself is produced out of band and is *not* in the repo. The
/// documented default (docs/08 §6) is a downloaded regional extract, e.g.:
///
/// ```text
/// pmtiles extract \
///   https://r2-public.protomaps.com/protomaps-sample-datasets/protomaps_vector_planet_odbl_z10.pmtiles \
///   /var/lib/jarvis/maps/region.pmtiles --bbox=13.0,52.3,13.8,52.7
/// ```
///
/// `attribution` overrides what the archive declares. It cannot *remove*
/// attribution: whatever is configured, the served string always names
/// OpenStreetMap (docs/12 §3 — attribution is never hidden).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MapsConfig {
    #[serde(default)]
    pub pmtiles_path: Option<PathBuf>,
    #[serde(default)]
    pub attribution: Option<String>,
}

impl MapsConfig {
    /// The configured archive path, or `None` when maps are not enabled. An
    /// empty string is "not configured" (docs/09 §1 documents `pmtiles_path =
    /// ""` as the off state), not a path to the current directory.
    pub fn archive_path(&self) -> Option<&std::path::Path> {
        self.pmtiles_path
            .as_deref()
            .filter(|p| !p.as_os_str().is_empty())
    }

    /// The operator's attribution override, if it says anything.
    pub fn attribution_override(&self) -> Option<String> {
        self.attribution
            .as_ref()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }
}

/// `[timers]` (FR-33, ADR-023, docs/09 §1). Timers, alarms and reminders are
/// **on by default** — unlike every `[integrations.*]` section, which gates an
/// outward-facing capability. A timer reaches nothing outside this machine: it
/// reads a clock, writes a local row, and makes a noise. Requiring opt-in for
/// the most-used assistant feature would be strictness spent where there is no
/// exposure to reduce.
///
/// `alert_command` is the only thing here with any reach, and it is **owner
/// config (Z1)**: no timer name, reminder note, or model output is ever
/// interpolated into it (the WAV goes to the child's stdin, so there is no
/// argument to inject into either).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimersConfig {
    /// Set false to run with no timer surface at all: no routes, no scheduler
    /// task, nothing resident.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Playback command for the audible alert. Fed a WAV on stdin, so `aplay`,
    /// `ffplay -nodisp -autoexit -` and friends all work. A command that is not
    /// installed means the timer fires silently (logged) — never a failed fire.
    #[serde(default = "default_alert_command")]
    pub alert_command: String,
    /// Extra arguments for `alert_command`.
    #[serde(default)]
    pub alert_args: Vec<String>,
}

impl Default for TimersConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            alert_command: default_alert_command(),
            alert_args: Vec::new(),
        }
    }
}

/// `[lists]` (FR-34, ADR-024, docs/09 §1). Lists and quick notes are **on by
/// default**, for the same reason timers are: the whole module reaches nothing
/// outside this machine — it parses an utterance with a pure function and writes
/// a local row. There is nothing here to gate.
///
/// Nothing else is configurable on purpose. The item bound, the name-key
/// normalization and the promotion threshold are domain constants (ADR-024): a
/// deployment that could retune them would be a deployment where the grammar's
/// behaviour is not the same everywhere it is tested.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListsConfig {
    /// Set false to run with no list surface at all: no routes, nothing
    /// resident.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for ListsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_true() -> bool {
    true
}

fn default_alert_command() -> String {
    "paplay".to_owned()
}

/// `[integrations.media]` (FR-22, docs/02 §11a, ADR-012, docs/09 §1). Local
/// MPRIS transport control.
///
/// `enabled` defaults to **false**: media control is an ambient capability over
/// the session bus, and an unconfigured host should register no media tools and
/// expose no control surface (the same opt-in stance as every other
/// `[integrations.*]` section).
///
/// Two keys documented in docs/09 §1 are deliberately **not** implemented here,
/// because F3a.4 already shipped the mechanisms they would duplicate:
/// `media_window_app_id` (the app-id is the fixed `jarvis.media` from
/// `Surface::MediaWindow`, and the agent accepts only the `jarvis.` namespace)
/// and `default_display` (the media window is placed through the ordinary
/// display profile, `[display].profile.media_window`). Flagged for /sync-docs.
///
/// `max_volume_pct` is the hearing-protection cap. At or below it, a volume set
/// is R1 and auto-authorizes; above it requires an approved `media.volume_boost`
/// (R2) and is refused outright on the owner-driven REST surface. 70% is a
/// deliberately conservative default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_max_volume_pct")]
    pub max_volume_pct: u8,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_volume_pct: default_max_volume_pct(),
        }
    }
}

fn default_max_volume_pct() -> u8 {
    70
}

impl MediaConfig {
    /// The validated cap. An out-of-range value is a config error (fail fast at
    /// startup) rather than a silent clamp — a typo'd `max_volume_pct = 700`
    /// must not read as "no cap".
    pub fn max_volume(&self) -> anyhow::Result<jarvis_domain::media::VolumePct> {
        jarvis_domain::media::VolumePct::new(self.max_volume_pct)
            .map_err(|e| anyhow::anyhow!("[integrations.media].max_volume_pct: {e}"))
    }
}

/// `[display]` (docs/02 §8/§12, FR-09/10). The display profile: which monitor
/// each logical surface is pinned to. Keys are surface names in snake_case
/// (`artifact_canvas`, `conversation`, …); values are compositor connector names
/// (`DP-1`, `eDP-1`). Absent ⇒ an empty profile: placements must then name their
/// monitor explicitly (`POST …/open {display}`) or fail closed. Single-machine,
/// multi-monitor only in M3 (distributed nodes are M7).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayConfig {
    #[serde(default)]
    pub profile: std::collections::BTreeMap<String, String>,
    /// Room name → paired device id (F7.5, FR-19), e.g.
    /// `node_aliases = { kitchen = "01ARZ…" }`. This is the vocabulary the
    /// owner actually uses — "put it on the kitchen screen" — mapped to the
    /// device the pairing flow created. Same shape as
    /// `[integrations.spotify].device_aliases` (docs/02 §11).
    #[serde(default)]
    pub node_aliases: std::collections::BTreeMap<String, String>,
    /// Which paired device shows cast-a-link's media window (M7 gate D-M7-2).
    /// Unset keeps the pre-node behaviour — every presenter — which is safe
    /// with a single screen and is not once room nodes exist, because
    /// `media.open_url` is R1 and its URL can be influenced by untrusted
    /// content. A device id, or a room name from `node_aliases`.
    #[serde(default)]
    pub media_window_device: Option<String>,
}

/// `[storage]` (docs/04 §1, ADR-008). Root of the content-addressed artifact
/// blob store: manifests live in Postgres, blob bytes live here keyed by their
/// SHA-256. The directory is created lazily on first write, so it need not exist
/// at startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    #[serde(default = "default_artifacts_root")]
    pub artifacts_root: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            artifacts_root: default_artifacts_root(),
        }
    }
}

fn default_artifacts_root() -> PathBuf {
    PathBuf::from("/var/lib/jarvis/artifacts")
}

/// `[location]` (docs/02 §11c, ADR-015). The configured home coordinate — the
/// practical default "where" for a stationary desktop assistant, resolution
/// source #2. Both absent ⇒ no home source (device GPS / IP geolocation would
/// supply the coordinate, or "nearby" is sent without one — never guessed).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocationConfig {
    #[serde(default)]
    pub home_lat: Option<f64>,
    #[serde(default)]
    pub home_lon: Option<f64>,
}

impl LocationConfig {
    /// The configured home coordinate, if BOTH lat and lon are present and valid
    /// (a half-configured coordinate is rejected rather than paired with a
    /// defaulted 0.0). Range-checked so a typo cannot ship an off-globe location.
    pub fn home_coordinate(&self) -> Option<(f64, f64)> {
        match (self.home_lat, self.home_lon) {
            (Some(lat), Some(lon))
                if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) =>
            {
                Some((lat, lon))
            }
            _ => None,
        }
    }
}

/// Optional integrations (docs/09 §1 `[integrations.*]`). Each is absent by
/// default — an unconfigured integration registers no tools, the stricter
/// default (no ambient authority until the host opts in).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationsConfig {
    /// `[integrations.caldav]` (M4, FR-35, ADR-025). Read-only until a later
    /// milestone adds explicitly approved calendar mutations.
    #[serde(default)]
    pub caldav: CaldavConfig,
    /// `[integrations.web_search]`. Present ⇒ the `web.search`/`web.fetch` R0
    /// tools are registered against the live provider; absent ⇒ they are not,
    /// which is the external-egress consent gate (CF-5, docs/06 §5).
    #[serde(default)]
    pub web_search: Option<WebSearchConfig>,
    /// `[integrations.media]` (F3a.7, FR-22). Disabled by default ⇒ no media
    /// tools, no media routes, no session-bus connection.
    #[serde(default)]
    pub media: MediaConfig,
    /// `[integrations.smtp]` (M4, FR-36, ADR-026). Disabled by default so
    /// external message authority is never ambient.
    #[serde(default)]
    pub smtp: SmtpConfig,
    /// `[integrations.home_assistant]` (M5, FR-14, ADR-006). Disabled by
    /// default; authority over physical devices is never ambient.
    #[serde(default)]
    pub home_assistant: HomeAssistantConfig,
    /// `[integrations.spotify]` (M5, FR-21, ADR-012/022). Disabled by default.
    #[serde(default)]
    pub spotify: SpotifyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaldavConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub server_url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default = "default_caldav_password_secret")]
    pub password_secret: String,
}

fn default_caldav_password_secret() -> String {
    "keyring:jarvis/caldav".to_owned()
}

impl Default for CaldavConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_url: String::new(),
            username: String::new(),
            password_secret: default_caldav_password_secret(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmtpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default = "default_smtp_password_secret")]
    pub password_secret: String,
    #[serde(default)]
    pub from_address: String,
}

fn default_smtp_port() -> u16 {
    587
}

fn default_smtp_password_secret() -> String {
    "keyring:jarvis/smtp".to_owned()
}

impl Default for SmtpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: default_smtp_port(),
            username: String::new(),
            password_secret: default_smtp_password_secret(),
            from_address: String::new(),
        }
    }
}

/// `[integrations.home_assistant]` (M5 F5.3, FR-14, ADR-006, docs/02 §10).
///
/// Disabled by default, and the four allowlists are **empty** by default: an
/// enabled-but-unpopulated section controls nothing. That is deliberate —
/// HA is the one integration that changes *physical* state, so ambient
/// authority over it must never be the accident of turning a flag on.
///
/// `base_url` must be HTTPS: a long-lived bearer token is sent on every
/// request, and this adapter refuses to put it on the wire in clear text
/// (docs/06 §7). Many HA installs are plain HTTP on the LAN; that needs TLS
/// in front of HA rather than a quiet exception here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeAssistantConfig {
    #[serde(default)]
    pub enabled: bool,
    /// `https://…` base URL of the Home Assistant instance.
    #[serde(default)]
    pub base_url: String,
    /// Secret reference (`env:VAR`/`keyring:…`) resolving to a **dedicated,
    /// least-privilege** long-lived access token (docs/02 §10) — never the
    /// owner's primary credential.
    #[serde(default = "default_ha_token_secret")]
    pub token_secret: String,
    /// Entities `home.get_state` may read. Reading a presence or occupancy
    /// sensor is itself a privacy effect, so reads are allowlisted too.
    #[serde(default)]
    pub readable: Vec<String>,
    /// `light.*` entities `home.set_light` may switch (R1).
    #[serde(default)]
    pub lights: Vec<String>,
    /// `scene.*` entities `home.execute_scene` may run (R2, approval).
    #[serde(default)]
    pub scenes: Vec<String>,
    /// `script.*` entities `home.run_script` may run (R2, approval).
    #[serde(default)]
    pub scripts: Vec<String>,
}

fn default_ha_token_secret() -> String {
    "keyring:jarvis/home-assistant".to_owned()
}

impl Default for HomeAssistantConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            token_secret: default_ha_token_secret(),
            readable: Vec::new(),
            lights: Vec::new(),
            scenes: Vec::new(),
            scripts: Vec::new(),
        }
    }
}

/// `[integrations.spotify]` (M5 F5.6, FR-21, ADR-012, ADR-022, docs/02 §11a).
///
/// Disabled by default. The refresh token is a secret *reference*; minting the
/// first one (browser consent against [`jarvis_adapters::spotify::OAUTH_SCOPES`])
/// is an enrollment step outside this daemon. Scopes stay at playlist-*read* —
/// the adapter holds no library-mutation authority at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpotifyConfig {
    #[serde(default)]
    pub enabled: bool,
    /// OAuth client id (public by design in the PKCE flow — not a secret).
    #[serde(default)]
    pub client_id: String,
    /// Secret reference resolving to the OAuth **refresh** token.
    #[serde(default = "default_spotify_refresh_secret")]
    pub refresh_token_secret: String,
    /// Ceiling for `spotify.volume` (R1). Above it, only the separate
    /// `spotify.volume_boost` (R2, approval) applies — hearing protection is a
    /// real reversibility question (docs/02 §11a).
    #[serde(default = "default_spotify_max_volume")]
    pub max_volume_pct: u8,
    /// Optional ISO-3166-1 alpha-2 market for catalogue relevance.
    #[serde(default)]
    pub market: Option<String>,
    /// Friendly name → Spotify Connect device id, so a spoken "in the kitchen"
    /// resolves without the user reciting an opaque id.
    #[serde(default)]
    pub device_aliases: std::collections::BTreeMap<String, String>,
}

fn default_spotify_refresh_secret() -> String {
    "keyring:jarvis/spotify".to_owned()
}

fn default_spotify_max_volume() -> u8 {
    70
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            client_id: String::new(),
            refresh_token_secret: default_spotify_refresh_secret(),
            max_volume_pct: default_spotify_max_volume(),
            market: None,
            device_aliases: std::collections::BTreeMap::new(),
        }
    }
}

/// `[integrations.web_search]` (docs/02 §11b, ADR-014). The API key is a secret
/// *reference* resolved at the adapter boundary, never a literal in config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchConfig {
    /// Search provider — only `brave` is implemented in M2 (config-swappable).
    #[serde(default = "default_web_provider")]
    pub provider: String,
    /// Secret reference (`env:VAR`/`keyring:…`) resolving to the provider API key.
    pub api_key_secret: String,
    /// Max bytes read from a fetched page before truncation (docs/06 §5).
    #[serde(default = "default_max_fetch_bytes")]
    pub max_fetch_bytes: usize,
}

fn default_web_provider() -> String {
    "brave".to_owned()
}

fn default_max_fetch_bytes() -> usize {
    2 * 1024 * 1024
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Where the daemon listens. Loopback needs nothing further; **any other
    /// address requires TLS** and is refused without it (docs/06 §7, F7.3).
    pub bind: String,
    /// Static Angular assets; optional until packaging serves them.
    pub web_assets: Option<PathBuf>,
    /// TLS for LAN/remote nodes (F7.3, ADR-031). Absent = plaintext, which is
    /// only legal on loopback.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

/// `[server.tls]` — the certificate a node pins at pairing (ADR-031).
///
/// Both paths are required together: a certificate without its key cannot
/// serve, and a key without its certificate cannot be pinned. Self-signed is
/// the expected case — there is no CA in a house, and the fingerprint handed
/// to the node during the pairing ceremony is what makes the certificate
/// meaningful.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Secret *reference* (`env:VAR` or `keyring:service/entry`) resolving to
    /// the postgres URL. Literal URLs are rejected at validation.
    pub url_secret: String,
    pub max_connections: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservabilityConfig {
    /// OTLP gRPC endpoint. Off by default — the collector runs only while
    /// actively debugging (docs/09 §5); spans still go to the journal.
    pub otlp_endpoint: Option<String>,
}

/// Model/embedding provider configuration (docs/09 §1 `[providers.*]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    #[serde(rename = "claude-cli")]
    pub claude_cli: ClaudeCliConfig,
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
}

/// `[providers.embeddings]` (M4, docs/09 §1/§5). The model is CPU-only and
/// loaded lazily by the adapter; these values are references/bounds, never
/// prompt content or secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingsConfig {
    #[serde(default = "default_embedding_model")]
    pub model: String,
    #[serde(default = "default_embedding_cache_dir")]
    pub cache_dir: PathBuf,
    #[serde(default = "default_embedding_idle_unload_secs")]
    pub idle_unload_secs: u64,
    #[serde(default = "default_embedding_threads")]
    pub intra_threads: usize,
}

fn default_embedding_model() -> String {
    "bge-small-en-v1.5".to_owned()
}

fn default_embedding_cache_dir() -> PathBuf {
    PathBuf::from("/var/lib/jarvis/models")
}

fn default_embedding_idle_unload_secs() -> u64 {
    600
}

fn default_embedding_threads() -> usize {
    2
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            model: default_embedding_model(),
            cache_dir: default_embedding_cache_dir(),
            idle_unload_secs: default_embedding_idle_unload_secs(),
            intra_threads: default_embedding_threads(),
        }
    }
}

impl EmbeddingsConfig {
    pub fn to_adapter(&self) -> jarvis_adapters::embeddings::FastEmbedConfig {
        jarvis_adapters::embeddings::FastEmbedConfig {
            model: self.model.clone(),
            cache_dir: self.cache_dir.clone(),
            intra_threads: self.intra_threads,
            idle_unload_secs: self.idle_unload_secs,
        }
    }
}

/// `[providers.claude-cli]` (docs/09 §1, ADR-004). The reasoning-profile CLI
/// adapter's spawn contract: binary, controlled workdir, built-in tools disabled.
///
/// Unknown keys are tolerated (no `deny_unknown_fields`) because docs/09 §1
/// documents the full block — `enabled`, `timeout_secs`, `single_flight`,
/// `backoff_initial_secs`, `backoff_max_secs` — but those are host-level health
/// /single-flight concerns wired in F1.7, not the adapter's spawn contract. They
/// are modelled here when that wiring lands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeCliConfig {
    /// The CLI binary, resolved on the PATH of the service user.
    pub binary: String,
    /// Controlled working directory the process is spawned in (ADR-004).
    pub workdir: PathBuf,
    /// Reasoning profile disables the CLI's built-in tools — Jarvis tools are the
    /// only action path (invariant 1, ADR-004/014).
    pub reasoning_disable_builtin_tools: bool,
    /// Idle read timeout in seconds: no event within this window ⇒ unhealthy.
    pub idle_timeout_secs: u64,
}

impl ClaudeCliConfig {
    /// Map to the adapter's spawn config (`jarvis-adapters`). Kept here so the
    /// adapter never depends on the host's config types.
    pub fn to_adapter(&self) -> jarvis_adapters::claude_cli::ClaudeCliConfig {
        jarvis_adapters::claude_cli::ClaudeCliConfig {
            binary: self.binary.clone(),
            workdir: self.workdir.clone(),
            disable_builtin_tools: self.reasoning_disable_builtin_tools,
            idle_timeout: std::time::Duration::from_secs(self.idle_timeout_secs),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                bind: "127.0.0.1:8741".into(),
                web_assets: None,
                tls: None,
            },
            database: DatabaseConfig {
                url_secret: "env:JARVIS_DB_URL".into(),
                max_connections: 8,
            },
            observability: ObservabilityConfig {
                otlp_endpoint: None,
            },
            providers: ProvidersConfig {
                // Mirrors the documented `[providers.claude-cli]` defaults (docs/09 §1).
                claude_cli: ClaudeCliConfig {
                    binary: "claude".into(),
                    workdir: PathBuf::from("/var/lib/jarvis/claude-work"),
                    reasoning_disable_builtin_tools: true,
                    idle_timeout_secs: 60,
                },
                embeddings: EmbeddingsConfig::default(),
            },
            integrations: IntegrationsConfig::default(),
            location: LocationConfig::default(),
            storage: StorageConfig::default(),
            display: DisplayConfig::default(),
            maps: MapsConfig::default(),
            ui: UiConfig::default(),
            voice: VoiceConfig::default(),
            timers: TimersConfig::default(),
            lists: ListsConfig::default(),
            apps: AppsConfig::default(),
        }
    }
}

impl Config {
    /// Standard layering (docs/09 §1). Missing files are fine; env wins.
    pub fn load() -> anyhow::Result<Self> {
        // Defaults are layered exclusively by from_figment; this builds only
        // the file/env layers on top.
        let mut figment = Figment::new().merge(Toml::file("/etc/jarvis/jarvisd.toml"));
        if let Some(home) = std::env::var_os("HOME") {
            figment = figment.merge(Toml::file(
                PathBuf::from(home).join(".config/jarvis/jarvisd.toml"),
            ));
        }
        Self::from_figment(figment.merge(Env::prefixed("JARVIS__").split("__")))
    }

    pub fn from_figment(figment: Figment) -> anyhow::Result<Self> {
        let figment = Figment::from(Serialized::defaults(Config::default())).merge(figment);
        let config: Config = figment.extract()?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        let addr: SocketAddr = self.server.bind.parse().map_err(|e| {
            anyhow::anyhow!(
                "server.bind {:?} is not a socket address: {e}",
                self.server.bind
            )
        })?;
        // F7.3 (docs/06 §7): loopback may stay plaintext; anything reachable
        // from the network may not. Fail closed at startup with no override —
        // a daemon that serves device tokens in the clear on a LAN is the one
        // configuration mistake with no recovery, because the credential is
        // gone the moment it is used.
        match (&self.server.tls, addr.ip().is_loopback()) {
            (None, true) => {}
            (None, false) => anyhow::bail!(
                "server.bind {addr} is not loopback and [server.tls] is not configured — \
                 jarvisd refuses to serve device tokens in the clear off loopback \
                 (docs/06 §7). Configure [server.tls] cert_path/key_path, or bind loopback."
            ),
            (Some(tls), _) => {
                for (field, path) in [
                    ("[server.tls].cert_path", &tls.cert_path),
                    ("[server.tls].key_path", &tls.key_path),
                ] {
                    anyhow::ensure!(
                        path.is_absolute(),
                        "{field} {} must be absolute — a relative path resolves against \
                         whatever directory the service happened to start in",
                        path.display()
                    );
                }
            }
        }
        validate_secret_ref(&self.database.url_secret)?;
        // A relative map path would resolve against whatever directory the
        // service happens to start in — fail fast at config time rather than
        // "no such file" at startup or, worse, a different file after a
        // working-directory change.
        if let Some(path) = self.maps.archive_path() {
            anyhow::ensure!(
                path.is_absolute(),
                "[maps].pmtiles_path {} must be an absolute path",
                path.display()
            );
        }
        // Half-configured voice (F8.9). These are the states people actually
        // reach on a first install, and each one fails *later* and less
        // legibly than it does here: a daemon that starts and then cannot
        // speak looks like broken hardware, not a missing line of config.
        if self.voice.elevenlabs.enabled {
            anyhow::ensure!(
                self.voice.enabled,
                "[voice.elevenlabs] is enabled but [voice].enabled is false — \
                 there is no voice pipeline for it to speak through"
            );
            anyhow::ensure!(
                self.voice.wyoming_tts.is_some(),
                "[voice.elevenlabs] is enabled but [voice].wyoming_tts is unset — \
                 there would be no local voice to fall back to, and an alarm must \
                 sound even when the network is down (ADR-023, ADR-033 §3)"
            );
            let api_key_ref = self
                .voice
                .elevenlabs
                .api_key_ref
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("[voice.elevenlabs].api_key_ref is required when it is enabled")
                })?;
            // A literal key here would be a secret in a config file
            // (invariant 5). Refused at load, not at first use.
            validate_secret_ref_named("[voice.elevenlabs].api_key_ref", api_key_ref)?;
            anyhow::ensure!(
                self.voice
                    .elevenlabs
                    .voice_id
                    .as_deref()
                    .is_some_and(|id| !id.trim().is_empty()),
                "[voice.elevenlabs].voice_id is required when it is enabled"
            );
            anyhow::ensure!(
                self.voice.elevenlabs.character_budget > 0,
                "[voice.elevenlabs].character_budget must be greater than zero — \
                 a zero budget silently routes every utterance to the local voice, \
                 which is a confusing way to spell `enabled = false`"
            );
        }

        if self.integrations.smtp.enabled {
            anyhow::ensure!(
                !self.integrations.smtp.host.trim().is_empty(),
                "[integrations.smtp].host is required when SMTP is enabled"
            );
            anyhow::ensure!(
                !self.integrations.smtp.from_address.trim().is_empty(),
                "[integrations.smtp].from_address is required when SMTP is enabled"
            );
            validate_secret_ref_named(
                "[integrations.smtp].password_secret",
                &self.integrations.smtp.password_secret,
            )?;
        }
        if self.integrations.caldav.enabled {
            anyhow::ensure!(
                !self.integrations.caldav.server_url.trim().is_empty(),
                "[integrations.caldav].server_url is required when CalDAV is enabled"
            );
            validate_secret_ref_named(
                "[integrations.caldav].password_secret",
                &self.integrations.caldav.password_secret,
            )?;
        }
        if self.voice.enabled {
            anyhow::ensure!(
                self.voice.wyoming_stt.starts_with("tcp://"),
                "[voice].wyoming_stt must use tcp:// when voice is enabled"
            );
            anyhow::ensure!(
                self.voice
                    .wyoming_tts
                    .as_ref()
                    .is_none_or(|tts| tts.starts_with("tcp://")),
                "[voice].wyoming_tts must use tcp:// when set"
            );
            anyhow::ensure!(
                self.voice.audio.sample_rate > 0 && self.voice.audio.channels > 0,
                "[voice].audio sample_rate and channels must be positive"
            );
            anyhow::ensure!(
                self.voice.audio.format == "s16le",
                "[voice].audio.format must be s16le"
            );
        }
        if self.integrations.home_assistant.enabled {
            // HTTPS is not negotiable here: a least-privilege but still
            // powerful bearer token rides on every request (docs/06 §7).
            anyhow::ensure!(
                self.integrations
                    .home_assistant
                    .base_url
                    .starts_with("https://"),
                "[integrations.home_assistant].base_url must be https:// — the access token \
                 is sent on every request and must not cross the network in clear text; \
                 put TLS in front of Home Assistant rather than relaxing this"
            );
            validate_secret_ref_named(
                "[integrations.home_assistant].token_secret",
                &self.integrations.home_assistant.token_secret,
            )?;
        }
        if self.integrations.spotify.enabled {
            anyhow::ensure!(
                !self.integrations.spotify.client_id.trim().is_empty(),
                "[integrations.spotify].client_id is required when Spotify is enabled"
            );
            anyhow::ensure!(
                (1..=100).contains(&self.integrations.spotify.max_volume_pct),
                "[integrations.spotify].max_volume_pct must be 1..=100"
            );
            validate_secret_ref_named(
                "[integrations.spotify].refresh_token_secret",
                &self.integrations.spotify.refresh_token_secret,
            )?;
        }
        Ok(())
    }

    pub fn bind_addr(&self) -> SocketAddr {
        self.server.bind.parse().expect("validated at construction")
    }
}

fn validate_secret_ref_named(field: &str, reference: &str) -> anyhow::Result<()> {
    // NEVER echo the rejected value: the failing case is precisely "someone
    // pasted a literal secret", and this error reaches stderr/journald.
    anyhow::ensure!(
        reference.starts_with("env:") || reference.starts_with("keyring:"),
        "{field} (scheme {:?}) is not a secret reference — secrets must be \
         `env:VAR` or `keyring:service/entry` references, never literal values \
         (invariant 5); the rejected value is withheld from this message",
        scheme_of(reference)
    );
    Ok(())
}

fn validate_secret_ref(reference: &str) -> anyhow::Result<()> {
    validate_secret_ref_named("database.url_secret", reference)
}

/// The scheme part of a reference — safe to echo; never the remainder.
///
/// A value with **no** `:` at all has no scheme, and returning "everything
/// before the first `:`" would then return the whole value. That is precisely
/// the case this function exists to protect (someone pasted a raw token or
/// password), so it must not be echoed: an opaque marker goes out instead.
///
/// A `:` alone is not enough either: a pasted literal password may well contain
/// one (`Summer:2026!`), and echoing "everything before the first colon" then
/// leaks a usable prefix of the secret into stderr/journald — the same leak in
/// smaller print (invariant 5).
///
/// Shape alone does not separate the two cases: `Summer` is a perfectly
/// well-formed RFC 3986 scheme. What separates them is *provenance* — so the
/// echo is drawn from a fixed, code-owned list of scheme names rather than from
/// the rejected value. A candidate that matches one of those names carries no
/// information the operator did not already read in this file; anything else is
/// withheld wholesale. The message stays actionable regardless, because the two
/// facts an operator needs — which field, and which schemes are accepted — are
/// always spelled out and never come from the value.
const WITHHELD_SCHEME: &str = "<withheld>";

/// Scheme names safe to repeat, because they *are* these literals: the two
/// Jarvis accepts (echoed when only the shape after the colon is wrong), plus
/// the near-misses an operator plausibly types into a `*_secret` field. A scheme
/// outside this list is not "unknown but harmless" — it may be the first half of
/// a password, so it is withheld.
const ECHOABLE_SCHEMES: &[&str] = &[
    "env",
    "keyring",
    "file",
    "vault",
    "secret",
    "op",
    "http",
    "https",
    "postgres",
    "postgresql",
];

fn scheme_of(reference: &str) -> &str {
    match reference.split_once(':') {
        Some((scheme, _)) => ECHOABLE_SCHEMES
            .iter()
            .find(|known| scheme.eq_ignore_ascii_case(known))
            .copied()
            .unwrap_or(WITHHELD_SCHEME),
        None => WITHHELD_SCHEME,
    }
}

/// Resolve a secret reference at the adapter boundary. The value comes back
/// [`Redacted`] so it cannot reach logs or serialization by accident.
pub fn resolve_secret_ref(reference: &str) -> anyhow::Result<Redacted<String>> {
    resolve_secret_ref_with(reference, |var| std::env::var(var).ok())
}

/// Resolve a secret without blocking an async runtime worker.
///
/// keyring's pure-Rust Secret Service backend presents the crate's synchronous
/// [`keyring::Entry`] facade by driving async D-Bus work internally. Its own
/// contract requires those calls to run on a separate thread when the caller
/// already owns a Tokio runtime; doing otherwise can deadlock during startup.
/// The task is awaited, so startup neither detaches work nor outlives a lookup.
pub async fn resolve_secret_ref_async(reference: &str) -> anyhow::Result<Redacted<String>> {
    let reference = reference.to_owned();
    tokio::task::spawn_blocking(move || resolve_secret_ref(&reference))
        .await
        .map_err(|_| anyhow::anyhow!("secret resolution task failed"))?
}

/// Injectable-lookup variant so tests never mutate process-global env
/// (`std::env::set_var` is `unsafe` in Rust 2024 and stays banned here).
pub fn resolve_secret_ref_with(
    reference: &str,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<Redacted<String>> {
    if let Some(var) = reference.strip_prefix("env:") {
        let value = env_lookup(var).ok_or_else(|| {
            anyhow::anyhow!("secret reference {reference:?}: environment variable {var} is not set")
        })?;
        Ok(Redacted::new(value))
    } else if reference.starts_with("keyring:") {
        let key = reference
            .strip_prefix("keyring:")
            .and_then(|value| value.split_once('/'))
            .filter(|(service, entry)| !service.is_empty() && !entry.is_empty())
            .ok_or_else(|| anyhow::anyhow!("keyring reference has invalid shape"))?;
        let entry = keyring::Entry::new(key.0, key.1)
            .map_err(|_| anyhow::anyhow!("keyring reference could not be opened"))?;
        let value = entry
            .get_password()
            .map_err(|_| anyhow::anyhow!("keyring reference could not be resolved"))?;
        Ok(Redacted::new(value))
    } else {
        // Same rule as validate_secret_ref: the value may BE a secret.
        anyhow::bail!(
            "secret reference with scheme {:?} is not supported (env: or keyring:); \
             the value is withheld from this message",
            scheme_of(reference)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- half-configured states a first install actually reaches (F8.9) ----

    /// Builds a config from TOML the way an operator's file would load.
    fn load(toml: &str) -> anyhow::Result<Config> {
        Config::from_figment(Figment::new().merge(Toml::string(toml)))
    }

    #[test]
    fn elevenlabs_without_a_voice_pipeline_is_refused() {
        let error = load(
            r#"
            [voice]
            enabled = false
            [voice.elevenlabs]
            enabled = true
            api_key_ref = "keyring:jarvis/elevenlabs"
            voice_id = "abc"
            "#,
        )
        .expect_err("must refuse");
        assert!(
            error.to_string().contains("no voice pipeline"),
            "unexpected: {error}"
        );
    }

    /// The one that matters most: enabling a cloud voice with no local voice
    /// underneath would make an internet outage a mute house, and would let a
    /// failed alarm be silent (ADR-023, ADR-033 §3).
    #[test]
    fn elevenlabs_without_a_local_voice_to_fall_back_to_is_refused() {
        let error = load(
            r#"
            [voice]
            enabled = true
            [voice.elevenlabs]
            enabled = true
            api_key_ref = "keyring:jarvis/elevenlabs"
            voice_id = "abc"
            "#,
        )
        .expect_err("must refuse");
        assert!(
            error.to_string().contains("fall back"),
            "unexpected: {error}"
        );
    }

    #[test]
    fn a_literal_elevenlabs_key_in_config_is_refused() {
        let error = load(
            r#"
            [voice]
            enabled = true
            wyoming_tts = "tcp://127.0.0.1:10200"
            [voice.elevenlabs]
            enabled = true
            api_key_ref = "sk_a_real_looking_key"
            voice_id = "abc"
            "#,
        )
        .expect_err("a secret must never be a literal in config (invariant 5)");
        // The message must not echo the value back into the operator's terminal.
        assert!(!error.to_string().contains("sk_a_real_looking_key"));
    }

    #[test]
    fn elevenlabs_missing_its_voice_id_or_budget_is_refused() {
        let base = |extra: &str| {
            format!(
                r#"
                [voice]
                enabled = true
                wyoming_tts = "tcp://127.0.0.1:10200"
                [voice.elevenlabs]
                enabled = true
                api_key_ref = "keyring:jarvis/elevenlabs"
                {extra}
                "#
            )
        };
        assert!(load(&base("")).is_err(), "no voice_id");
        assert!(
            load(&base("voice_id = \"abc\"\ncharacter_budget = 0"))
                .expect_err("zero budget")
                .to_string()
                .contains("greater than zero")
        );
        // …and the fully configured version is accepted.
        assert!(load(&base("voice_id = \"abc\"")).is_ok());
    }

    /// A disabled block is never validated: an operator leaving a half-filled
    /// `[voice.elevenlabs]` behind with `enabled = false` must still be able to
    /// start the daemon.
    #[test]
    fn a_disabled_elevenlabs_block_is_not_validated() {
        assert!(
            load(
                r#"
                [voice.elevenlabs]
                enabled = false
                voice_id = ""
                character_budget = 0
                "#,
            )
            .is_ok()
        );
    }

    #[test]
    fn defaults_carry_the_documented_claude_cli_config() {
        let config = Config::from_figment(Figment::new()).expect("defaults are valid");
        let cli = config.providers.claude_cli;
        assert_eq!(cli.binary, "claude");
        assert_eq!(cli.workdir, PathBuf::from("/var/lib/jarvis/claude-work"));
        assert!(cli.reasoning_disable_builtin_tools);
        assert_eq!(cli.idle_timeout_secs, 60);
    }

    #[test]
    fn the_ui_section_defaults_to_the_documented_values() {
        // docs/09 §1 `[ui]`.
        let config = Config::from_figment(Figment::new()).expect("defaults are valid");
        assert_eq!(config.ui.background, "none");
        assert_eq!(config.ui.panel_ttl_hours, 2);
        assert_eq!(config.ui.deepdive_promote_after, 3);
        assert_eq!(config.ui.motion, "auto");
    }

    #[test]
    fn the_documented_ui_block_is_accepted_verbatim() {
        // `Config` denies unknown fields, so an operator pasting the block from
        // docs/09 §1 must parse — every documented key is modelled.
        let figment = Figment::new().merge(Toml::string(
            r#"
            [ui]
            background = "photo"
            background_photo = "/var/lib/jarvis/wall.jpg"
            panel_ttl_hours = 4
            deepdive_promote_after = 0
            motion = "reduced"
            "#,
        ));
        let config = Config::from_figment(figment).expect("the documented block parses");
        assert_eq!(config.ui.background, "photo");
        // Zero is the documented "never offer" setting, not an invalid value.
        assert_eq!(config.ui.deepdive_promote_after, 0);
        assert_eq!(config.ui.panel_ttl_hours, 4);
    }

    #[test]
    fn maps_are_off_by_default_and_an_empty_path_stays_off() {
        // The safe default: no archive ⇒ no map endpoints at all (F3b.5).
        let config = Config::from_figment(Figment::new()).expect("defaults are valid");
        assert!(config.maps.archive_path().is_none());
        assert!(config.maps.attribution_override().is_none());

        // docs/09 §1 documents `pmtiles_path = ""` as the off state — it must not
        // read as a path to the working directory.
        let config = Config::from_figment(
            Figment::new().merge(Toml::string("[maps]\npmtiles_path = \"\"\n")),
        )
        .expect("an empty path is valid config");
        assert!(config.maps.archive_path().is_none());
    }

    #[test]
    fn a_relative_map_archive_path_is_rejected_at_startup() {
        // A relative path would resolve against whatever directory the service
        // started in — fail fast rather than serve a different file later.
        let error = Config::from_figment(Figment::new().merge(Toml::string(
            "[maps]\npmtiles_path = \"maps/region.pmtiles\"\n",
        )))
        .expect_err("a relative archive path must be refused");
        assert!(
            error.to_string().contains("absolute"),
            "unexpected error: {error}"
        );

        let config = Config::from_figment(Figment::new().merge(Toml::string(
            "[maps]\npmtiles_path = \"/var/lib/jarvis/maps/region.pmtiles\"\nattribution = \"  \"\n",
        )))
        .expect("an absolute archive path is valid");
        assert_eq!(
            config.maps.archive_path(),
            Some(std::path::Path::new("/var/lib/jarvis/maps/region.pmtiles"))
        );
        // A blank override is no override — the archive/default attribution wins.
        assert!(config.maps.attribution_override().is_none());
    }

    #[test]
    fn kebab_section_overrides_and_tolerates_unwired_f17_keys() {
        // `[providers.claude-cli]` is kebab-cased in TOML (docs/09 §1); the
        // still-unwired F1.7 keys (`timeout_secs`, `single_flight`, `backoff_*`)
        // must not fail the parse.
        let toml = r#"
            [providers.claude-cli]
            binary = "claude-test"
            workdir = "/tmp/jarvis-work"
            reasoning_disable_builtin_tools = false
            idle_timeout_secs = 90
            timeout_secs = 300
            single_flight = true
            backoff_initial_secs = 30
        "#;
        let config = Config::from_figment(Figment::new().merge(Toml::string(toml)))
            .expect("documented block parses");
        let adapter = config.providers.claude_cli.to_adapter();
        assert_eq!(adapter.binary, "claude-test");
        assert_eq!(adapter.workdir, PathBuf::from("/tmp/jarvis-work"));
        assert!(!adapter.disable_builtin_tools);
        assert_eq!(adapter.idle_timeout, std::time::Duration::from_secs(90));
    }

    /// **F7.3, the rule with no override (docs/06 §7).** A daemon that serves
    /// device tokens in the clear on a LAN is the one configuration mistake
    /// with no recovery — the credential is gone the moment it is used — so
    /// the refusal happens at startup, not at first request.
    #[test]
    fn a_non_loopback_bind_without_tls_refuses_to_start() {
        let figment = Figment::new().merge(figment::providers::Serialized::defaults(
            serde_json::json!({ "server": { "bind": "0.0.0.0:8080" } }),
        ));
        let error = Config::from_figment(figment)
            .expect_err("a public bind without TLS must not start")
            .to_string();
        assert!(
            error.contains("server.tls"),
            "the error must name the fix: {error}"
        );

        // The same bind IS allowed once TLS is configured.
        let figment = Figment::new().merge(figment::providers::Serialized::defaults(
            serde_json::json!({
                "server": {
                    "bind": "0.0.0.0:8080",
                    "tls": { "cert_path": "/etc/jarvis/cert.pem", "key_path": "/etc/jarvis/key.pem" }
                }
            }),
        ));
        Config::from_figment(figment).expect("a TLS-configured public bind is legal");
    }

    #[test]
    fn loopback_still_needs_no_tls_and_tls_paths_must_be_absolute() {
        let figment = Figment::new().merge(figment::providers::Serialized::defaults(
            serde_json::json!({ "server": { "bind": "127.0.0.1:8080" } }),
        ));
        Config::from_figment(figment).expect("loopback plaintext is the M0–M6 shape");

        for bad in ["cert.pem", "./certs/cert.pem"] {
            let figment = Figment::new().merge(figment::providers::Serialized::defaults(
                serde_json::json!({
                    "server": {
                        "bind": "127.0.0.1:8080",
                        "tls": { "cert_path": bad, "key_path": "/etc/jarvis/key.pem" }
                    }
                }),
            ));
            let error = Config::from_figment(figment)
                .expect_err("a relative TLS path must be refused")
                .to_string();
            assert!(error.contains("absolute"), "{error}");
        }
    }
}
