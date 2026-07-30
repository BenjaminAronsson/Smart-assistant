//! Local map serving (F3b.5, FR-25, ADR-013, docs/12 §3): the PMTiles region
//! extract, served by `jarvisd` to the HUD's map card.
//!
//! Two authenticated endpoints, mounted only when `[maps] pmtiles_path` names a
//! readable archive:
//!
//! * `GET /api/v1/map/coverage` — the archive's bounds, zoom range, centre,
//!   tile-URL template and attribution. The client reads this **before** it
//!   requests any tile and decides in-region versus out-of-region (docs/12 §3:
//!   outside the extract it falls back to online raster, or to a
//!   coordinates-only card offline — never a blank or wrong-region map).
//! * `GET /api/v1/map/tiles/{z}/{x}/{y}` — one tile body, exactly as stored.
//!
//! **Why decode server-side instead of range-serving the archive.** The
//! alternative is to serve byte ranges of the `.pmtiles` file and let the
//! browser's `pmtiles://` protocol handler do directory lookups. We serve
//! decoded tiles instead, for three reasons that all point the same way:
//!
//! 1. *The client gets simpler, not harder.* A `{z}/{x}/{y}` template is a
//!    plain MapLibre vector source — no protocol plugin, no client-side
//!    directory parsing, and one obvious place (`transformRequest`) to attach
//!    the device bearer token. Range requests would need the same token on every
//!    range anyway, plus a JS library to interpret them.
//! 2. *The server can be honest about bounds.* A range server cannot refuse a
//!    wrong-region tile — it does not know what the bytes mean. Here, an
//!    out-of-coverage request is refused before the file is even opened, which
//!    is what "never silently show the wrong place" requires of the server side.
//! 3. *Caching is truthful per tile.* Each tile gets a strong validator derived
//!    from the archive fingerprint and the tile's address in it, so a client
//!    revalidates one tile rather than opaque byte windows of a 300 MB file.
//!
//! The cost — decoding the PMTiles directory per request — is bounded and small:
//! the root directory is resident, and a tile costs one open plus one or two
//! short reads. The archive is never buffered (see `pmtiles::Archive`).

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use jarvis_contracts::errors::ErrorCode;
use jarvis_contracts::maps::{MapBoundsDto, MapCenterDto, MapCoverageResponse, MapTileFormatDto};

use crate::pmtiles::{Archive, ArchiveError, TileCoord, TileType};
use crate::problem::problem;

/// The tile URL template handed to MapLibre. Same-origin and server-relative:
/// no map request ever leaves the machine (ADR-013).
pub const TILE_URL_TEMPLATE: &str = "/api/v1/map/tiles/{z}/{x}/{y}";

/// The attribution every locally served map carries when the archive says
/// nothing usable. OpenStreetMap data under ODbL — docs/12 §3 makes this
/// mandatory and never hidden.
pub const DEFAULT_ATTRIBUTION: &str = "\u{00a9} OpenStreetMap contributors";

/// The substring that must appear in whatever attribution we serve. An archive
/// (or an operator override) may add to it; it can never replace it away.
const REQUIRED_ATTRIBUTION_SUBJECT: &str = "OpenStreetMap";

/// State for the map routes. Cloneable so it can be axum route state; the
/// archive is shared, never copied.
#[derive(Clone)]
pub struct MapApi {
    archive: Arc<Archive>,
    attribution: Arc<str>,
}

impl MapApi {
    /// Build the surface for an opened archive.
    ///
    /// Attribution resolution, in order: the operator's `[maps] attribution`
    /// override, else the archive's own metadata (reduced to plain text), else
    /// the OSM default. Whatever comes out, if it does not already name
    /// OpenStreetMap the default is appended — the one string the HUD is
    /// required to be able to show cannot be configured away (docs/12 §3).
    pub fn new(archive: Arc<Archive>, override_attribution: Option<String>) -> Self {
        let chosen = override_attribution
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .or_else(|| archive.metadata().attribution.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_ATTRIBUTION.to_owned());
        let attribution = if chosen.contains(REQUIRED_ATTRIBUTION_SUBJECT) {
            chosen
        } else {
            format!("{chosen} \u{2014} {DEFAULT_ATTRIBUTION}")
        };
        Self {
            archive,
            attribution: attribution.into(),
        }
    }

    /// The coverage projection, also used by the tests and by `GET …/coverage`.
    pub fn coverage(&self) -> MapCoverageResponse {
        let header = self.archive.header();
        MapCoverageResponse {
            bounds: MapBoundsDto {
                min_lon: header.bounds.min_lon,
                min_lat: header.bounds.min_lat,
                max_lon: header.bounds.max_lon,
                max_lat: header.bounds.max_lat,
            },
            min_zoom: header.min_zoom,
            max_zoom: header.max_zoom,
            center: MapCenterDto {
                lon: header.center_lon,
                lat: header.center_lat,
                zoom: header.center_zoom,
            },
            tile_url_template: TILE_URL_TEMPLATE.to_owned(),
            tile_format: match header.tile_type {
                TileType::Mvt => MapTileFormatDto::Mvt,
                TileType::Png => MapTileFormatDto::Png,
                TileType::Jpeg => MapTileFormatDto::Jpeg,
                TileType::Webp => MapTileFormatDto::Webp,
                TileType::Avif => MapTileFormatDto::Avif,
            },
            attribution: self.attribution.to_string(),
            name: self.archive.metadata().name.clone(),
        }
    }
}

