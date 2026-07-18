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
