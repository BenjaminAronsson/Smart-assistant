use serde::{Deserialize, Serialize};

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
