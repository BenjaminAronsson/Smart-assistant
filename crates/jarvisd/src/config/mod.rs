//! Layered configuration (docs/09 §1): file → env (`JARVIS__…`) → secret
//! references. Validated at startup; invalid config fails fast with a precise
//! error. Secrets are references (`env:` / `keyring:`), never values —
//! CLAUDE.md invariant 5.

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

mod apps;
mod database;
mod display;
mod integrations;
mod lists;
mod location;
mod maps;
mod media;
mod observability;
mod providers;
mod secrets;
mod server;
mod storage;
mod timers;
mod ui;
mod voice;

pub use apps::*;
pub use database::*;
pub use display::*;
pub use integrations::*;
pub use lists::*;
pub use location::*;
pub use maps::*;
pub use media::*;
pub use observability::*;
pub use providers::*;
pub use secrets::*;
pub use server::*;
pub use storage::*;
pub use timers::*;
pub use ui::*;
pub use voice::*;

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

#[cfg(test)]
mod tests;
