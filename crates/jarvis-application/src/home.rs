//! Deterministic Home Assistant intent grammar (M4 foundation, FR-28).
//!
//! This parser only recognizes an unambiguous bounded intent. It does not
//! authorize or execute a home action; any future adapter still goes through
//! the policy/grant path.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeAction {
    TurnOn,
    TurnOff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeIntent {
    pub action: HomeAction,
    pub target: String,
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
