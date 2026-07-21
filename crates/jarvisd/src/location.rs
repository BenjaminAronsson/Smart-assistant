//! Location-provider composition (F2.9, ADR-015, docs/02 §11c). Assembles the
//! layered [`LocationProvider`] from configured sources in the ADR-015 order:
//! paired-device GPS → configured home coordinate → IP geolocation.
//!
//! M2 wires only the **home coordinate** source (from `[location]`): device GPS
//! needs a paired mobile client (M7) and live IP geolocation is a thin follow-up
//! adapter. The layered provider degrades correctly — with no configured source
//! it resolves to `None`, so a "nearby" query is sent without a coordinate rather
//! than guessing one (ADR-015). The resolved provider is consumed by the
//! location-aware search flow (orchestrator wiring lands with the F2.11 golden
//! traces, where a fixed fake coordinate proves "lunch nearby" localizes).

use std::sync::Arc;

use jarvis_application::location::{
    FixedLocationProvider, LayeredLocationProvider, LocationProvider,
};
use jarvis_domain::location::{Location, LocationSource};

use crate::config::LocationConfig;

/// Build the layered location provider from `[location]` config. Returns `None`
/// when no source is configured (M2: only the home coordinate) — the caller then
/// resolves "nearby" without a coordinate rather than fabricating one.
pub fn build_location_provider(config: &LocationConfig) -> Option<Arc<dyn LocationProvider>> {
    let mut sources: Vec<Arc<dyn LocationProvider>> = Vec::new();

    // Source #1 (device GPS) and #3 (IP geolocation) are not wired in M2.
    // Source #2: the configured home coordinate.
    if let Some((lat, lon)) = config.home_coordinate() {
        sources.push(Arc::new(FixedLocationProvider::new(Location::new(
            lat,
            lon,
            LocationSource::HomeCoordinate,
        ))));
    }

    if sources.is_empty() {
        None
    } else {
        Some(Arc::new(LayeredLocationProvider::new(sources)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn resolves_the_configured_home_coordinate() {
        let config = LocationConfig {
            home_lat: Some(59.3293),
            home_lon: Some(18.0686),
        };
        let provider = build_location_provider(&config).expect("home coordinate configured");
        let resolved = provider.resolve(CancellationToken::new()).await.unwrap();
        assert_eq!(resolved.source, LocationSource::HomeCoordinate);
        assert!((resolved.latitude - 59.3293).abs() < 1e-9);
    }

    #[test]
    fn no_configured_source_yields_no_provider() {
        assert!(build_location_provider(&LocationConfig::default()).is_none());
        // A half-configured coordinate is not a source (rejected by the config).
        let half = LocationConfig {
            home_lat: Some(59.0),
            home_lon: None,
        };
        assert!(build_location_provider(&half).is_none());
        // An out-of-range coordinate is rejected too.
        let bad = LocationConfig {
            home_lat: Some(999.0),
            home_lon: Some(0.0),
        };
        assert!(build_location_provider(&bad).is_none());
    }
}
