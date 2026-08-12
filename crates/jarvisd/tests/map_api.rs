//! F3b.5: local PMTiles map serving through the production router (ADR-013,
//! docs/12 §3, FR-25).
//!
//! The fixture is a **synthetic PMTiles v3 archive built byte-for-byte here** —
//! no network, no committed binary blob. It carries a Berlin-shaped bounding box
//! and two real tile ids so the coverage rules are exercised against a genuine
//! header/directory/tile layout rather than a mock.
//!
//! Conformance against a *real* extract (cut from the Protomaps sample planet
//! with `pmtiles extract … --bbox=…`) is the `#[ignore]`d test at the bottom;
//! it needs an archive on disk, so it never runs in CI.
//!
//! Covered: coverage projection + mandatory OSM attribution, tile serving with
//! content type/encoding/`nosniff`/ETag/304, refusal of out-of-bbox and
//! out-of-zoom tiles, coordinate validation (no overflow, no traversal), empty
//! squares, a lying directory entry, truncated/corrupt archives, auth, and the
//! unconfigured case where the routes do not exist at all.

mod identity_fixture;
use identity_fixture::InMemoryIdentityStore;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use jarvisd::api::{AppState, Wiring, router_with};
use jarvisd::auth::AuthState;
use jarvisd::maps::MapApi;
use jarvisd::pmtiles::{Archive, TileCoord};
use std::time::SystemTime;
use tower::ServiceExt;

// --- synthetic archive builder ------------------------------------------

/// A tile to place in the fixture archive.
struct TileSpec {
    z: u8,
    x: u32,
    y: u32,
    body: Vec<u8>,
}

struct ArchiveSpec {
    min_zoom: u8,
    max_zoom: u8,
    /// (min_lon, min_lat, max_lon, max_lat)
    bounds: (f64, f64, f64, f64),
    metadata: String,
    tiles: Vec<TileSpec>,
    /// Push the last entry's data offset outside the tile-data region, to model
    /// an archive whose directory lies about where bytes live.
    lying_offset: bool,
}

impl Default for ArchiveSpec {
    fn default() -> Self {
        Self {
            min_zoom: 2,
            max_zoom: 10,
            // The Berlin extract used throughout these tests.
            bounds: (13.0, 52.3, 13.8, 52.7),
            metadata: String::from(
                r#"{"name":"jarvis test region","attribution":"<a href=\"https://example.test\">Protomaps</a> © <a href=\"https://www.openstreetmap.org\">OpenStreetMap</a>"}"#,
            ),
            tiles: Vec::new(),
            lying_offset: false,
        }
    }
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).expect("gzip write");
    encoder.finish().expect("gzip finish")
}

fn e7(degrees: f64) -> [u8; 4] {
    ((degrees * 1e7).round() as i32).to_le_bytes()
}

