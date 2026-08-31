use serde::{Deserialize, Serialize};

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