/// `GET /api/v1/map/coverage`.
pub async fn get_coverage(State(api): State<MapApi>) -> Json<MapCoverageResponse> {
    Json(api.coverage())
}

/// `GET /api/v1/map/tiles/{z}/{x}/{y}`.
///
/// Outcomes, all deliberate:
///
/// * **400** — the path is not three numbers, or names a tile that cannot exist
///   at that zoom (`x >= 2^z`). Parsed in the handler rather than via typed
///   `Path` extractors so the body is our RFC 9457 problem, not axum's
///   plain-text 400.
/// * **404** — the tile is outside the archive's zoom range or bounding box.
///   A refusal: the server will not answer with a neighbouring region's tile,
///   and the client is expected to have taken the coverage fallback already.
/// * **204** — inside coverage, but the archive holds no tile there (an empty
///   square). Standard for tile services and rendered as empty by MapLibre.
/// * **503** — the archive is unreadable or structurally broken. Fail closed,
///   with no bytes and no internals in the body.
pub async fn get_tile(
    State(api): State<MapApi>,
    Path((z, x, y)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    // Parse into the exact widths the coordinate space allows. Anything that is
    // not a plain unsigned decimal — a sign, whitespace, `..`, a percent escape,
    // a number past u32 — fails here and never becomes arithmetic. There is no
    // path component that reaches the filesystem: the archive path comes from
    // config alone, so traversal has nothing to traverse.
    let (Ok(z), Ok(x), Ok(y)) = (z.parse::<u8>(), x.parse::<u32>(), y.parse::<u32>()) else {
        return Err(bad_coordinate());
    };
    let Some(coord) = TileCoord::new(z, x, y) else {
        return Err(bad_coordinate());
    };

    // Coverage is checked before the archive is touched: an out-of-region
    // request costs no I/O and cannot resolve to anything.
    if !api.archive.covers(coord) {
        return Err(problem(
            StatusCode::NOT_FOUND,
            ErrorCode::ResourceNotFound,
            "that tile is outside the locally served map region",
            None,
        ));
    }

    // A strong validator over (which archive, where the tile sits in it). It
    // changes if the operator swaps the archive, so a cached tile from the
    // previous region can never be revalidated as fresh.
    let etag = format!(
        "\"{}-{}-{}\"",
        api.archive.fingerprint(),
        coord.tile_id(),
        z
    );
    if let Some(inm) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        && inm.split(',').any(|t| t.trim() == etag || t.trim() == "*")
    {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    let tile = match api.archive.tile(coord).await {
        Ok(Some(tile)) => tile,
        // In coverage, no tile stored: an honestly empty square.
        Ok(None) => {
            return Ok((
                StatusCode::NO_CONTENT,
                [
                    (header::ETAG, etag),
                    (header::CACHE_CONTROL, cache_control().to_owned()),
                ],
            )
                .into_response());
        }
        Err(e) => {
            // The detail stays generic: offsets and paths are internals
            // (docs/06 §5). The operator gets the real reason in the log.
            tracing::error!(error = %e, z, x, y, "map tile read failed");
            return Err(problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                match e {
                    ArchiveError::Io(_) => "the map archive is unreadable",
                    _ => "the map archive is not usable",
                },
                None,
            ));
        }
    };

    let mut response_headers = HeaderMap::new();
    let tile_type = api.archive.header().tile_type;
    put_header(
        &mut response_headers,
        header::CONTENT_TYPE,
        tile_type.content_type(),
    );
    // Tile bodies are forwarded exactly as stored — the browser inflates them.
    // The server never decodes a tile it only passes through, which keeps both
    // CPU and per-request memory flat.
    if let Some(encoding) = api.archive.header().tile_compression.content_encoding() {
        put_header(&mut response_headers, header::CONTENT_ENCODING, encoding);
    }
    // The declared type is the only type: no sniffing a vector tile into
    // something executable (docs/06 §6).
    put_header(
        &mut response_headers,
        header::X_CONTENT_TYPE_OPTIONS,
        "nosniff",
    );
    put_header(&mut response_headers, header::ETAG, &etag);
    put_header(
        &mut response_headers,
        header::CACHE_CONTROL,
        cache_control(),
    );

    Ok((StatusCode::OK, response_headers, Body::from(tile.bytes)).into_response())
}

/// Deliberately **not** `immutable`. The artifact blob route may use it because
/// its URL *is* the content address; a tile URL is not — `/12/2200/1343` means
/// whatever archive is configured today. Re-cutting the extract would leave an
/// `immutable` cache serving the old region with no way to revalidate, which is
/// the wrong-region failure by another route. A day of freshness plus a strong
/// ETag gets the same offline behaviour and stays correct across a swap.
fn cache_control() -> &'static str {
    "private, max-age=86400"
}

fn put_header(into: &mut HeaderMap, name: HeaderName, value: &str) {
    match HeaderValue::from_str(value) {
        Ok(value) => {
            into.insert(name, value);
        }
        // Every value here is either a constant or hex+digits, so this is
        // unreachable in practice — but a header that cannot be built is dropped
        // rather than panicking on a request path.
        Err(_) => tracing::error!(header = %name, "map response header value was not valid"),
    }
}

fn bad_coordinate() -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        ErrorCode::ValidationFailed,
        "tile coordinates must be z/x/y with 0 <= x,y < 2^z and z <= 26",
        None,
    )
}
