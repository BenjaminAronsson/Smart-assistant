use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
