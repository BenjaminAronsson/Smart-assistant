use serde::{Deserialize, Serialize};

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
