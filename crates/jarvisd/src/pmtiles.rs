//! A minimal, fail-closed **PMTiles v3** reader (F3b.5, ADR-013).
//!
//! ADR-013 renders maps from a PMTiles region extract served by `jarvisd`: no
//! API key, no per-load cost, and — decisively — **no tile request leaves the
//! machine**. That makes the archive file the only input, and it is a *file on
//! disk that this process did not write*: everything past the 127-byte header is
//! attacker-shaped data as far as this module is concerned (a downloaded extract,
//! a half-finished `pmtiles extract`, a truncated copy on a full disk). Hence the
//! rules this module follows without exception:
//!
//! * **No panics on archive bytes.** No `unwrap`, no indexing, no unchecked
//!   arithmetic on any value that came out of the file — every span is
//!   `checked_add`ed and range-checked against the real file length before it is
//!   read, every slice goes through `get(..)`, and every varint has an overflow
//!   guard. `#![deny(unsafe_code)]` is on crate-wide.
//! * **Bounded memory, always.** A request reads the directory pages and the one
//!   tile it needs — never the archive. Every allocation from a
//!   file-declared length is capped *before* the `vec![0; len]`, and gzip streams
//!   are read through `Read::take` so a decompression bomb hits a ceiling
//!   instead of the OOM killer.
//! * **Absent, not approximated.** A tile that is not in the archive comes back
//!   as `Ok(None)`. A structurally broken archive is an `Err`. Neither ever
//!   yields *some other tile's* bytes — serving the wrong region is the one
//!   failure docs/12 §3 names explicitly ("never silently show the wrong
//!   place").
//!
//! Why hand-rolled rather than the `pmtiles` crate: the reader we need is the
//! header, the directory varints and a Hilbert index — the parts of the spec
//! that are frozen — while the crate's value is in the backends we must not have
//! (HTTP/S3 fetchers pull a second TLS stack; the mmap backend turns a truncated
//! file into SIGBUS, which is precisely the failure mode this module is required
//! to turn into a clean error). The one thing we cannot hand-roll is inflate,
//! which is why `flate2` is a dependency.
//!
//! Spec: <https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md>.

use std::borrow::Cow;
use std::io::Read;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Fixed-size v3 header at offset 0.
const HEADER_LEN: usize = 127;
const MAGIC: &[u8] = b"PMTiles";
const SPEC_VERSION: u8 = 3;

/// Ceiling on a single directory page, compressed and decompressed. Root
/// directories are ~16 KiB by convention and leaves smaller still; 8 MiB is
/// orders of magnitude of headroom while still bounding one request's memory.
const MAX_DIRECTORY_BYTES: usize = 8 * 1024 * 1024;
/// Ceiling on the JSON metadata blob (name/attribution/vector layers).
const MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;
/// Ceiling on one tile body. Vector tiles are tens of KiB; anything past this is
/// a corrupt length, not a tile.
const MAX_TILE_BYTES: usize = 16 * 1024 * 1024;
/// Directory entries we will parse from one page. Guards `with_capacity` and the
/// parse loop against a bogus entry count.
const MAX_DIRECTORY_ENTRIES: u64 = 4_000_000;
/// Leaf-directory hops per lookup. The spec allows nesting; real archives use at
/// most one level. A cycle in a corrupt archive terminates here rather than
/// looping forever.
const MAX_LEAF_DEPTH: usize = 4;
/// Highest zoom addressable by the Hilbert tile-id scheme (2·26 = 52 bits).
pub const MAX_ZOOM: u8 = 26;

/// Everything that can go wrong reading an archive. Deliberately coarse at the
/// boundary — the HTTP layer maps all of these to "the map is unavailable" and
/// never echoes offsets or paths to a client (docs/06 §5).
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("i/o error reading the archive: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a PMTiles archive (bad magic)")]
    NotPmTiles,
    #[error("PMTiles spec version {0} is not supported (this reader speaks v3)")]
    UnsupportedVersion(u8),
    #[error("archive is truncated: {0} extends past the end of the file")]
    Truncated(&'static str),
    #[error(
        "archive uses {kind} compression for {what}; supported: none, gzip \
         (re-cut the archive with `pmtiles convert`)"
    )]
    UnsupportedCompression {
        what: &'static str,
        kind: &'static str,
    },
    #[error("archive declares tile type {0}, which this reader does not serve")]
    UnsupportedTileType(u8),
    #[error("archive header is invalid: {0}")]
    InvalidHeader(&'static str),
    #[error("archive directory is malformed: {0}")]
    MalformedDirectory(&'static str),
    #[error("archive {what} is larger than the {limit} byte ceiling")]
    Oversize { what: &'static str, limit: usize },
    #[error("archive nests leaf directories more than {MAX_LEAF_DEPTH} deep")]
    LeafDepthExceeded,
}

/// Compression of a byte range inside the archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Brotli,
    Zstd,
}

impl Compression {
    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::None),
            2 => Some(Self::Gzip),
            3 => Some(Self::Brotli),
            4 => Some(Self::Zstd),
            // 0 is "unknown" — an archive that will not say how it is encoded is
            // not one we guess at.
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Gzip => "gzip",
            Self::Brotli => "brotli",
            Self::Zstd => "zstd",
        }
    }

    /// The HTTP `Content-Encoding` for tile bodies stored with this compression,
    /// or `None` when the bytes are already plain. Tile bodies are passed
    /// through *as stored* — the browser inflates them — so the server never
    /// spends CPU or memory decoding a tile it only forwards.
    pub fn content_encoding(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Gzip => Some("gzip"),
            Self::Brotli => Some("br"),
            Self::Zstd => Some("zstd"),
        }
    }
}

