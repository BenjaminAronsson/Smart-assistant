//! Map coverage wire DTOs (F3b.5, FR-25, ADR-013, docs/12 §3).
//!
//! ADR-013's map is a PMTiles region extract served by `jarvisd` — offline,
//! keyless, no third-party tile requests. The extract covers the owner's home
//! region *only*, which makes coverage a first-class part of the contract:
//! docs/12 §3 requires the card to fall back (online raster, or a
//! coordinates-only card offline) for anywhere outside it, and to **never
//! silently show the wrong place**.
//!
//! [`MapCoverageResponse`] is what makes that decision possible *before* a tile
//! is requested: the client compares the destination against `bounds` and
//! `minZoom`/`maxZoom` and chooses its renderer. The server enforces the same
//! box on every tile request regardless of what the client concluded — a tile
//! outside the archive is refused, never approximated.
//!
//! `attribution` is non-optional and guaranteed non-empty: docs/12 §3 makes OSM
//! attribution mandatory and never hidden, so a client that renders coverage
//! has the attribution string in the same payload it needed to draw at all.
//! It is **plain text** — markup in the archive's metadata is stripped
//! server-side, so the shell renders it as text and an archive file cannot
//! inject a link or markup into the HUD.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What the archive's tiles are, so the client picks a vector or raster source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MapTileFormatDto {
    /// Mapbox Vector Tile — the ADR-013 production case (MapLibre GL vector source).
    Mvt,
    Png,
    Jpeg,
    Webp,
    Avif,
}

/// A WGS84 bounding box in degrees, `min <= max` on both axes. As an archive's
/// coverage, it is the region the extract covers: outside it the client must
/// fall back (docs/12 §3), and the server refuses those tiles either way.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MapBoundsDto {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

/// The archive's suggested opening view.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MapCenterDto {
    pub lon: f64,
    pub lat: f64,
    pub zoom: u8,
}

/// `GET /api/v1/map/coverage` — what the locally served archive covers.
///
/// The endpoint exists only when an archive is configured (`[maps]
/// pmtiles_path`); with none, the map routes are not registered at all and this
/// is a 404, which the client reads as "no local map, use the fallback" —
/// absent rather than broken.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MapCoverageResponse {
    pub bounds: MapBoundsDto,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub center: MapCenterDto,
    /// Tile URL template with `{z}`/`{x}`/`{y}` placeholders, ready to hand to
    /// MapLibre as a source URL. Server-relative and same-origin: tiles are
    /// served by `jarvisd`, so no map traffic leaves the machine (ADR-013).
    pub tile_url_template: String,
    pub tile_format: MapTileFormatDto,
    /// Mandatory, non-empty, plain text, and always names OpenStreetMap
    /// (docs/12 §3: attribution is never hidden).
    pub attribution: String,
    /// The archive's own name, when it declares one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}