/// Serialize a complete, valid PMTiles v3 archive: header, gzip root directory,
/// gzip metadata, then the tile bodies (clustered, in tile-id order).
fn build_archive(spec: &ArchiveSpec) -> Vec<u8> {
    let mut entries: Vec<(u64, u64, u32)> = Vec::new(); // (tile_id, offset, length)
    let mut tile_data: Vec<u8> = Vec::new();
    let mut ordered: Vec<&TileSpec> = spec.tiles.iter().collect();
    ordered.sort_by_key(|t| {
        TileCoord::new(t.z, t.x, t.y)
            .expect("fixture tiles are addressable")
            .tile_id()
    });
    for tile in ordered {
        let id = TileCoord::new(tile.z, tile.x, tile.y).unwrap().tile_id();
        let offset = tile_data.len() as u64;
        tile_data.extend_from_slice(&tile.body);
        entries.push((id, offset, tile.body.len() as u32));
    }
    if spec.lying_offset
        && let Some(last) = entries.last_mut()
    {
        last.1 = 1 << 40;
    }

    // Directory: count, tile-id deltas, run lengths, lengths, offsets.
    let mut dir = Vec::new();
    put_varint(&mut dir, entries.len() as u64);
    let mut previous_id = 0u64;
    for (id, _, _) in &entries {
        put_varint(&mut dir, id - previous_id);
        previous_id = *id;
    }
    for _ in &entries {
        put_varint(&mut dir, 1); // run length 1: a single tile per entry
    }
    for (_, _, length) in &entries {
        put_varint(&mut dir, u64::from(*length));
    }
    for (_, offset, _) in &entries {
        put_varint(&mut dir, offset + 1); // explicit offsets (never the 0 shorthand)
    }
    let root = gzip(&dir);
    let metadata = gzip(spec.metadata.as_bytes());

    let header_len = 127u64;
    let root_offset = header_len;
    let metadata_offset = root_offset + root.len() as u64;
    let leaf_offset = metadata_offset + metadata.len() as u64;
    let tile_data_offset = leaf_offset;

    let mut head = vec![0u8; 127];
    head[..7].copy_from_slice(b"PMTiles");
    head[7] = 3;
    head[8..16].copy_from_slice(&root_offset.to_le_bytes());
    head[16..24].copy_from_slice(&(root.len() as u64).to_le_bytes());
    head[24..32].copy_from_slice(&metadata_offset.to_le_bytes());
    head[32..40].copy_from_slice(&(metadata.len() as u64).to_le_bytes());
    head[40..48].copy_from_slice(&leaf_offset.to_le_bytes());
    head[48..56].copy_from_slice(&0u64.to_le_bytes());
    head[56..64].copy_from_slice(&tile_data_offset.to_le_bytes());
    head[64..72].copy_from_slice(&(tile_data.len() as u64).to_le_bytes());
    head[72..80].copy_from_slice(&(entries.len() as u64).to_le_bytes());
    head[80..88].copy_from_slice(&(entries.len() as u64).to_le_bytes());
    head[88..96].copy_from_slice(&(entries.len() as u64).to_le_bytes());
    head[96] = 1; // clustered
    head[97] = 2; // internal compression: gzip
    head[98] = 2; // tile compression: gzip
    head[99] = 1; // tile type: mvt
    head[100] = spec.min_zoom;
    head[101] = spec.max_zoom;
    head[102..106].copy_from_slice(&e7(spec.bounds.0));
    head[106..110].copy_from_slice(&e7(spec.bounds.1));
    head[110..114].copy_from_slice(&e7(spec.bounds.2));
    head[114..118].copy_from_slice(&e7(spec.bounds.3));
    head[118] = spec.min_zoom;
    head[119..123].copy_from_slice(&e7((spec.bounds.0 + spec.bounds.2) / 2.0));
    head[123..127].copy_from_slice(&e7((spec.bounds.1 + spec.bounds.3) / 2.0));

    let mut out = head;
    out.extend_from_slice(&root);
    out.extend_from_slice(&metadata);
    out.extend_from_slice(&tile_data);
    out
}

/// Berlin at zoom 10 — inside the fixture's bounding box.
const BERLIN_Z10: (u8, u32, u32) = (10, 550, 335);
/// Angkor Wat at zoom 10 — the docs/12 §3 out-of-region example.
const ANGKOR_Z10: (u8, u32, u32) = (10, 807, 473);
/// Inside the fixture's box and zoom range, but no tile is stored there.
const EMPTY_IN_REGION: (u8, u32, u32) = (10, 551, 335);

fn berlin_body() -> Vec<u8> {
    // Stored as gzip, exactly how a real archive holds an MVT body.
    gzip(b"\x1a\x0fjarvis-test-tile")
}

fn default_spec() -> ArchiveSpec {
    ArchiveSpec {
        tiles: vec![
            TileSpec {
                z: 2,
                x: 2,
                y: 1,
                body: gzip(b"zoom-two"),
            },
            TileSpec {
                z: BERLIN_Z10.0,
                x: BERLIN_Z10.1,
                y: BERLIN_Z10.2,
                body: berlin_body(),
            },
        ],
        ..ArchiveSpec::default()
    }
}

fn temp_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "jarvis-map-{tag}-{}-{nanos}.pmtiles",
        std::process::id()
    ))
}

async fn write_archive(tag: &str, bytes: &[u8]) -> PathBuf {
    let path = temp_path(tag);
    tokio::fs::write(&path, bytes).await.expect("write fixture");
    path
}

// --- harness ------------------------------------------------------------

async fn app_with(maps: Option<MapApi>) -> (Router, String) {
    let identity = Arc::new(InMemoryIdentityStore::default());
    let auth = AuthState::bootstrap(identity).await;
    let code = auth.current_pairing_code().unwrap();
    let app = router_with(
        AppState::new().with_auth(auth),
        Wiring {
            maps,
            ..Wiring::default()
        },
    );
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/pair")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"pairingCode":"{code}","deviceName":"laptop"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let token = body["deviceToken"].as_str().unwrap().to_owned();
    (app, token)
}

/// Router with the default Berlin fixture mounted.
async fn app(tag: &str) -> (Router, String, PathBuf) {
    app_from_spec(tag, &default_spec(), None).await
}