/// What the tiles in this archive are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileType {
    Mvt,
    Png,
    Jpeg,
    Webp,
    Avif,
}

impl TileType {
    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Mvt),
            2 => Some(Self::Png),
            3 => Some(Self::Jpeg),
            4 => Some(Self::Webp),
            5 => Some(Self::Avif),
            _ => None,
        }
    }

    /// The `Content-Type` served for a tile body. All five are inert image or
    /// protobuf types — none of them is script, and `nosniff` pins the browser
    /// to exactly this (docs/06 §6).
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Mvt => "application/vnd.mapbox-vector-tile",
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Avif => "image/avif",
        }
    }
}

/// Geographic coverage declared by the archive header, in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

impl Bounds {
    /// Does the given tile's footprint overlap this coverage at all? The test a
    /// tile request has to pass before the archive is touched: a query outside
    /// the extract is refused here, without I/O and without any chance of
    /// resolving to a neighbouring region's bytes.
    pub fn intersects(&self, other: &Bounds) -> bool {
        self.min_lon <= other.max_lon
            && self.max_lon >= other.min_lon
            && self.min_lat <= other.max_lat
            && self.max_lat >= other.min_lat
    }
}

/// The parsed v3 header.
#[derive(Debug, Clone)]
pub struct Header {
    root_dir_offset: u64,
    root_dir_len: u64,
    metadata_offset: u64,
    metadata_len: u64,
    leaf_dirs_offset: u64,
    leaf_dirs_len: u64,
    tile_data_offset: u64,
    tile_data_len: u64,
    pub internal_compression: Compression,
    pub tile_compression: Compression,
    pub tile_type: TileType,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub bounds: Bounds,
    pub center_zoom: u8,
    pub center_lon: f64,
    pub center_lat: f64,
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64, ArchiveError> {
    let raw: [u8; 8] = bytes
        .get(at..at + 8)
        .and_then(|s| s.try_into().ok())
        .ok_or(ArchiveError::Truncated("header"))?;
    Ok(u64::from_le_bytes(raw))
}

fn i32_at(bytes: &[u8], at: usize) -> Result<i32, ArchiveError> {
    let raw: [u8; 4] = bytes
        .get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .ok_or(ArchiveError::Truncated("header"))?;
    Ok(i32::from_le_bytes(raw))
}

fn u8_at(bytes: &[u8], at: usize) -> Result<u8, ArchiveError> {
    bytes
        .get(at)
        .copied()
        .ok_or(ArchiveError::Truncated("header"))
}

/// E7 fixed-point degrees, as the spec stores coordinates.
fn e7(value: i32) -> f64 {
    f64::from(value) / 1e7
}

impl Header {
    /// Parse the fixed 127-byte header. Every field is read by offset with a
    /// bounds check; nothing here can panic on a short or hostile buffer.
    pub fn parse(bytes: &[u8]) -> Result<Self, ArchiveError> {
        if bytes.len() < HEADER_LEN {
            return Err(ArchiveError::Truncated("header"));
        }
        if bytes.get(0..MAGIC.len()) != Some(MAGIC) {
            return Err(ArchiveError::NotPmTiles);
        }
        let version = u8_at(bytes, 7)?;
        if version != SPEC_VERSION {
            return Err(ArchiveError::UnsupportedVersion(version));
        }

        let internal_compression = Compression::from_code(u8_at(bytes, 97)?).ok_or(
            ArchiveError::UnsupportedCompression {
                what: "directories",
                kind: "unknown",
            },
        )?;
        // Directories must be *decoded* by this process, so only what flate2
        // gives us is acceptable. Tile bodies are a different matter — they are
        // forwarded as stored, so any of the four is fine there.
        match internal_compression {
            Compression::None | Compression::Gzip => {}
            other => {
                return Err(ArchiveError::UnsupportedCompression {
                    what: "directories",
                    kind: other.label(),
                });
            }
        }
        let tile_compression = Compression::from_code(u8_at(bytes, 98)?).ok_or(
            ArchiveError::UnsupportedCompression {
                what: "tiles",
                kind: "unknown",
            },
        )?;
        let tile_type_code = u8_at(bytes, 99)?;
        let tile_type = TileType::from_code(tile_type_code)
            .ok_or(ArchiveError::UnsupportedTileType(tile_type_code))?;

        let min_zoom = u8_at(bytes, 100)?;
        let max_zoom = u8_at(bytes, 101)?;
        if min_zoom > max_zoom {
            return Err(ArchiveError::InvalidHeader("min zoom exceeds max zoom"));
        }
        if max_zoom > MAX_ZOOM {
            return Err(ArchiveError::InvalidHeader("max zoom exceeds 26"));
        }

        let bounds = Bounds {
            min_lon: e7(i32_at(bytes, 102)?),
            min_lat: e7(i32_at(bytes, 106)?),
            max_lon: e7(i32_at(bytes, 110)?),
            max_lat: e7(i32_at(bytes, 114)?),
        };
        if !(-180.0..=180.0).contains(&bounds.min_lon)
            || !(-180.0..=180.0).contains(&bounds.max_lon)
            || !(-90.0..=90.0).contains(&bounds.min_lat)
            || !(-90.0..=90.0).contains(&bounds.max_lat)
            || bounds.min_lon > bounds.max_lon
            || bounds.min_lat > bounds.max_lat
        {
            return Err(ArchiveError::InvalidHeader("bounds are not a valid box"));
        }

        Ok(Self {
            root_dir_offset: u64_at(bytes, 8)?,
            root_dir_len: u64_at(bytes, 16)?,
            metadata_offset: u64_at(bytes, 24)?,
            metadata_len: u64_at(bytes, 32)?,
            leaf_dirs_offset: u64_at(bytes, 40)?,
            leaf_dirs_len: u64_at(bytes, 48)?,
            tile_data_offset: u64_at(bytes, 56)?,
            tile_data_len: u64_at(bytes, 64)?,
            internal_compression,
            tile_compression,
            tile_type,
            min_zoom,
            max_zoom,
            bounds,
            center_zoom: u8_at(bytes, 118)?,
            center_lon: e7(i32_at(bytes, 119)?),
            center_lat: e7(i32_at(bytes, 123)?),
        })
    }

