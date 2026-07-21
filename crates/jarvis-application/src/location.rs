//! Location resolution (F2.9, FR-26, ADR-015). The [`LocationProvider`] port
//! yields a resolved [`Location`] (or `None` when nothing knows where we are);
//! [`LayeredLocationProvider`] implements the ADR-015 resolution order — paired
//! device GPS → configured home coordinate → IP geolocation — by trying an
//! ordered list of sources and returning the first hit.
//!
//! The port lives in the application layer (pure): concrete sources (a home
//! coordinate from config, live IP geolocation) are adapters. Everything is
//! cancellable (invariant #4). A resolved location is sensitive (NFR-02); this
//! layer only *resolves* it — attaching it to a search query and labelling it
//! for the context assembler is the caller's job (`localize_query`).

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use jarvis_domain::location::Location;

/// A single source of a device/user location. Returns `None` when this source
/// cannot supply one right now (no GPS fix, scope not granted, lookup failed) —
/// a source never guesses (ADR-015). `cancel` aborts an in-flight lookup.
#[async_trait]
pub trait LocationProvider: Send + Sync {
    async fn resolve(&self, cancel: CancellationToken) -> Option<Location>;
}

/// Resolves a location by trying an ordered list of [`LocationProvider`]s and
/// returning the first that yields one — the ADR-015 order when constructed as
/// `[device_gps, home_coordinate, ip_geolocation]`. An empty list (nothing
/// configured) resolves to `None`: "nearby" is then sent without a coordinate
/// rather than guessed.
pub struct LayeredLocationProvider {
    sources: Vec<Arc<dyn LocationProvider>>,
}

impl LayeredLocationProvider {
    pub fn new(sources: Vec<Arc<dyn LocationProvider>>) -> Self {
        Self { sources }
    }
}

#[async_trait]
impl LocationProvider for LayeredLocationProvider {
    async fn resolve(&self, cancel: CancellationToken) -> Option<Location> {
        for source in &self.sources {
            if cancel.is_cancelled() {
                return None;
            }
            if let Some(location) = source.resolve(cancel.clone()).await {
                return Some(location);
            }
        }
        None
    }
}

/// A source that always yields a fixed location (the configured home coordinate
/// — ADR-015 source #2). Pure; the config wiring lives in jarvisd.
pub struct FixedLocationProvider {
    location: Location,
}

impl FixedLocationProvider {
    pub fn new(location: Location) -> Self {
        Self { location }
    }
}

#[async_trait]
impl LocationProvider for FixedLocationProvider {
    async fn resolve(&self, _cancel: CancellationToken) -> Option<Location> {
        Some(self.location)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::location::LocationSource;

    struct Absent;

    #[async_trait]
    impl LocationProvider for Absent {
        async fn resolve(&self, _cancel: CancellationToken) -> Option<Location> {
            None
        }
    }

    fn at(source: LocationSource) -> Arc<dyn LocationProvider> {
        Arc::new(FixedLocationProvider::new(Location::new(1.0, 2.0, source)))
    }

    #[tokio::test]
    async fn falls_through_to_the_first_available_source() {
        // Device GPS absent → home coordinate wins (ADR-015 order).
        let layered = LayeredLocationProvider::new(vec![
            Arc::new(Absent),
            at(LocationSource::HomeCoordinate),
            at(LocationSource::IpGeolocation),
        ]);
        let resolved = layered.resolve(CancellationToken::new()).await.unwrap();
        assert_eq!(resolved.source, LocationSource::HomeCoordinate);
    }

    #[tokio::test]
    async fn falls_all_the_way_to_ip_geolocation() {
        let layered = LayeredLocationProvider::new(vec![
            Arc::new(Absent),
            Arc::new(Absent),
            at(LocationSource::IpGeolocation),
        ]);
        let resolved = layered.resolve(CancellationToken::new()).await.unwrap();
        assert_eq!(resolved.source, LocationSource::IpGeolocation);
        assert!(resolved.is_approximate());
    }

    #[tokio::test]
    async fn no_sources_resolves_to_none() {
        let layered = LayeredLocationProvider::new(vec![]);
        assert!(layered.resolve(CancellationToken::new()).await.is_none());

        let all_absent = LayeredLocationProvider::new(vec![Arc::new(Absent), Arc::new(Absent)]);
        assert!(all_absent.resolve(CancellationToken::new()).await.is_none());
    }

    #[tokio::test]
    async fn cancellation_stops_resolution() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let layered = LayeredLocationProvider::new(vec![at(LocationSource::HomeCoordinate)]);
        assert!(layered.resolve(cancel).await.is_none());
    }
}
