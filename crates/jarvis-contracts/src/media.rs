//! Media wire DTOs (FR-22, docs/02 §11a, ADR-012, docs/12 §2.3).
//!
//! Three surfaces:
//!
//! * [`MediaStateDto`] — the payload of the **transient** `media.state` WS event
//!   that drives the media bar. Transient because it is a *current-value*
//!   readout, not a timeline fact: a client that missed one is not behind, it
//!   just has a stale value, and the next change (or [`MediaStateResponse`] on
//!   connect) corrects it. Replaying "was playing X at 14:02" into a timeline
//!   would be meaningless (docs/05 §3 persistence classification).
//! * [`MediaCommandRequest`] / [`MediaCommandResponse`] — the REST body for
//!   `POST /api/v1/media/command`, the **owner-driven** control path the media
//!   bar's pause button uses (exit evidence #4). This is an authenticated human
//!   action on their own device, the same shape as `POST …/artifacts/{id}/open`
//!   — the model's path to the same effect is the registered `media.playback`
//!   tool through `policy::evaluate`, never this endpoint.
//! * [`MediaStateResponse`] — `GET /api/v1/media/state`, so a client that just
//!   connected has a value before the first change arrives.
//!
//! Every string that originated with a *player* (identity, title, artist, album,
//! art URL) is Z4-untrusted content, sanitized in `jarvis_domain::media` before
//! it is projected here. The client renders these as text only.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Wire mirror of `jarvis_domain::media::PlaybackStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStatusDto {
    Playing,
    Paused,
    Stopped,
}

impl From<jarvis_domain::media::PlaybackStatus> for PlaybackStatusDto {
    fn from(status: jarvis_domain::media::PlaybackStatus) -> Self {
        use jarvis_domain::media::PlaybackStatus as S;
        match status {
            S::Playing => Self::Playing,
            S::Paused => Self::Paused,
            S::Stopped => Self::Stopped,
        }
    }
}

/// Track metadata as published by the player — **untrusted display data**
/// (docs/06 §2 Z4). Absent fields are omitted rather than blank so the card can
/// render "unknown" instead of an empty line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrackMetadataDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// `https`-only album art (the domain drops every other scheme). The client
    /// may render it directly; a player cannot use this field to make the shell
    /// fetch a local file or an internal address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub art_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length_secs: Option<u64>,
}

impl From<&jarvis_domain::media::TrackMetadata> for TrackMetadataDto {
    fn from(meta: &jarvis_domain::media::TrackMetadata) -> Self {
        Self {
            title: meta.title.clone(),
            artist: meta.artist.clone(),
            album: meta.album.clone(),
            art_url: meta.art_url.clone(),
            length_secs: meta.length.map(|d| d.as_secs()),
        }
    }
}

/// One player's state on the media bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaPlayerDto {
    /// The MPRIS bus name (`org.mpris.MediaPlayer2.spotify`) — the handle a
    /// command targets. Validated as a bus name server-side on the way back in.
    pub player: String,
    /// Human-readable player name ("Spotify"), sanitized player-published text.
    pub identity: String,
    pub status: PlaybackStatusDto,
    pub metadata: TrackMetadataDto,
    /// Current volume in whole percent, when the player exposes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_pct: Option<u8>,
    /// What the player says it supports — the bar disables the rest rather than
    /// offering a control that will silently do nothing.
    pub can_play: bool,
    pub can_pause: bool,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub can_seek: bool,
}

impl From<&jarvis_domain::media::PlayerState> for MediaPlayerDto {
    fn from(state: &jarvis_domain::media::PlayerState) -> Self {
        Self {
            player: state.player.to_string(),
            identity: state.identity.clone(),
            status: state.status.into(),
            metadata: (&state.metadata).into(),
            volume_pct: state.volume.map(|v| v.get()),
            can_play: state.can_play,
            can_pause: state.can_pause,
            can_go_next: state.can_go_next,
            can_go_previous: state.can_go_previous,
            can_seek: state.can_seek,
        }
    }
}

/// Payload of the transient `media.state` event and of `GET …/media/state`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaStateDto {
    /// Players currently on the session bus, in a stable order. Empty means
    /// nothing is running — the bar hides itself rather than showing an empty
    /// shell.
    pub players: Vec<MediaPlayerDto>,
    /// The player an untargeted command applies to, when exactly one is
    /// unambiguous. `null` with a non-empty `players` means the choice is
    /// genuinely ambiguous (two players playing) — the bar shows both and the
    /// voice path asks (ADR-016), it never guesses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_player: Option<String>,
    /// The configured volume cap in percent. The bar clamps its own slider to
    /// this; the server enforces it regardless (hearing protection, docs/02
    /// §11a).
    pub max_volume_pct: u8,
}

impl MediaStateDto {
    /// Project a domain snapshot, resolving the unambiguous target.
    pub fn from_snapshot(
        snapshot: &jarvis_domain::media::MediaSnapshot,
        max_volume: jarvis_domain::media::VolumePct,
    ) -> Self {
        use jarvis_domain::media::TargetSelection;
        Self {
            players: snapshot
                .players()
                .iter()
                .map(MediaPlayerDto::from)
                .collect(),
            active_player: match snapshot.target() {
                TargetSelection::One(id) => Some(id.to_string()),
                TargetSelection::None | TargetSelection::Ambiguous(_) => None,
            },
            max_volume_pct: max_volume.get(),
        }
    }
}

/// `GET /api/v1/media/state`. Present because `media.state` is transient and
/// therefore never replayed on resync (docs/05 §3): a client fetches the current
/// value once on connect and follows events afterwards.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaStateResponse {
    pub state: MediaStateDto,
    /// False when media control is not configured or no session bus is present.
    /// The bar renders nothing at all rather than dead buttons.
    pub available: bool,
}

/// `POST /api/v1/media/command` — the owner-driven transport control behind the
/// media bar (exit evidence #4).
///
/// `command` is one of the closed transport verbs (`play`, `pause`,
/// `play_pause`, `stop`, `next`, `previous`, `seek`, `set_volume`); unknown
/// verbs are a 400, never forwarded. Omitting `player` targets the unambiguous
/// active player and fails cleanly (409) when the choice is ambiguous — the
/// server never picks one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaCommandRequest {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub player: Option<String>,
    /// Required by `seek`; ignored otherwise. Negative rewinds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_secs: Option<i64>,
    /// Required by `set_volume`. **Must not exceed the configured cap** — the
    /// media bar cannot raise volume above it at all; going higher is the R2
    /// approved tool path, deliberately not reachable from a UI button
    /// (docs/02 §11a).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_pct: Option<i64>,
}

/// Response to `POST …/media/command`: the effect that was audited and applied,
/// plus the state immediately afterwards so the bar re-renders without waiting
/// for the event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaCommandResponse {
    /// The verb that was applied (echo of the request's `command`).
    pub command: String,
    /// The player it was applied to.
    pub player: String,
    pub state: MediaStateDto,
}