    /// Every span the header declares must lie inside the file that actually
    /// exists. This is where a truncated download stops being usable: the header
    /// still parses (it is the first 127 bytes), but its directory or tile-data
    /// span runs past EOF, and the archive is rejected at open rather than
    /// producing short reads at request time.
    fn validate_spans(&self, file_len: u64) -> Result<(), ArchiveError> {
        let spans: [(&'static str, u64, u64); 4] = [
            ("root directory", self.root_dir_offset, self.root_dir_len),
            ("metadata", self.metadata_offset, self.metadata_len),
            (
                "leaf directories",
                self.leaf_dirs_offset,
                self.leaf_dirs_len,
            ),
            ("tile data", self.tile_data_offset, self.tile_data_len),
        ];
        for (what, offset, len) in spans {
            let end = offset
                .checked_add(len)
                .ok_or(ArchiveError::Truncated(what))?;
            if end > file_len {
                return Err(ArchiveError::Truncated(what));
            }
        }
        if self.root_dir_len > MAX_DIRECTORY_BYTES as u64 {
            return Err(ArchiveError::Oversize {
                what: "root directory",
                limit: MAX_DIRECTORY_BYTES,
            });
        }
        if self.metadata_len > MAX_METADATA_BYTES as u64 {
            return Err(ArchiveError::Oversize {
                what: "metadata",
                limit: MAX_METADATA_BYTES,
            });
        }
        Ok(())
    }

    /// Absolute file span of a leaf directory named by a directory entry, or an
    /// error if the entry points outside the declared leaf-directory region.
    /// Directory entries are archive-supplied data: an entry claiming to live at
    /// offset 2^63 must not become a seek.
    fn leaf_span(&self, entry: Entry) -> Result<(u64, usize), ArchiveError> {
        let len = u64::from(entry.length);
        let end = entry
            .offset
            .checked_add(len)
            .ok_or(ArchiveError::MalformedDirectory("leaf span overflows"))?;
        if end > self.leaf_dirs_len {
            return Err(ArchiveError::MalformedDirectory(
                "leaf entry points outside the leaf-directory region",
            ));
        }
        if len > MAX_DIRECTORY_BYTES as u64 {
            return Err(ArchiveError::Oversize {
                what: "leaf directory",
                limit: MAX_DIRECTORY_BYTES,
            });
        }
        let offset = self
            .leaf_dirs_offset
            .checked_add(entry.offset)
            .ok_or(ArchiveError::MalformedDirectory("leaf offset overflows"))?;
        Ok((offset, len as usize))
    }

    /// Absolute file span of a tile body named by a directory entry, with the
    /// same containment rule as [`Self::leaf_span`].
    fn tile_span(&self, entry: Entry) -> Result<(u64, usize), ArchiveError> {
        let len = u64::from(entry.length);
        let end = entry
            .offset
            .checked_add(len)
            .ok_or(ArchiveError::MalformedDirectory("tile span overflows"))?;
        if end > self.tile_data_len {
            return Err(ArchiveError::MalformedDirectory(
                "tile entry points outside the tile-data region",
            ));
        }
        if len > MAX_TILE_BYTES as u64 {
            return Err(ArchiveError::Oversize {
                what: "tile",
                limit: MAX_TILE_BYTES,
            });
        }
        let offset = self
            .tile_data_offset
            .checked_add(entry.offset)
            .ok_or(ArchiveError::MalformedDirectory("tile offset overflows"))?;
        Ok((offset, len as usize))
    }
}

/// One directory entry: a tile id (or a run of them), and where its bytes live
/// relative to the tile-data or leaf-directory region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    tile_id: u64,
    offset: u64,
    length: u32,
    /// `0` marks a pointer to a leaf directory rather than a tile body.
    run_length: u32,
}

/// LEB128 varint as the directory format uses. Returns the value and how many
/// bytes it consumed. The shift guard is what stops a run of `0xFF` bytes from
/// shifting out of range.
fn varint(bytes: &[u8], at: usize) -> Result<(u64, usize), ArchiveError> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    let mut idx = at;
    loop {
        let byte = *bytes.get(idx).ok_or(ArchiveError::MalformedDirectory(
            "varint runs past the page",
        ))?;
        idx += 1;
        value |= u64::from(byte & 0x7f)
            .checked_shl(shift)
            .ok_or(ArchiveError::MalformedDirectory("varint overflows 64 bits"))?;
        if byte & 0x80 == 0 {
            return Ok((value, idx - at));
        }
        shift += 7;
        if shift >= 64 {
            return Err(ArchiveError::MalformedDirectory("varint overflows 64 bits"));
        }
    }
}

