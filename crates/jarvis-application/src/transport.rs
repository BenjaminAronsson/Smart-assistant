//! Deterministic media-transport grammar (M5/F5.5, FR-13, docs/03 §4
//! "quota-first routing").
//!
//! Recognizes a **closed** set of transport utterances ("pause the music",
//! "skip", "next track") and maps them onto
//! [`jarvis_domain::media::TransportCommand`] — the very type the `media.playback`
//! tool parses its `command` argument into. Sharing the domain type is what
//! makes "the grammar cannot emit a verb the tool would reject" a compile-time
//! property instead of a convention: the wire spelling is
//! [`TransportCommand::as_str`], the inverse of the parser the executor calls.
//!
//! **Recognition is not authorization.** This module returns an *intent*; the
//! caller ([`crate::deterministic`]) turns it into a `ToolProposal`, which is
//! only ever an input to `policy::evaluate` (invariant #1). Nothing here
//! executes, and no recognized utterance shortcuts the state machine
//! (invariant #2).
//!
//! Unrecognized input is never guessed — an utterance this table does not
//! cover falls through to the reasoning provider, which costs quota but is the
//! only honest answer.

use jarvis_domain::media::TransportCommand;

/// Longest utterance the grammar will even look at. Transport phrasing is
/// short; a long input is prose, not a command.
const MAX_UTTERANCE_BYTES: usize = 128;

/// The recognized verbs.
///
/// `bare_ok` marks the verbs that mean **only** a media transport in this
/// system's vocabulary, and may therefore stand alone:
///
/// * `pause` / `resume` / `unpause` / `next` / `skip` / `previous` — nothing
///   else in Jarvis is paused, resumed or skipped today.
/// * `stop` on its own is deliberately **not** accepted: "stop" is what a user
///   says to a running timer, to a speaking assistant (barge-in, F5.2) and to a
///   long answer, and `Stop` is the one transport verb that discards playback
///   position. It is recognized only with an explicit media object.
/// * `play` on its own is deliberately **not** accepted either: "play …" is
///   overwhelmingly a *content* request ("play some jazz"), which is the
///   Spotify surface's job (F5.6), not a transport verb. Only `play <media
///   object>` — an explicit "resume what is playing" — is recognized here.
///
/// Ordering is irrelevant to correctness (no entry is a prefix of another at a
/// word boundary) but the table is grouped by effect for readability.
const VERBS: &[(&str, TransportCommand, bool)] = &[
    ("pause", TransportCommand::Pause, true),
    ("unpause", TransportCommand::Play, true),
    ("resume", TransportCommand::Play, true),
    ("continue", TransportCommand::Play, false),
    ("play", TransportCommand::Play, false),
    ("stop", TransportCommand::Stop, false),
    ("next", TransportCommand::Next, true),
    ("skip", TransportCommand::Next, true),
    ("previous", TransportCommand::Previous, true),
];

/// The closed set of objects a verb may take. An object outside this set is not
/// a near-miss to be guessed at — it means the utterance is about something
/// else ("stop the timer", "skip ahead 30 seconds", "play some jazz"), so the
/// whole match is refused and the provider decides.
const OBJECTS: &[&str] = &[
    "it",
    "music",
    "the music",
    "my music",
    "song",
    "the song",
    "this song",
    "the current song",
    "track",
    "the track",
    "this track",
    "the current track",
    "playback",
    "the playback",
    "audio",
    "the audio",
    "video",
    "the video",
    "movie",
    "the movie",
    "podcast",
    "the podcast",
    "episode",
    "the episode",
];

/// Parse one utterance into a transport verb, or `None` when it is not
/// unambiguously a transport command.
///
/// Deliberately **not** recognized (each falls through to the provider):
/// bare "stop"/"play"; any seek ("skip ahead 30 seconds" — a relative seek needs
/// an offset argument, and mis-parsing a duration is worse than asking);
/// content requests ("play some jazz"); anything with a politeness prefix
/// ("please pause the music"), matching the M4 home grammar's own refusal of
/// "please turn on …" rather than inventing a second, looser convention.
pub fn parse_transport_intent(input: &str) -> Option<TransportCommand> {
    let text = normalize(input)?;
    for (verb, command, bare_ok) in VERBS {
        let Some(rest) = text.strip_prefix(verb) else {
            continue;
        };
        if rest.is_empty() {
            return bare_ok.then_some(*command);
        }
        // Require a word boundary, so "playback" is not read as "play" + "back".
        let Some(object) = rest.strip_prefix(' ') else {
            continue;
        };
        // A verb we know with an object we do not: refuse outright rather than
        // trying a different verb or guessing the object away.
        return OBJECTS.contains(&object).then_some(*command);
    }
    None
}

