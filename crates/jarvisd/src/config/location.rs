use serde::{Deserialize, Serialize};

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