/// Parse a decompressed directory page: entry count, then four columns
/// (tile-id deltas, run lengths, lengths, offsets).
///
/// Entries come out sorted by construction — tile ids are cumulative sums of
/// unsigned deltas — which is what makes the binary search in [`find`] sound
/// even for an archive we did not write.
fn parse_directory(bytes: &[u8]) -> Result<Vec<Entry>, ArchiveError> {
    let (count, mut cursor) = varint(bytes, 0)?;
    if count > MAX_DIRECTORY_ENTRIES {
        return Err(ArchiveError::MalformedDirectory(
            "directory declares an implausible entry count",
        ));
    }
    let count = count as usize;
    // Never pre-allocate on the file's word alone: reserve a modest amount and
    // let the vector grow as entries are actually decoded.
    let mut entries: Vec<Entry> = Vec::with_capacity(count.min(4096));

    let mut tile_id: u64 = 0;
    for _ in 0..count {
        let (delta, used) = varint(bytes, cursor)?;
        cursor += used;
        tile_id = tile_id
            .checked_add(delta)
            .ok_or(ArchiveError::MalformedDirectory("tile id overflows"))?;
        entries.push(Entry {
            tile_id,
            offset: 0,
            length: 0,
            run_length: 0,
        });
    }
    for entry in entries.iter_mut() {
        let (run_length, used) = varint(bytes, cursor)?;
        cursor += used;
        entry.run_length = u32::try_from(run_length)
            .map_err(|_| ArchiveError::MalformedDirectory("run length exceeds u32"))?;
    }
    for entry in entries.iter_mut() {
        let (length, used) = varint(bytes, cursor)?;
        cursor += used;
        entry.length = u32::try_from(length)
            .map_err(|_| ArchiveError::MalformedDirectory("entry length exceeds u32"))?;
    }
    for idx in 0..entries.len() {
        let (raw, used) = varint(bytes, cursor)?;
        cursor += used;
        let offset = if raw == 0 {
            // 0 means "directly after the previous entry" — the run-length
            // encoding the spec uses for clustered archives.
            let previous = idx.checked_sub(1).and_then(|i| entries.get(i)).ok_or(
                ArchiveError::MalformedDirectory("first entry uses the contiguous-offset encoding"),
            )?;
            previous
                .offset
                .checked_add(u64::from(previous.length))
                .ok_or(ArchiveError::MalformedDirectory("entry offset overflows"))?
        } else {
            raw - 1
        };
        // `idx` is in range by construction of the loop.
        if let Some(entry) = entries.get_mut(idx) {
            entry.offset = offset;
        }
    }
    Ok(entries)
}

