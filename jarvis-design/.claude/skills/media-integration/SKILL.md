---
name: media-integration
description: Implementing media control - MPRIS adapter, Spotify Web API, cast-a-link media window, media bar. Use whenever touching jarvis-adapters media code or the media UI.
---

# Media integration

Spec: docs/02 §11a, ADR-012, UI in docs/12 §5 (media bar). Netflix: NO integration -
reject any request to add search/browse/automation for it; MPRIS + window focus is the
ceiling (ADR-012 is Accepted).

1. **MPRIS adapter (zbus)**: discover `org.mpris.MediaPlayer2.*` names on the session
   bus; subscribe to PropertiesChanged (event-driven - no polling, perf-warden checks);
   normalize to a `MediaState` DTO feeding the transient `media.state` WS event. Player
   appearing/disappearing must not error runs - media tools return a clean
   "no active player" result. Tools: media.playback {play|pause|next|previous|seek},
   media.volume (R1 up to max_volume_pct; above => R2 approval - snapshot-test the
   exact_effect string "set volume to 85% (above 70% cap)").
2. **Multiple players**: target by player identity; default = the most recently active.
   "pause the music" in the deterministic grammar maps to active player - test the
   ambiguity case (two players active => ask, don't guess). The ask is the ADR-016
   fluent single-question pattern - one natural spoken question, NEVER a picker; do not
   invent a different ask-mechanism here just because this is a media skill.
3. **Spotify adapter**: OAuth authorization-code + PKCE against the owner's own
   developer app; refresh token in keyring (never DB); scopes minimal
   (user-read-playback-state, user-modify-playback-state, playlist read only unless
   playlist tools enabled). Detect non-Premium from the 403 PREMIUM_REQUIRED response
   and surface a clear UI state - do not retry-loop. Fixtures for: token refresh,
   429 Retry-After (honor it), device list, PREMIUM_REQUIRED, revoked token.
4. **Cast-a-link**: media.open_url validates scheme https, launches/reuses the
   jarvis-media Chromium app-id via jarvis-agent on the configured display; the media
   profile has NO credentials and is separate from the browser-worker profiles. URL is
   shown verbatim in the R1 audit event.
5. **Voice/degraded**: transport verbs (pause/play/next/volume down) live in the
   deterministic grammar - zero LLM calls (test: quota-exhausted + "pause" still works).
6. Media bar (Angular): driven only by media.state events; absent when idle+disabled;
   popover = Spotify search, cast input, device picker. Follow docs/12 tokens - no
   album-art dominance, it's an instrument panel, not a jukebox.
7. **Artist-play default** (ADR-022): `spotify.play` resolving to an artist-only match
   starts that artist's context (shuffled top tracks / artist radio) - no clarification
   for the common case of naming an artist. Reserve clarification for genuine multi-match
   (two distinct artists sharing a name), via the ADR-016 pattern.
8. **Playlist-by-name** (ADR-022): `spotify.play_playlist { name }` searches the user's
   OWN saved playlists first (playlist-read scope - now actually wired to a capability,
   not just anticipated); public catalog search is fallback only if nothing matches in
   the user's library. Fuzzy/partial name matching needed since library playlist names
   are user-chosen and inconsistent; ambiguous matches go through ADR-016, not a picker.
9. **Now-playing query** (ADR-022, FR-32): "what's playing/what is this song" is answered
   from the same MPRIS metadata already feeding the media bar - spoken answer plus a
   now-playing card (title/artist/album, `mpris:artUrl` when the active player exposes
   it - Spotify desktop does, other players may not). No new adapter or tool call - this
   is a routing/card-grammar addition over data already being collected. Test: art
   present (Spotify) and art absent (a player with no artUrl) both render correctly, the
   latter as a text-only card per the standard no-fabricated-image rule.
