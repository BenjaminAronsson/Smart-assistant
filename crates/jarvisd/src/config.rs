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