/// The entry covering `tile_id`, i.e. the last entry whose id does not exceed
/// it. Whether that entry actually *contains* the id is the caller's run-length
/// check — this only narrows the search.
fn find(entries: &[Entry], tile_id: u64) -> Option<Entry> {
    match entries.binary_search_by(|entry| entry.tile_id.cmp(&tile_id)) {
        Ok(idx) => entries.get(idx).copied(),
        Err(0) => None,
        Err(idx) => entries.get(idx - 1).copied(),
    }
}

/// Decompress a directory or metadata page, refusing anything that expands past
/// `limit` (a decompression bomb in a hostile archive must hit a ceiling, not
/// the allocator).
fn decompress(
    compression: Compression,
    bytes: &[u8],
    limit: usize,
    what: &'static str,
) -> Result<Vec<u8>, ArchiveError> {
    match compression {
        Compression::None => {
            if bytes.len() > limit {
                return Err(ArchiveError::Oversize { what, limit });
            }
            Ok(bytes.to_vec())
        }
        Compression::Gzip => {
            let mut out = Vec::new();
            // `+ 1` so an output that exactly fills the limit is still
            // distinguishable from one that was cut short by it.
            let capped = limit.saturating_add(1) as u64;
            flate2::read::GzDecoder::new(bytes)
                .take(capped)
                .read_to_end(&mut out)
                .map_err(|_| ArchiveError::MalformedDirectory("gzip page did not decode"))?;
            if out.len() > limit {
                return Err(ArchiveError::Oversize { what, limit });
            }
            Ok(out)
        }
        other => Err(ArchiveError::UnsupportedCompression {
            what,
            kind: other.label(),
        }),
    }
}

/// A tile map coordinate that has already been proven addressable: `z` within
/// the Hilbert scheme's range and `x`/`y` inside `2^z`. Constructing one is the
/// only way to reach [`Archive::tile`], so no arithmetic downstream can run off
/// the edge of the world or overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileCoord {
    z: u8,
    x: u32,
    y: u32,
}

impl TileCoord {
    /// `None` when the coordinate cannot name a tile at all: past zoom 26, or a
    /// column/row outside the `2^z × 2^z` grid.
    pub fn new(z: u8, x: u32, y: u32) -> Option<Self> {
        if z > MAX_ZOOM {
            return None;
        }
        // z <= 26, so the shift and the comparisons stay far inside u64.
        let side = 1u64 << z;
        if u64::from(x) >= side || u64::from(y) >= side {
            return None;
        }
        Some(Self { z, x, y })
    }

    pub fn z(self) -> u8 {
        self.z
    }

    /// The PMTiles tile id: the count of all tiles below this zoom, plus the
    /// Hilbert-curve index of `(x, y)` within it. Mirrors the reference
    /// implementations (go-pmtiles `ZxyToID`, pmtiles.js `zxyToTileId`),
    /// including their in-loop rotation by `s`; the wrapping subtraction is that
    /// rotation's modular arithmetic, which only ever affects bits below `s` and
    /// is exactly what the reference implementations do in fixed-width integers.
    pub fn tile_id(self) -> u64 {
        // (4^z - 1) / 3 tiles exist at zooms 0..z. z <= 26 ⇒ 2z <= 52 bits.
        let acc = ((1u64 << (2 * u32::from(self.z))) - 1) / 3;
        let (mut tx, mut ty) = (u64::from(self.x), u64::from(self.y));
        let mut d: u64 = 0;
        let mut s: u64 = (1u64 << self.z) / 2;
        while s > 0 {
            let rx = u64::from(tx & s > 0);
            let ry = u64::from(ty & s > 0);
            d += s * s * ((3 * rx) ^ ry);
            if ry == 0 {
                if rx == 1 {
                    tx = s.wrapping_sub(1).wrapping_sub(tx);
                    ty = s.wrapping_sub(1).wrapping_sub(ty);
                }
                std::mem::swap(&mut tx, &mut ty);
            }
            s /= 2;
        }
        acc + d
    }

    /// The tile's geographic footprint (Web Mercator, EPSG:3857 → WGS84). Used
    /// to decide in-region versus out-of-region *before* any file access.
    pub fn bounds(self) -> Bounds {
        let side = f64::from(1u32 << self.z);
        let lon = |x: f64| x / side * 360.0 - 180.0;
        let lat = |y: f64| {
            (std::f64::consts::PI * (1.0 - 2.0 * y / side))
                .sinh()
                .atan()
                .to_degrees()
        };
        Bounds {
            min_lon: lon(f64::from(self.x)),
            max_lon: lon(f64::from(self.x) + 1.0),
            // y grows southwards, so y+1 is the *southern* edge.
            max_lat: lat(f64::from(self.y)),
            min_lat: lat(f64::from(self.y) + 1.0),
        }
    }
}

