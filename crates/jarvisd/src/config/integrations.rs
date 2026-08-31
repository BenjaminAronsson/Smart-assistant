use serde::{Deserialize, Serialize};

use super::media::MediaConfig;

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
