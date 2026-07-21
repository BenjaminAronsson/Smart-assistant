//! Location value types and the location-dependence classifier (F2.9, FR-26,
//! ADR-015, docs/02 §11c). Pure domain vocabulary: no I/O. Resolving an actual
//! coordinate (device GPS, IP geolocation) happens behind the
//! `jarvis-application::LocationProvider` port; this module owns *what* a
//! location is, how sensitive/approximate it is, and *when* a query needs one.
//!
//! Location is **sensitive** (NFR-02): a resolved coordinate is always labeled
//! [`Sensitivity::Sensitive`] and carries its [`LocationSource`] provenance, so
//! the context assembler can surface it and never silently attach it to an
//! outbound cloud request.

/// Where a resolved location came from (ADR-015 resolution order). Provenance is
/// carried end-to-end so the user can see how "nearby" was localized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationSource {
    /// A paired device (jarvis-agent / mobile client) reporting GPS, location
    /// scope granted. Precise.
    DeviceGps,
    /// The configured `[location] home_lat/home_lon` — the practical default for
    /// a stationary desktop assistant. Precise (as configured).
    HomeCoordinate,
    /// Coarse IP-based geolocation, the last resort. **Approximate** (city-level
    /// at best) and must be presented as such (ADR-015).
    IpGeolocation,
}

impl LocationSource {
    /// How precise a coordinate from this source is. IP geolocation is coarse;
    /// the others are as precise as their input.
    pub fn accuracy(self) -> LocationAccuracy {
        match self {
            Self::DeviceGps | Self::HomeCoordinate => LocationAccuracy::Precise,
            Self::IpGeolocation => LocationAccuracy::Approximate,
        }
    }
}

/// Whether a coordinate is precise or a coarse approximation (ADR-015: IP
/// geolocation must never be presented as precise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationAccuracy {
    Precise,
    Approximate,
}

/// The sensitivity class of a context item (NFR-02). Location is always
/// [`Sensitivity::Sensitive`]; the enum exists so the classification is explicit
/// and testable rather than an unwritten assumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    Normal,
    Sensitive,
}

/// A resolved geographic location: a coordinate plus its provenance. Latitude/
/// longitude are plain `f64` — this is a value type surfaced to the user and
/// attached to a search, never hashed into a grant (so it needs no
/// `CanonicalValue` float discipline).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
    pub source: LocationSource,
}

impl Location {
    pub fn new(latitude: f64, longitude: f64, source: LocationSource) -> Self {
        Self {
            latitude,
            longitude,
            source,
        }
    }

    pub fn accuracy(&self) -> LocationAccuracy {
        self.source.accuracy()
    }

    pub fn is_approximate(&self) -> bool {
        self.accuracy() == LocationAccuracy::Approximate
    }

    /// Location is always sensitive (NFR-02) — this is the label the context
    /// assembler must honour before any egress.
    pub fn sensitivity(&self) -> Sensitivity {
        Sensitivity::Sensitive
    }
}

/// Phrases that make a query **location-dependent** (ADR-015): it needs a "where"
/// to answer well, so the router should resolve coordinates and prefer
/// `web.search` with them over a bare text query. A named, greppable classifier
/// — the routing *signal* the skill requires, not a heuristic buried in a prompt.
const LOCATION_MARKERS: &[&str] = &[
    "nearby",
    "near me",
    "near here",
    "close by",
    "closest",
    "around here",
    "in my area",
    "walking distance",
    "near my location",
];

/// Whether a query is location-dependent (case-insensitive substring match on the
/// [`LOCATION_MARKERS`]). Deliberately conservative: it recognises explicit
/// "nearby"/"near me" phrasing, not every query that *might* benefit from a
/// location — over-triggering would attach a sensitive coordinate to unrelated
/// searches (NFR-02).
pub fn is_location_dependent(query: &str) -> bool {
    let lower = query.to_lowercase();
    LOCATION_MARKERS.iter().any(|m| lower.contains(m))
}

/// Attach a resolved location to a search query when — and only when — the query
/// is location-dependent and a location is available. Otherwise the query is
/// returned unchanged: a non-nearby query never carries a coordinate, and a
/// nearby query with no resolved location is sent as-is rather than guessing a
/// place (ADR-015: never guess a location). The coordinate is appended as text
/// the search provider localizes on (`… near <lat>,<lon>`).
pub fn localize_query(query: &str, location: Option<&Location>) -> String {
    match location {
        Some(loc) if is_location_dependent(query) => {
            format!("{query} near {:.4},{:.4}", loc.latitude, loc.longitude)
        }
        _ => query.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_geolocation_is_approximate_others_precise() {
        assert_eq!(
            LocationSource::IpGeolocation.accuracy(),
            LocationAccuracy::Approximate
        );
        assert_eq!(
            LocationSource::DeviceGps.accuracy(),
            LocationAccuracy::Precise
        );
        assert_eq!(
            LocationSource::HomeCoordinate.accuracy(),
            LocationAccuracy::Precise
        );
        assert!(Location::new(1.0, 2.0, LocationSource::IpGeolocation).is_approximate());
    }

    #[test]
    fn location_is_always_sensitive() {
        for source in [
            LocationSource::DeviceGps,
            LocationSource::HomeCoordinate,
            LocationSource::IpGeolocation,
        ] {
            assert_eq!(
                Location::new(0.0, 0.0, source).sensitivity(),
                Sensitivity::Sensitive
            );
        }
    }

    #[test]
    fn classifies_nearby_phrasing_as_location_dependent() {
        assert!(is_location_dependent("find a lunch place nearby"));
        assert!(is_location_dependent("coffee near me"));
        assert!(is_location_dependent("Closest pharmacy"));
        // Not location-dependent: no "where" is needed.
        assert!(!is_location_dependent("who is the president"));
        assert!(!is_location_dependent("weather in Paris"));
    }

    #[test]
    fn localize_attaches_coordinates_only_when_nearby_and_available() {
        let home = Location::new(59.3293, 18.0686, LocationSource::HomeCoordinate);
        // Nearby + location → coordinates reach the query (F2.9 exit evidence).
        let localized = localize_query("lunch nearby", Some(&home));
        assert!(localized.contains("near 59.3293,18.0686"), "{localized}");
        // Nearby but NO location → sent as-is, never a guessed place (ADR-015).
        assert_eq!(localize_query("lunch nearby", None), "lunch nearby");
        // Not nearby → never carries a coordinate (NFR-02).
        assert_eq!(
            localize_query("who is the president", Some(&home)),
            "who is the president"
        );
    }
}