/// Lowercase, collapse whitespace, drop trailing sentence punctuation. Control
/// characters are refused rather than stripped: they have no place in a spoken
/// or typed transport command, and refusing keeps this grammar from being a
/// second, weaker sanitizer next to `tools::sanitize_result_content`.
fn normalize(input: &str) -> Option<String> {
    if input.len() > MAX_UTTERANCE_BYTES {
        return None;
    }
    let trimmed = input.trim().trim_end_matches(['.', '!', '?', ',']);
    if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
        return None;
    }
    let mut normalized = String::with_capacity(trimmed.len());
    for word in trimmed.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.push_str(&word.to_ascii_lowercase());
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_the_bounded_transport_phrasing() {
        let cases = [
            ("pause the music", TransportCommand::Pause),
            ("Pause the music.", TransportCommand::Pause),
            ("  pause   the   music  ", TransportCommand::Pause),
            ("pause", TransportCommand::Pause),
            ("pause it", TransportCommand::Pause),
            ("resume", TransportCommand::Play),
            ("resume the music", TransportCommand::Play),
            ("unpause", TransportCommand::Play),
            ("play the music", TransportCommand::Play),
            ("continue the podcast", TransportCommand::Play),
            ("stop the music", TransportCommand::Stop),
            ("skip", TransportCommand::Next),
            ("skip this song", TransportCommand::Next),
            ("next", TransportCommand::Next),
            ("next track", TransportCommand::Next),
            ("previous", TransportCommand::Previous),
            ("previous track", TransportCommand::Previous),
        ];
        for (utterance, expected) in cases {
            assert_eq!(
                parse_transport_intent(utterance),
                Some(expected),
                "{utterance:?}"
            );
        }
    }

    #[test]
    fn refuses_everything_it_is_not_sure_about() {
        let refused = [
            // Ambiguous across surfaces — timers, barge-in, a long answer.
            "stop",
            "stop the timer",
            // A content request, not a transport verb (Spotify's surface).
            "play",
            "play some jazz",
            "play the beatles",
            // A seek needs an offset argument; mis-parsing a duration is worse
            // than asking.
            "skip ahead 30 seconds",
            "skip forward",
            // Politeness prefixes are not stripped (same rule as the M4 home
            // grammar).
            "please pause the music",
            "hey jarvis pause the music",
            // Near-misses and prose.
            "playback",
            "playing",
            "pauses",
            "why did the music pause",
            "tell me a story",
            "",
            "   ",
        ];
        for utterance in refused {
            assert_eq!(parse_transport_intent(utterance), None, "{utterance:?}");
        }
    }

    #[test]
    fn refuses_an_utterance_longer_than_the_bound_even_if_it_starts_with_a_verb() {
        let long = format!("pause the music {}", "x".repeat(MAX_UTTERANCE_BYTES));
        assert_eq!(parse_transport_intent(&long), None);
    }

    #[test]
    fn refuses_control_characters_rather_than_stripping_them() {
        assert_eq!(parse_transport_intent("pause the music\u{7}"), None);
        assert_eq!(parse_transport_intent("pause\nskip"), None);
    }

    /// The grammar may only produce verbs `media.playback` accepts: every
    /// recognized command must round-trip through the *same* domain parser the
    /// executor calls (`TransportCommand::parse`), with no `offset_secs`.
    #[test]
    fn every_recognized_verb_round_trips_through_the_tools_own_parser() {
        for (utterance, _, _) in VERBS {
            let phrase = format!("{utterance} the music");
            let Some(command) = parse_transport_intent(&phrase) else {
                panic!("{phrase:?} must be recognized");
            };
            assert_eq!(
                TransportCommand::parse(command.as_str(), None),
                Ok(command),
                "{phrase:?} produced a verb the media tool would reject"
            );
        }
    }

    /// No `Seek` may escape this grammar: it is the one verb whose argument
    /// (`offset_secs`) the transport table cannot supply.
    #[test]
    fn never_produces_a_seek() {
        for (verb, command, _) in VERBS {
            assert!(
                !matches!(command, TransportCommand::Seek { .. }),
                "{verb} maps to a seek"
            );
        }
    }
}