async fn app_from_spec(
    tag: &str,
    spec: &ArchiveSpec,
    attribution: Option<String>,
) -> (Router, String, PathBuf) {
    let path = write_archive(tag, &build_archive(spec)).await;
    let archive = Archive::open(&path).await.expect("fixture archive opens");
    let (router, token) = app_with(Some(MapApi::new(Arc::new(archive), attribution))).await;
    (router, token, path)
}

struct Sent {
    status: StatusCode,
    body: Vec<u8>,
    content_type: Option<String>,
    content_encoding: Option<String>,
    cache_control: Option<String>,
    nosniff: Option<String>,
    etag: Option<String>,
}

async fn send(app: &Router, request: Request<Body>) -> Sent {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let head = |name: header::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let sent = Sent {
        status,
        content_type: head(header::CONTENT_TYPE),
        content_encoding: head(header::CONTENT_ENCODING),
        cache_control: head(header::CACHE_CONTROL),
        nosniff: head(header::X_CONTENT_TYPE_OPTIONS),
        etag: head(header::ETAG),
        body: Vec::new(),
    };
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Sent { body, ..sent }
}

fn get(path: &str, token: &str) -> Request<Body> {
    Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn tile_path((z, x, y): (u8, u32, u32)) -> String {
    format!("/api/v1/map/tiles/{z}/{x}/{y}")
}

// --- tests --------------------------------------------------------------

#[tokio::test]
async fn coverage_reports_bounds_zoom_range_and_plain_text_osm_attribution() {
    let (app, token, _path) = app("coverage").await;
    let sent = send(&app, get("/api/v1/map/coverage", &token)).await;
    assert_eq!(sent.status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();

    assert_eq!(v["bounds"]["minLon"], 13.0);
    assert_eq!(v["bounds"]["minLat"], 52.3);
    assert_eq!(v["bounds"]["maxLon"], 13.8);
    assert_eq!(v["bounds"]["maxLat"], 52.7);
    assert_eq!(v["minZoom"], 2);
    assert_eq!(v["maxZoom"], 10);
    assert_eq!(v["tileFormat"], "mvt");
    assert_eq!(v["tileUrlTemplate"], "/api/v1/map/tiles/{z}/{x}/{y}");
    assert_eq!(v["name"], "jarvis test region");

    // docs/12 §3: attribution is mandatory and never hidden — and it is text,
    // so an archive's metadata cannot inject markup or a link into the HUD.
    let attribution = v["attribution"].as_str().expect("attribution is present");
    assert!(
        attribution.contains("OpenStreetMap"),
        "attribution must name OpenStreetMap: {attribution}"
    );
    assert!(
        !attribution.contains('<') && !attribution.contains("href"),
        "attribution must be plain text: {attribution}"
    );
}

#[tokio::test]
async fn an_archive_that_omits_osm_cannot_suppress_the_attribution() {
    // Operator override that forgets OSM, and archive metadata with none: the
    // served string still names OpenStreetMap.
    let spec = ArchiveSpec {
        metadata: r#"{"name":"unattributed"}"#.to_owned(),
        ..default_spec()
    };
    let (app, token, _path) =
        app_from_spec("attribution", &spec, Some("My Own Data".to_owned())).await;
    let sent = send(&app, get("/api/v1/map/coverage", &token)).await;
    let v: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    let attribution = v["attribution"].as_str().unwrap();
    assert!(attribution.contains("My Own Data"));
    assert!(
        attribution.contains("OpenStreetMap"),
        "the OSM credit is appended, never replaced away: {attribution}"
    );
}

#[tokio::test]
async fn in_region_tile_is_served_with_type_encoding_nosniff_and_a_strong_etag() {
    let (app, token, _path) = app("tile").await;
    let sent = send(&app, get(&tile_path(BERLIN_Z10), &token)).await;

    assert_eq!(sent.status, StatusCode::OK);
    assert_eq!(sent.body, berlin_body(), "stored bytes round-trip exactly");
    assert_eq!(
        sent.content_type.as_deref(),
        Some("application/vnd.mapbox-vector-tile")
    );
    // Forwarded as stored — the server never inflates a tile it only passes on.
    assert_eq!(sent.content_encoding.as_deref(), Some("gzip"));
    assert_eq!(sent.nosniff.as_deref(), Some("nosniff"));
    // Cacheable, but NOT immutable: the URL is not a content address, and an
    // archive swap must be able to invalidate it.
    let cache = sent.cache_control.expect("tiles carry Cache-Control");
    assert!(cache.contains("private"), "{cache}");
    assert!(
        !cache.contains("immutable"),
        "a tile URL is not content-addressed: {cache}"
    );
    let etag = sent.etag.expect("tiles carry an ETag");
    assert!(etag.starts_with('"') && etag.ends_with('"'));

    let revalidated = send(
        &app,
        Request::get(tile_path(BERLIN_Z10))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::IF_NONE_MATCH, &etag)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(revalidated.status, StatusCode::NOT_MODIFIED);
    assert!(revalidated.body.is_empty());
}

#[tokio::test]
async fn a_tile_outside_the_bounding_box_is_refused_not_approximated() {
    let (app, token, _path) = app("outside").await;
    let sent = send(&app, get(&tile_path(ANGKOR_Z10), &token)).await;
    assert_eq!(
        sent.status,
        StatusCode::NOT_FOUND,
        "docs/12 §3: never silently show the wrong place"
    );
    let v: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(v["code"], "resource.not_found");
    assert!(
        sent.body.windows(4).all(|w| w != b"\x1f\x8b\x08\x00"),
        "no tile bytes may accompany a refusal"
    );
}

#[tokio::test]
async fn tiles_outside_the_archive_zoom_range_are_refused() {
    let (app, token, _path) = app("zoom").await;
    // Below min zoom (fixture starts at 2): the world tile exists in every real
    // basemap, but not in this archive.
    let below = send(&app, get(&tile_path((1, 1, 0)), &token)).await;
    assert_eq!(below.status, StatusCode::NOT_FOUND);
    // Above max zoom: MapLibre would happily ask for z11 while overzooming.
    let above = send(&app, get(&tile_path((11, 1100, 671)), &token)).await;
    assert_eq!(above.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn coordinates_cannot_overflow_escape_or_address_a_nonexistent_grid() {
    let (app, token, _path) = app("coords").await;
    for path in [
        // Not numbers at all.
        "/api/v1/map/tiles/a/b/c",
        "/api/v1/map/tiles/10/-1/335",
        "/api/v1/map/tiles/10/550/%20335",
        // Past the grid at that zoom (2^10 = 1024).
        "/api/v1/map/tiles/10/1024/335",
        "/api/v1/map/tiles/10/550/1024",
        // Past u32 / u8, and past the addressable zoom range.
        "/api/v1/map/tiles/10/4294967296/335",
        "/api/v1/map/tiles/300/1/1",
        "/api/v1/map/tiles/27/1/1",
        // Traversal attempts: the path never reaches the filesystem (the
        // archive path comes from config), and these do not even parse.
        "/api/v1/map/tiles/10/550/%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "/api/v1/map/tiles/10/550/335%00",
    ] {
        let sent = send(&app, get(path, &token)).await;
        assert_eq!(
            sent.status,
            StatusCode::BAD_REQUEST,
            "{path} must be rejected as a malformed coordinate"
        );
        let v: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(v["code"], "validation.failed", "{path}");
    }
}

#[tokio::test]
async fn an_empty_square_inside_coverage_is_no_content_not_a_neighbour() {
    let (app, token, _path) = app("empty").await;
    let sent = send(&app, get(&tile_path(EMPTY_IN_REGION), &token)).await;
    assert_eq!(sent.status, StatusCode::NO_CONTENT);
    assert!(
        sent.body.is_empty(),
        "an absent tile returns nothing at all, never a nearby tile's bytes"
    );
}

#[tokio::test]
async fn a_directory_entry_pointing_outside_the_tile_region_fails_closed() {
    // The archive is well-formed except that one entry claims its bytes live at
    // offset 2^40 — past the end of the tile data. That must be an error with no
    // body, not a panic and not some other tile's bytes.
    let spec = ArchiveSpec {
        lying_offset: true,
        ..default_spec()
    };
    let (app, token, _path) = app_from_spec("lying", &spec, None).await;
    let sent = send(&app, get(&tile_path(BERLIN_Z10), &token)).await;
    assert_eq!(sent.status, StatusCode::SERVICE_UNAVAILABLE);
    let v: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(v["code"], "provider.unavailable");
}

#[tokio::test]
async fn map_endpoints_require_a_device_token() {
    let (app, _token, _path) = app("auth").await;
    for path in ["/api/v1/map/coverage", &tile_path(BERLIN_Z10)] {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{path} must require a paired device"
        );
    }
}

#[tokio::test]
async fn without_a_configured_archive_the_map_routes_do_not_exist() {
    // Absent, not broken: the client reads the 404 on coverage as "no local
    // map" and takes the docs/12 §3 fallback.
    let (app, token) = app_with(None).await;
    for path in ["/api/v1/map/coverage", &tile_path(BERLIN_Z10)] {
        let sent = send(&app, get(path, &token)).await;
        assert_eq!(sent.status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn a_truncated_archive_is_a_clean_error_not_a_panic() {
    let bytes = build_archive(&default_spec());
    // Keep the header (so the magic still matches) and lose the rest: exactly
    // what an interrupted download looks like.
    let path = write_archive("truncated", &bytes[..bytes.len() / 2]).await;
    // `match` rather than `expect_err`: an open archive is deliberately not
    // `Debug` (it would print the whole root directory).
    let message = match Archive::open(&path).await {
        Ok(_) => panic!("a truncated archive must not open"),
        Err(e) => e.to_string(),
    };
    assert!(
        message.contains("truncated") || message.contains("malformed"),
        "unexpected error: {message}"
    );

    // Nothing at all left of the file.
    let empty = write_archive("empty-file", b"").await;
    assert!(Archive::open(&empty).await.is_err());

    // Header-length garbage: right size, wrong content.
    let garbage = write_archive("garbage", &vec![0xa5u8; 4096]).await;
    assert!(Archive::open(&garbage).await.is_err());

    // A v2 archive (correct magic, wrong version) is refused rather than
    // half-understood.
    let mut v2 = bytes.clone();
    v2[7] = 2;
    let v2_path = write_archive("v2", &v2).await;
    assert!(Archive::open(&v2_path).await.is_err());

    // A missing file is an error, never a silently empty map.
    assert!(Archive::open(temp_path("absent")).await.is_err());
}

#[tokio::test]
async fn an_archive_whose_root_directory_is_corrupt_is_refused_at_open() {
    let mut bytes = build_archive(&default_spec());
    // Scribble over the gzip root directory (it starts right after the header).
    for byte in bytes.iter_mut().skip(127).take(16) {
        *byte = 0x00;
    }
    let path = write_archive("corrupt-root", &bytes).await;
    assert!(
        Archive::open(&path).await.is_err(),
        "a corrupt root directory must fail at open, not at request time"
    );
}

/// Conformance against a **real** regional extract, which is deliberately not in
/// the repository (binary bulk). Cut one with go-pmtiles over HTTP range
/// requests — the docs/08 §6 "downloaded regional extract" default:
///
/// ```text
/// pmtiles extract \
///   https://r2-public.protomaps.com/protomaps-sample-datasets/protomaps_vector_planet_odbl_z10.pmtiles \
///   /tmp/region.pmtiles --bbox=13.0,52.3,13.8,52.7
/// pmtiles tile /tmp/region.pmtiles 10 550 335 > /tmp/reference-tile.mvt
/// JARVIS_PMTILES_FIXTURE=/tmp/region.pmtiles \
/// JARVIS_PMTILES_REFERENCE=/tmp/reference-tile.mvt \
///   cargo test -p jarvisd --test map_api -- --ignored
/// ```
///
/// The reference tile is the point: it proves this reader's Hilbert indexing and
/// directory decoding agree with go-pmtiles on a real archive, which a
/// self-built fixture cannot.
#[tokio::test]
#[ignore = "needs a real extract; set JARVIS_PMTILES_FIXTURE"]
async fn real_extract_agrees_with_the_reference_implementation() {
    let Some(fixture) = std::env::var_os("JARVIS_PMTILES_FIXTURE") else {
        panic!("set JARVIS_PMTILES_FIXTURE to a real .pmtiles archive");
    };
    let archive = Archive::open(PathBuf::from(fixture))
        .await
        .expect("real extract opens");
    let header = archive.header();
    assert!(header.max_zoom >= 10, "expected a z10 extract");

    let berlin = TileCoord::new(BERLIN_Z10.0, BERLIN_Z10.1, BERLIN_Z10.2).unwrap();
    assert!(archive.covers(berlin), "Berlin must be inside the extract");
    let tile = archive
        .tile(berlin)
        .await
        .expect("read succeeds")
        .expect("Berlin z10 is present in the extract");
    assert!(!tile.bytes.is_empty());

    if let Some(reference) = std::env::var_os("JARVIS_PMTILES_REFERENCE") {
        let expected = tokio::fs::read(PathBuf::from(reference))
            .await
            .expect("reference tile file");
        assert_eq!(
            tile.bytes, expected,
            "tile bytes must match `pmtiles tile` byte-for-byte"
        );
    }

    // Out of region: refused by coverage, and absent from the archive too.
    let angkor = TileCoord::new(ANGKOR_Z10.0, ANGKOR_Z10.1, ANGKOR_Z10.2).unwrap();
    assert!(!archive.covers(angkor));
    assert!(archive.tile(angkor).await.expect("read succeeds").is_none());
}