/// Name and attribution lifted from the archive's JSON metadata.
#[derive(Debug, Clone, Default)]
pub struct ArchiveMetadata {
    pub name: Option<String>,
    /// Attribution **as plain text**. Archives ship this as HTML (`<a
    /// href=…>OpenStreetMap</a>`); markup is stripped here so the wire contract
    /// carries text the HUD renders as text — an archive file cannot inject
    /// markup or a link into the shell.
    pub attribution: Option<String>,
}

/// Strip tags and collapse whitespace, yielding `None` when nothing legible is
/// left. Deliberately blunt: this is not an HTML sanitizer, it is a "reduce to
/// text" pass on a field we will only ever render as text.
fn to_plain_text(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut in_tag = false;
    for ch in raw.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if in_tag => {
                let _ = c;
            }
            // Control characters (including bidi overrides) never reach the UI.
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then_some(collapsed)
}

impl ArchiveMetadata {
    fn parse(bytes: &[u8]) -> Self {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            // Metadata is informational: a map still serves correct tiles
            // without it, and the attribution falls back to the mandated
            // default (docs/12 §3). Warn, do not refuse to start.
            tracing::warn!("map archive metadata is not valid JSON; using defaults");
            return Self::default();
        };
        Self {
            name: value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .and_then(to_plain_text),
            attribution: value
                .get("attribution")
                .and_then(serde_json::Value::as_str)
                .and_then(to_plain_text),
        }
    }
}

/// One tile body exactly as stored, plus the address it came from (the ETag
/// ingredients). The bytes are forwarded to the client without being decoded.
#[derive(Debug, Clone)]
pub struct Tile {
    pub bytes: Vec<u8>,
    pub offset: u64,
    pub length: u32,
}

/// An opened PMTiles archive: header, root directory and metadata resident;
/// everything else read on demand.
///
/// Resident cost is the root directory (~16 KiB for a regional extract) — the
/// archive itself, hundreds of MB, is never mapped or buffered (docs/09 §5, the
/// 8 GB budget). Leaf directories are deliberately *not* cached: a regional
/// extract usually has none at all, and a cache here would add shared mutable
/// state and an invalidation question for no measurable win on a local file.
pub struct Archive {
    path: PathBuf,
    header: Header,
    root: Vec<Entry>,
    metadata: ArchiveMetadata,
    fingerprint: String,
}

impl Archive {
    /// Open and fully validate an archive. Every structural problem — wrong
    /// magic, wrong version, truncation, an unsupported codec, an unparsable
    /// root directory — surfaces here, at startup, as an error rather than at
    /// request time as a surprise.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, ArchiveError> {
        let path = path.as_ref().to_path_buf();
        let mut file = tokio::fs::File::open(&path).await?;
        let file_len = file.metadata().await?.len();
        if file_len < HEADER_LEN as u64 {
            return Err(ArchiveError::Truncated("header"));
        }

        let head = read_span(&mut file, 0, HEADER_LEN).await?;
        let header = Header::parse(&head)?;
        header.validate_spans(file_len)?;

        // Lengths are checked against MAX_* in validate_spans before this cast
        // becomes an allocation.
        let root_raw = read_span(
            &mut file,
            header.root_dir_offset,
            header.root_dir_len as usize,
        )
        .await?;
        let root = parse_directory(&decompress(
            header.internal_compression,
            &root_raw,
            MAX_DIRECTORY_BYTES,
            "root directory",
        )?)?;

        let metadata = if header.metadata_len == 0 {
            ArchiveMetadata::default()
        } else {
            let raw = read_span(
                &mut file,
                header.metadata_offset,
                header.metadata_len as usize,
            )
            .await?;
            match decompress(
                header.internal_compression,
                &raw,
                MAX_METADATA_BYTES,
                "metadata",
            ) {
                Ok(json) => ArchiveMetadata::parse(&json),
                Err(e) => {
                    tracing::warn!(error = %e, "map archive metadata could not be decoded; using defaults");
                    ArchiveMetadata::default()
                }
            }
        };

        // Identity of *this* archive, for cache validators. Derived from the
        // header (offsets, tile counts, bounds all change when an archive is
        // re-cut) plus the file length, so swapping the file changes every ETag
        // and no client keeps serving the previous region's tiles.
        let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
        sha2::Digest::update(&mut hasher, &head);
        sha2::Digest::update(&mut hasher, file_len.to_le_bytes());
        let digest = sha2::Digest::finalize(hasher);
        let fingerprint = hex::encode(&digest[..8]);

        tracing::info!(
            path = %path.display(),
            min_zoom = header.min_zoom,
            max_zoom = header.max_zoom,
            entries = root.len(),
            "opened PMTiles archive"
        );

