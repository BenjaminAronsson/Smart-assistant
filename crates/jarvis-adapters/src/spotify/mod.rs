//! Spotify Web API adapter (F5.6, FR-21, ADR-012, ADR-022, docs/02 §11a).
//!
//! Service-level music actions that MPRIS cannot express: search the catalogue,
//! start playback of a resolved URI on a chosen Spotify Connect device, queue a
//! track, and set that device's volume. Local transport control stays with the
//! MPRIS adapter (`media_mpris`, ADR-012 tier 1) — this adapter is tier 2.
//!
//! **Authorization.** OAuth authorization-code + PKCE. The host resolves the
//! refresh token from the keyring and hands it to [`SpotifyConfig`] already
//! resolved (the [`crate::smtp::SmtpConfig`] pattern): this adapter never reads
//! a secret store, never logs a token, never puts one in a [`ToolError`], and
//! neither [`SpotifyConfig`] nor [`AccessToken`] can print its secret through
//! `Debug` (invariant #5). The *enrollment* flow that mints the first refresh
//! token (browser consent + code exchange) is deliberately out of this adapter:
//! an adapter that could open a consent window would be a side-effect path that
//! did not come from the policy engine.
//!
//! **The volume cap is a tool boundary, not an executor courtesy.**
//! `policy::evaluate` classifies a proposal by the registered tool's
//! [`ToolPolicy`] and never inspects its arguments (docs/06 §3) — the same
//! constraint the M3a MPRIS work hit. So the cap is enforced *twice over*:
//! [`SpotifyVolumeTool`] (R1) has no code path that can emit an above-cap
//! request — it refuses before any transport call — and above-cap levels live
//! only in [`SpotifyVolumeBoostTool`] (R2), which parks for human approval and
//! re-validates the grant — tool, version, single use, argument hash, target
//! resource **and expiry** ([`check_grant`]) — before acting, so a direct
//! invocation of the executor cannot bypass the validator that already ran.
//! `spotify.play`'s optional
//! `volume_pct` is checked by the same one function, first thing, before a
//! single byte reaches Spotify.
//!
//! **Premium is detected, never assumed** (docs/02 §11a). Spotify's player
//! endpoints answer a free account with `403 … PREMIUM_REQUIRED`; that becomes
//! [`SpotifyError::PremiumRequired`] and an honest tool error — never a silent
//! no-op reported as success.
//!
//! **Resolution defaults (ADR-022).** An artist-only match starts that artist's
//! shuffled context with **no** clarifying question (the common case);
//! `spotify.play_playlist` searches the owner's *own* saved playlists before it
//! will look at the public catalogue. Genuine multi-match ambiguity asks one
//! fluent spoken question (ADR-016, [`clarifying_question`]) and performs no
//! action — never a picker.
//!
//! Everything is bounded: per-request timeout, capped response bodies, capped
//! pagination, one bounded `Retry-After`-honouring retry, and every await is
//! cancellable (invariant #4).

use std::time::Duration;

mod client;
mod tools;
mod wire;

pub use client::*;
pub use tools::*;
pub use wire::*;

// ---------------------------------------------------------------------------
// Bounds and endpoints
// ---------------------------------------------------------------------------

/// Web API base. Every request path this adapter builds is a static literal
/// suffix of this constant — no model-supplied text becomes a path segment.
pub const API_BASE: &str = "https://api.spotify.com/v1";
/// OAuth token endpoint (accounts host, not the API host).
pub const TOKEN_ENDPOINT: &str = "https://accounts.spotify.com/api/token";

/// The minimal scope set (docs/02 §11a: "playback/read/playlist-read"). Stated
/// here so the enrollment flow and this adapter cannot drift apart. Note the
/// absence of any `playlist-modify-*`: this adapter performs **no library
/// mutation**, so it must not hold the authority to.
pub const OAUTH_SCOPES: &[&str] = &[
    "user-read-playback-state",
    "user-modify-playback-state",
    "playlist-read-private",
    "playlist-read-collaborative",
];

/// Per-request wall clock for one Spotify call.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Connect timeout — a fast clean failure rather than hanging on a dead route.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Hard cap on any response body, streamed, so a misbehaving upstream cannot
/// grow memory even if it lies about `Content-Length`.
pub(crate) const MAX_BODY_BYTES: usize = 256 * 1024;
/// Refresh the access token this far before it actually expires.
pub(crate) const TOKEN_REFRESH_SKEW: Duration = Duration::from_secs(60);
/// Longest `Retry-After` this adapter will wait out inline; anything longer is
/// surfaced to the caller instead of silently stalling a run.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(5);
/// Page size and page count for the owner's saved-playlist sweep. 4 × 50 = 200
/// playlists — generous for a personal library and a hard bound on the work one
/// `play_playlist` call can do.
pub(crate) const PLAYLIST_PAGE_LIMIT: usize = 50;
pub(crate) const MAX_PLAYLIST_PAGES: usize = 4;
/// Result-count bounds for `spotify.search`.
pub(crate) const DEFAULT_SEARCH_LIMIT: i64 = 5;
pub(crate) const MAX_SEARCH_LIMIT: i64 = 20;
/// Longest single Spotify-supplied string (track/artist/playlist name) kept in
/// tool output, after sanitisation.
pub(crate) const MAX_FIELD_BYTES: usize = 256;
/// Longest accepted free-text query — a bound on what reaches the provider.
pub(crate) const MAX_QUERY_BYTES: usize = 200;
/// Longest accepted Connect device id / alias.
pub(crate) const MAX_DEVICE_ID_BYTES: usize = 128;

/// Scope for the read-only catalogue search.
pub(crate) const SEARCH_SCOPE: &str = "media:search";
/// Scope for anything that changes what is playing. Shared with the MPRIS media
/// tools: a run that may control media may control it wherever it plays.
pub(crate) const CONTROL_SCOPE: &str = "media:control";

#[cfg(test)]
mod tests;
