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
    pub timers: TimersConfig,
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
    /// `[integrations.web_search]`. Present ⇒ the `web.search`/`web.fetch` R0
    /// tools are registered against the live provider; absent ⇒ they are not,
    /// which is the external-egress consent gate (CF-5, docs/06 §5).
    #[serde(default)]
    pub web_search: Option<WebSearchConfig>,
    /// `[integrations.media]` (F3a.7, FR-22). Disabled by default ⇒ no media
    /// tools, no media routes, no session-bus connection.
    #[serde(default)]
    pub media: MediaConfig,
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
    /// Loopback only for M0–M2 (docs/06 §7); validation enforces it.
    pub bind: String,
    /// Static Angular assets; optional until packaging serves them.
    pub web_assets: Option<PathBuf>,
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
            },
            integrations: IntegrationsConfig::default(),
            location: LocationConfig::default(),
            storage: StorageConfig::default(),
            display: DisplayConfig::default(),
            maps: MapsConfig::default(),
            timers: TimersConfig::default(),
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
        anyhow::ensure!(
            addr.ip().is_loopback(),
            "server.bind {addr} is not loopback — jarvisd binds loopback only until M7 \
             remote nodes exist (docs/06 §7)"
        );
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
        Ok(())
    }

    pub fn bind_addr(&self) -> SocketAddr {
        self.server.bind.parse().expect("validated at construction")
    }
}

fn validate_secret_ref(reference: &str) -> anyhow::Result<()> {
    // NEVER echo the rejected value: the failing case is precisely "someone
    // pasted a literal secret", and this error reaches stderr/journald.
    anyhow::ensure!(
        reference.starts_with("env:") || reference.starts_with("keyring:"),
        "database.url_secret (scheme {:?}) is not a secret reference — secrets must be \
         `env:VAR` or `keyring:service/entry` references, never literal values \
         (invariant 5); the rejected value is withheld from this message",
        scheme_of(reference)
    );
    Ok(())
}

/// Everything before the first `:` — safe to echo; never the remainder.
fn scheme_of(reference: &str) -> &str {
    reference.split(':').next().unwrap_or_default()
}

/// Resolve a secret reference at the adapter boundary. The value comes back
/// [`Redacted`] so it cannot reach logs or serialization by accident.
pub fn resolve_secret_ref(reference: &str) -> anyhow::Result<Redacted<String>> {
    resolve_secret_ref_with(reference, |var| std::env::var(var).ok())
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
        anyhow::bail!(
            "secret reference {reference:?}: keyring resolution is not yet available \
             (lands with packaging) — use an env: reference in dev"
        )
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
}
