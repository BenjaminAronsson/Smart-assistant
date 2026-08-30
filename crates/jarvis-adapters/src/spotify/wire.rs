//! Resolution types and pure parsing logic — unit-testable without a
//! transport (F9.5).

use jarvis_domain::media::VolumePct;
use jarvis_domain::synthesis::clarifying_question;
use jarvis_domain::tools::sanitize_result_content;

use super::*;

// ---------------------------------------------------------------------------
// Resolution types and pure logic (unit-testable without a transport)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistRef {
    pub name: String,
    pub uri: String,
    pub genre: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRef {
    pub name: String,
    pub uri: String,
    pub artists: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistRef {
    pub name: String,
    pub uri: String,
    pub owner: Option<String>,
    pub tracks: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRef {
    pub id: Option<String>,
    pub name: String,
    pub is_active: bool,
    /// This device's own volume. `None` when Spotify omits it (devices that
    /// cannot report a level, e.g. some Connect speakers) — absent, never
    /// guessed, because a guessed level would become a false undo.
    pub volume_pct: Option<VolumePct>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchHits {
    pub artists: Vec<ArtistRef>,
    pub tracks: Vec<TrackRef>,
    pub albums: Vec<TrackRef>,
    pub playlists: Vec<PlaylistRef>,
}

/// What a resolved play request will start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlayTarget {
    /// An artist's own context: shuffle on, then `context_uri` (ADR-022 (1)).
    ArtistContext { uri: String, label: String },
    /// An album/playlist context — played in order.
    Context { uri: String, label: String },
    /// One or more explicit track URIs.
    Tracks { uris: Vec<String>, label: String },
}

pub(crate) struct PlaylistMatch {
    pub(crate) playlist: PlaylistRef,
    pub(crate) from_library: bool,
}

pub(crate) enum PlaylistLookup {
    One(PlaylistRef),
    Ambiguous(String),
    None,
}

/// Lowercase, strip punctuation, collapse whitespace — the comparison form for
/// user-chosen names ("Björn's RUNNING mix!" → "björn s running mix"). Cheap and
/// deterministic; no fuzzy-distance library enters the tree for this. Diacritics
/// are **not** folded (that needs a Unicode dependency); a query that drops them
/// falls through to the substring pass, and an unmatched name asks rather than
/// guessing.
pub(crate) fn normalize(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Bound and control-strip any Spotify-supplied text before it reaches a tool
/// result, an error, or a spoken question (Z4 discipline, invariant #5).
pub(crate) fn short(raw: &str) -> String {
    sanitize_result_content(raw, MAX_FIELD_BYTES).text
}

pub(crate) fn is_valid_device_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_DEVICE_ID_BYTES
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A Spotify URI: `spotify:<type>:<base62 id>`. Strict — a URI is about to
/// become a request parameter, so anything else is refused rather than
/// forwarded.
pub(crate) fn parse_uri(raw: &str) -> Option<(&'static str, String)> {
    let mut parts = raw.split(':');
    if parts.next()? != "spotify" {
        return None;
    }
    let kind = match parts.next()? {
        "track" => "track",
        "album" => "album",
        "artist" => "artist",
        "playlist" => "playlist",
        _ => return None,
    };
    let id = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some((kind, raw.to_owned()))
}

/// The ADR-022 (1) rule, pure: artist-only → artist context, no question.
pub(crate) fn resolve_play_target(
    query: &str,
    hits: &SearchHits,
) -> Result<PlayTarget, SpotifyError> {
    let wanted = normalize(query);
    let exact_artists: Vec<&ArtistRef> = hits
        .artists
        .iter()
        .filter(|a| normalize(&a.name) == wanted)
        .collect();

    match exact_artists.as_slice() {
        // The common case: "play ABBA" starts ABBA, shuffled. No clarification.
        [only] => {
            return Ok(PlayTarget::ArtistContext {
                uri: only.uri.clone(),
                label: short(&only.name),
            });
        }
        // Genuine multi-match: two *different* artists with the same name. Ask
        // once, fluently (ADR-016), and start nothing.
        [_, _, ..] => {
            let labels: Vec<String> = exact_artists
                .iter()
                .map(|a| match &a.genre {
                    Some(genre) if !genre.trim().is_empty() => {
                        format!("{} ({})", short(&a.name), short(genre))
                    }
                    _ => short(&a.name),
                })
                .collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let question = clarifying_question(&refs).unwrap_or_else(|| {
                format!(
                    "Two different artists on Spotify are called {}; which one did you mean?",
                    short(query)
                )
            });
            return Err(SpotifyError::Ambiguity(question));
        }
        [] => {}
    }

    if let Some(track) = hits.tracks.first() {
        return Ok(PlayTarget::Tracks {
            uris: vec![track.uri.clone()],
            label: track_label(track),
        });
    }
    if let Some(album) = hits.albums.first() {
        return Ok(PlayTarget::Context {
            uri: album.uri.clone(),
            label: track_label(album),
        });
    }
    Err(SpotifyError::NoMatch)
}

pub(crate) fn track_label(track: &TrackRef) -> String {
    if track.artists.is_empty() {
        format!("\"{}\"", short(&track.name))
    } else {
        format!(
            "\"{}\" by {}",
            short(&track.name),
            short(&track.artists.join(", "))
        )
    }
}

/// Name-match a playlist within a candidate set: exact normalized match wins;
/// otherwise substring either way (library names are user-chosen and
/// inconsistent — ADR-022). Multiple candidates ask, never guess.
pub(crate) fn match_playlist(name: &str, candidates: &[PlaylistRef]) -> PlaylistLookup {
    let wanted = normalize(name);
    if wanted.is_empty() {
        return PlaylistLookup::None;
    }
    let exact: Vec<&PlaylistRef> = candidates
        .iter()
        .filter(|p| normalize(&p.name) == wanted)
        .collect();
    let pool: Vec<&PlaylistRef> = if exact.is_empty() {
        candidates
            .iter()
            .filter(|p| {
                let got = normalize(&p.name);
                got.contains(&wanted) || wanted.contains(&got)
            })
            .collect()
    } else {
        exact
    };

    match pool.as_slice() {
        [] => PlaylistLookup::None,
        [only] => PlaylistLookup::One((*only).clone()),
        many => {
            let labels: Vec<String> = many
                .iter()
                .map(|p| match p.tracks {
                    Some(total) => format!("{} ({total} tracks)", short(&p.name)),
                    None => short(&p.name),
                })
                .collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let question = clarifying_question(&refs).unwrap_or_else(|| {
                format!(
                    "You have more than one playlist called {}; which one did you mean?",
                    short(name)
                )
            });
            PlaylistLookup::Ambiguous(question)
        }
    }
}

// ---------------------------------------------------------------------------
// Response classification and parsing (pure)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub(crate) struct ErrorEnvelope {
    error: Option<ErrorBody>,
}

#[derive(serde::Deserialize)]
pub(crate) struct ErrorBody {
    #[serde(default)]
    message: String,
    #[serde(default)]
    reason: String,
}

/// Map a raw response to a domain outcome. The Premium and no-device cases are
/// *named* outcomes, not generic HTTP failures, because the honest answer to the
/// human differs completely (docs/02 §11a).
pub(crate) fn classify(response: ApiResponse) -> Result<ApiResponse, SpotifyError> {
    if response.is_success() {
        return Ok(response);
    }
    let body: Option<ErrorBody> = serde_json::from_str::<ErrorEnvelope>(&response.body)
        .ok()
        .and_then(|e| e.error);
    let reason = body.as_ref().map(|b| b.reason.to_ascii_uppercase());
    let message = body.as_ref().map(|b| b.message.to_lowercase());
    let says_premium = reason.as_deref() == Some("PREMIUM_REQUIRED")
        || message.as_deref().is_some_and(|m| m.contains("premium"));
    let says_no_device = reason.as_deref() == Some("NO_ACTIVE_DEVICE")
        || message
            .as_deref()
            .is_some_and(|m| m.contains("no active device"));

    match response.status {
        401 => Err(SpotifyError::AuthExpired),
        403 if says_premium => Err(SpotifyError::PremiumRequired),
        404 if says_no_device => Err(SpotifyError::NoActiveDevice),
        // A 404 from a player endpoint with no body detail is, in practice,
        // "there is nothing to control" — say that rather than "HTTP 404".
        404 if body.is_none() => Err(SpotifyError::NoActiveDevice),
        429 => Err(SpotifyError::RateLimited {
            retry_after_secs: response.retry_after_secs.unwrap_or(1),
        }),
        status => Err(SpotifyError::Api { status }),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct Page<T> {
    #[serde(default = "Vec::new")]
    items: Vec<Option<T>>,
}

#[derive(serde::Deserialize)]
pub(crate) struct SearchEnvelope {
    artists: Option<Page<ArtistObj>>,
    tracks: Option<Page<TrackObj>>,
    albums: Option<Page<TrackObj>>,
    playlists: Option<Page<PlaylistObj>>,
}

#[derive(serde::Deserialize)]
pub(crate) struct ArtistObj {
    #[serde(default)]
    name: String,
    uri: Option<String>,
    #[serde(default)]
    genres: Vec<String>,
}

#[derive(serde::Deserialize)]
pub(crate) struct TrackObj {
    #[serde(default)]
    name: String,
    uri: Option<String>,
    #[serde(default)]
    artists: Vec<NameObj>,
}

#[derive(serde::Deserialize)]
pub(crate) struct NameObj {
    #[serde(default)]
    name: String,
}

#[derive(serde::Deserialize)]
pub(crate) struct PlaylistObj {
    #[serde(default)]
    name: String,
    uri: Option<String>,
    owner: Option<OwnerObj>,
    tracks: Option<TotalObj>,
}

#[derive(serde::Deserialize)]
pub(crate) struct OwnerObj {
    display_name: Option<String>,
}

#[derive(serde::Deserialize)]
pub(crate) struct TotalObj {
    total: Option<u32>,
}

pub(crate) fn artist_from(obj: ArtistObj) -> Option<ArtistRef> {
    let uri = obj.uri.filter(|u| parse_uri(u).is_some())?;
    Some(ArtistRef {
        name: obj.name,
        uri,
        genre: obj.genres.into_iter().next(),
    })
}

pub(crate) fn track_from(obj: TrackObj) -> Option<TrackRef> {
    let uri = obj.uri.filter(|u| parse_uri(u).is_some())?;
    Some(TrackRef {
        name: obj.name,
        uri,
        artists: obj.artists.into_iter().map(|a| a.name).collect(),
    })
}

pub(crate) fn playlist_from(obj: PlaylistObj) -> Option<PlaylistRef> {
    let uri = obj.uri.filter(|u| parse_uri(u).is_some())?;
    Some(PlaylistRef {
        name: obj.name,
        uri,
        owner: obj.owner.and_then(|o| o.display_name),
        tracks: obj.tracks.and_then(|t| t.total),
    })
}

/// Spotify's search payload legitimately contains `null` entries in `items`
/// (a known API quirk); they are dropped, never treated as a parse failure.
pub(crate) fn parse_search(body: &str) -> Result<SearchHits, SpotifyError> {
    let parsed: SearchEnvelope =
        serde_json::from_str(body).map_err(|_| SpotifyError::InvalidResponse)?;
    Ok(SearchHits {
        artists: parsed
            .artists
            .map(|p| p.items)
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .filter_map(artist_from)
            .collect(),
        tracks: parsed
            .tracks
            .map(|p| p.items)
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .filter_map(track_from)
            .collect(),
        albums: parsed
            .albums
            .map(|p| p.items)
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .filter_map(track_from)
            .collect(),
        playlists: parsed
            .playlists
            .map(|p| p.items)
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .filter_map(playlist_from)
            .collect(),
    })
}

pub(crate) fn parse_playlist_page(body: &str) -> Result<Vec<PlaylistRef>, SpotifyError> {
    let parsed: Page<PlaylistObj> =
        serde_json::from_str(body).map_err(|_| SpotifyError::InvalidResponse)?;
    Ok(parsed
        .items
        .into_iter()
        .flatten()
        .filter_map(playlist_from)
        .collect())
}

#[derive(serde::Deserialize)]
pub(crate) struct DevicesEnvelope {
    #[serde(default = "Vec::new")]
    devices: Vec<Option<DeviceObj>>,
}

#[derive(serde::Deserialize)]
pub(crate) struct DeviceObj {
    id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    volume_percent: Option<i64>,
}

pub(crate) fn parse_devices(body: &str) -> Result<Vec<DeviceRef>, SpotifyError> {
    let parsed: DevicesEnvelope =
        serde_json::from_str(body).map_err(|_| SpotifyError::InvalidResponse)?;
    Ok(parsed
        .devices
        .into_iter()
        .flatten()
        .map(|d| DeviceRef {
            id: d.id,
            name: d.name,
            is_active: d.is_active,
            // An out-of-range level is dropped rather than clamped: a clamped
            // value would be a plausible-looking lie in the undo string.
            volume_pct: d.volume_percent.and_then(|v| VolumePct::from_i64(v).ok()),
        })
        .collect())
}
