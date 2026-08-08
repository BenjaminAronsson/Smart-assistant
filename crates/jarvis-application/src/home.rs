//! Deterministic Home Assistant intent grammar (M4 foundation, FR-28).
//!
//! This parser only recognizes an unambiguous bounded intent. It does not
//! authorize or execute a home action: since F5.5 a recognized intent becomes a
//! `ToolProposal` for `home.set_light` (see [`crate::deterministic`]), which is
//! only ever an input to `policy::evaluate` (invariant #1) and still travels the
//! ordinary orchestrator path (invariant #2).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeAction {
    TurnOn,
    TurnOff,
}

impl HomeAction {
    /// The spelling of the `state` argument `home.set_light` accepts. The tool's
    /// own parser (F5.3) takes exactly `"on"` or `"off"` and rejects anything
    /// else as a schema violation, so the two spellings live here, next to the
    /// action they come from, rather than being re-typed at the proposal site.
    pub fn light_state(self) -> &'static str {
        match self {
            Self::TurnOn => "on",
            Self::TurnOff => "off",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeIntent {
    pub action: HomeAction,
    pub target: String,
}

/// Resolves a *spoken* light target ("living room lights") onto the concrete
/// entity id the `home.set_light` tool takes (`light.living_room`, F5.3).
///
/// The grammar cannot do this itself and must not try: which entities exist,
/// and which of them the owner allowlisted, is host configuration that lives in
/// the Home Assistant adapter — and `jarvis-application` may not depend on an
/// adapter crate (NFR-08, `cargo xtask arch-test`). So the host implements this
/// and the application relays an opaque id; the entity-id *syntax* is validated
/// where it is interpolated into a request, by the adapter's own `EntityId`.
///
/// Returning `None` is a first-class answer: an unresolvable target means the
/// utterance was **not** recognized and goes to the reasoning provider. A
/// slugified guess (`light.living_room_lights`) is exactly the kind of invention
/// this grammar refuses — it would trade a quota-costing honest answer for a
/// fail-closed denial the owner then has to decode.
pub trait LightTargetResolver: Send + Sync {
    /// The entity id for `spoken_target`, or `None` if the host does not know
    /// it. Must not perform I/O that can outlive a user's patience: this runs on
    /// the deterministic, quota-free path, whose whole point is to answer
    /// without waiting on anything.
    fn resolve_light(&self, spoken_target: &str) -> Option<String>;
}

pub fn parse_home_intent(input: &str) -> Option<HomeIntent> {
    if input.len() > 256 {
        return None;
    }
    let words: Vec<&str> = input.split_whitespace().collect();
    if words.len() < 3 {
        return None;
    }
    let action = match words[0].to_ascii_lowercase().as_str() {
        "turn" if words.get(1)?.eq_ignore_ascii_case("on") => HomeAction::TurnOn,
        "turn" if words.get(1)?.eq_ignore_ascii_case("off") => HomeAction::TurnOff,
        _ => return None,
    };
    let target = words[2..].join(" ");
    let target = target.trim().to_owned();
    if target.is_empty() || target.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(HomeIntent { action, target })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_unambiguous_turn_commands() {
        assert_eq!(
            parse_home_intent("turn on living room lights"),
            Some(HomeIntent {
                action: HomeAction::TurnOn,
                target: "living room lights".into()
            })
        );
        assert_eq!(
            parse_home_intent("TURN off desk lamp"),
            Some(HomeIntent {
                action: HomeAction::TurnOff,
                target: "desk lamp".into()
            })
        );
    }

    #[test]
    fn refuses_ambiguous_or_unbounded_text() {
        assert!(parse_home_intent("turn on").is_none());
        assert!(parse_home_intent("please turn on living room lights").is_none());
        assert!(parse_home_intent(&("turn on ".to_owned() + &"x".repeat(300))).is_none());
    }
}