        Ok(Self {
            path,
            header,
            root,
            metadata,
            fingerprint,
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn metadata(&self) -> &ArchiveMetadata {
        &self.metadata
    }

    /// Stable per-archive identity used as the ETag prefix.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Is this coordinate inside the archive's declared coverage? Zoom range
    /// first (cheap), then footprint overlap. A `false` here is a refusal, not a
    /// miss — the caller answers "not found", never a neighbouring tile.
    pub fn covers(&self, coord: TileCoord) -> bool {
        coord.z() >= self.header.min_zoom
            && coord.z() <= self.header.max_zoom
            && coord.bounds().intersects(&self.header.bounds)
    }

    /// Read one tile. `Ok(None)` means the archive genuinely has no tile there
    /// (an empty ocean square in a clustered extract); `Err` means the archive
    /// is structurally broken. Neither case ever returns another tile's bytes.
    ///
    /// Memory is bounded by the directory pages touched plus the tile itself;
    /// the file is opened per request and closed on return, so a long-lived
    /// process holds no descriptor per archive.
    pub async fn tile(&self, coord: TileCoord) -> Result<Option<Tile>, ArchiveError> {
        let tile_id = coord.tile_id();
        let mut file = tokio::fs::File::open(&self.path).await?;
        let mut entries: Cow<'_, [Entry]> = Cow::Borrowed(&self.root);

        for _ in 0..MAX_LEAF_DEPTH {
            let Some(entry) = find(&entries, tile_id) else {
                return Ok(None);
            };
            if entry.run_length == 0 {
                let (offset, len) = self.header.leaf_span(entry)?;
                let raw = read_span(&mut file, offset, len).await?;
                let page = decompress(
                    self.header.internal_compression,
                    &raw,
                    MAX_DIRECTORY_BYTES,
                    "leaf directory",
                )?;
                entries = Cow::Owned(parse_directory(&page)?);
                continue;
            }
            // The entry covers a *run* of ids; anything past its end is simply
            // not in the archive.
            let run_end = entry
                .tile_id
                .checked_add(u64::from(entry.run_length))
                .ok_or(ArchiveError::MalformedDirectory("run length overflows"))?;
            if tile_id >= run_end {
                return Ok(None);
            }
            let (offset, len) = self.header.tile_span(entry)?;
            let bytes = read_span(&mut file, offset, len).await?;
            return Ok(Some(Tile {
                bytes,
                offset: entry.offset,
                length: entry.length,
            }));
        }
        Err(ArchiveError::LeafDepthExceeded)
    }
}

/// Read exactly `len` bytes at `offset`. Callers cap `len` against the
/// MAX_* ceilings *before* calling — the allocation here is already bounded.
async fn read_span(
    file: &mut tokio::fs::File,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, ArchiveError> {
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf).await?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_ids_match_the_spec_hilbert_ordering() {
        // The published ordering: zoom 0 is id 0, then each zoom starts at
        // (4^z - 1)/3 and walks the Hilbert curve.
        assert_eq!(TileCoord::new(0, 0, 0).unwrap().tile_id(), 0);
        assert_eq!(TileCoord::new(1, 0, 0).unwrap().tile_id(), 1);
        assert_eq!(TileCoord::new(1, 0, 1).unwrap().tile_id(), 2);
        assert_eq!(TileCoord::new(1, 1, 1).unwrap().tile_id(), 3);
        assert_eq!(TileCoord::new(1, 1, 0).unwrap().tile_id(), 4);
        assert_eq!(TileCoord::new(2, 0, 0).unwrap().tile_id(), 5);
        // Zoom 2 holds 16 tiles, so ids 5..=20 and no more.
        assert_eq!(TileCoord::new(2, 3, 0).unwrap().tile_id(), 20);
    }

    #[test]
    fn every_tile_at_a_zoom_gets_a_distinct_id_in_range() {
        // A collision or an out-of-band id would mean serving another tile's
        // bytes — the exact failure docs/12 §3 forbids.
        for z in 0..=5u8 {
            let side = 1u32 << z;
            let base = ((1u64 << (2 * u32::from(z))) - 1) / 3;
            let mut seen = std::collections::BTreeSet::new();
            for x in 0..side {
                for y in 0..side {
                    let id = TileCoord::new(z, x, y).unwrap().tile_id();
                    assert!(id >= base, "z{z} {x}/{y} landed below its zoom band");
                    assert!(
                        id < base + u64::from(side) * u64::from(side),
                        "z{z} {x}/{y} landed above its zoom band"
                    );
                    assert!(seen.insert(id), "z{z} {x}/{y} collided with another tile");
                }
            }
        }
    }

    #[test]
    fn coordinates_outside_the_grid_are_rejected() {
        assert!(TileCoord::new(0, 1, 0).is_none());
        assert!(TileCoord::new(1, 2, 0).is_none());
        assert!(TileCoord::new(1, 0, 2).is_none());
        assert!(TileCoord::new(27, 0, 0).is_none());
        assert!(TileCoord::new(255, 0, 0).is_none());
        // The largest addressable coordinate is fine and does not overflow.
        assert!(TileCoord::new(26, (1u32 << 26) - 1, (1u32 << 26) - 1).is_some());
        assert!(TileCoord::new(26, 1u32 << 26, 0).is_none());
    }

    #[test]
    fn tile_bounds_cover_the_world_at_zoom_zero_and_narrow_with_zoom() {
        let world = TileCoord::new(0, 0, 0).unwrap().bounds();
        assert!((world.min_lon + 180.0).abs() < 1e-9);
        assert!((world.max_lon - 180.0).abs() < 1e-9);
        assert!(world.max_lat > 85.0 && world.min_lat < -85.0);

        // Berlin at zoom 10 is tile 550/335; its footprint must contain the city
        // and must not reach Cambodia.
        let berlin = TileCoord::new(10, 550, 335).unwrap().bounds();
        assert!(berlin.min_lon < 13.4 && berlin.max_lon > 13.4);
        assert!(berlin.min_lat < 52.5 && berlin.max_lat > 52.5);
        let angkor = Bounds {
            min_lon: 103.8,
            min_lat: 13.4,
            max_lon: 103.9,
            max_lat: 13.5,
        };
        assert!(!berlin.intersects(&angkor));
    }

    #[test]
    fn header_rejects_hostile_and_truncated_bytes() {
        assert!(matches!(
            Header::parse(b"not-pmtiles"),
            Err(ArchiveError::Truncated("header"))
        ));
        let mut head = vec![0u8; HEADER_LEN];
        assert!(matches!(
            Header::parse(&head),
            Err(ArchiveError::NotPmTiles)
        ));
        head[..7].copy_from_slice(MAGIC);
        head[7] = 2;
        assert!(matches!(
            Header::parse(&head),
            Err(ArchiveError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn varints_cannot_overflow_or_run_off_the_page() {
        assert_eq!(varint(&[0x00], 0).unwrap(), (0, 1));
        assert_eq!(varint(&[0x96, 0x01], 0).unwrap(), (150, 2));
        // Continuation bits forever: must error, not spin or wrap.
        let runaway = vec![0xffu8; 32];
        assert!(matches!(
            varint(&runaway, 0),
            Err(ArchiveError::MalformedDirectory(_))
        ));
        // Truncated mid-varint.
        assert!(matches!(
            varint(&[0x80], 0),
            Err(ArchiveError::MalformedDirectory(_))
        ));
    }

    #[test]
    fn directory_parse_rejects_nonsense() {
        // Claims 4 entries, supplies none.
        assert!(matches!(
            parse_directory(&[0x04]),
            Err(ArchiveError::MalformedDirectory(_))
        ));
        // Implausible entry count (varint for 2^40).
        let mut huge = Vec::new();
        let mut value: u64 = 1 << 40;
        while value >= 0x80 {
            huge.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        huge.push(value as u8);
        assert!(matches!(
            parse_directory(&huge),
            Err(ArchiveError::MalformedDirectory(_))
        ));
        // Empty directory is legal and empty.
        assert_eq!(parse_directory(&[0x00]).unwrap(), Vec::new());
    }

    #[test]
    fn attribution_is_reduced_to_plain_text() {
        let raw = "<a href=\"https://example.test\">Protomaps</a> \u{00a9} \
                   <a href=\"https://www.openstreetmap.org\"> OpenStreetMap</a>";
        assert_eq!(
            to_plain_text(raw).as_deref(),
            Some("Protomaps \u{00a9} OpenStreetMap")
        );
        assert_eq!(to_plain_text("<b></b>   "), None);
        assert_eq!(to_plain_text("plain"), Some("plain".to_owned()));
        // A script tag reduces to its text, and the tag itself never survives.
        let script = to_plain_text("<script>alert(1)</script>").unwrap();
        assert!(!script.contains('<'), "markup must not survive: {script}");
    }

    #[test]
    fn find_returns_the_covering_entry_or_nothing() {
        let entries = vec![
            Entry {
                tile_id: 10,
                offset: 0,
                length: 1,
                run_length: 1,
            },
            Entry {
                tile_id: 20,
                offset: 1,
                length: 1,
                run_length: 1,
            },
        ];
        assert_eq!(find(&entries, 9), None);
        assert_eq!(find(&entries, 10).map(|e| e.tile_id), Some(10));
        assert_eq!(find(&entries, 15).map(|e| e.tile_id), Some(10));
        assert_eq!(find(&entries, 99).map(|e| e.tile_id), Some(20));
        assert_eq!(find(&[], 1), None);
    }
}
