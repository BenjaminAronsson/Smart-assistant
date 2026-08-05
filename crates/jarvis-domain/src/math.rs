//! Deterministic arithmetic and unit conversion (M4, docs/02 §11e).
//!
//! This intentionally small grammar is resolved before a model run.  Its only
//! inputs are an owner utterance and fixed conversion constants, which makes
//! common requests work offline and consume zero reasoning-provider quota.

/// Longest utterance accepted by the grammar.  This bounds work on an
/// untrusted transcript without making ordinary spoken requests awkward.
pub const MAX_UTTERANCE_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub enum MathCommand {
    Percentage { percent: f64, of: f64 },
    Convert { value: f64, from: Unit, to: Unit },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Kilometre,
    Mile,
    Metre,
    Foot,
    Centimetre,
    Inch,
}

impl Unit {
    fn parse(raw: &str) -> Option<Self> {
        match raw
            .trim_end_matches(['.', '?', '!'])
            .to_ascii_lowercase()
            .as_str()
        {
            "km" | "kilometre" | "kilometres" | "kilometer" | "kilometers" => Some(Self::Kilometre),
            "mi" | "mile" | "miles" => Some(Self::Mile),
            "m" | "metre" | "metres" | "meter" | "meters" => Some(Self::Metre),
            "ft" | "foot" | "feet" => Some(Self::Foot),
            "cm" | "centimetre" | "centimetres" | "centimeter" | "centimeters" => {
                Some(Self::Centimetre)
            }
            "in" | "inch" | "inches" => Some(Self::Inch),
            _ => None,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Self::Kilometre => "km",
            Self::Mile => "mi",
            Self::Metre => "m",
            Self::Foot => "ft",
            Self::Centimetre => "cm",
            Self::Inch => "in",
        }
    }

    fn metres(self) -> f64 {
        match self {
            Self::Kilometre => 1_000.0,
            Self::Mile => 1_609.344,
            Self::Metre => 1.0,
            Self::Foot => 0.3048,
            Self::Centimetre => 0.01,
            Self::Inch => 0.0254,
        }
    }
}

/// A rendered deterministic answer. `expression` is retained so callers can
/// show precisely what was understood rather than an unexplained number.
#[derive(Debug, Clone, PartialEq)]
pub struct MathResult {
    pub expression: String,
    pub value: f64,
    pub unit: Option<Unit>,
}

impl MathCommand {
    pub fn evaluate(&self) -> Option<MathResult> {
        let result = match *self {
            Self::Percentage { percent, of } => MathResult {
                expression: format_number(percent) + "% of " + &format_number(of),
                value: percent * of / 100.0,
                unit: None,
            },
            Self::Convert { value, from, to } => MathResult {
                expression: format!(
                    "{} {} to {}",
                    format_number(value),
                    from.symbol(),
                    to.symbol()
                ),
                value: value * from.metres() / to.metres(),
                unit: Some(to),
            },
        };
        result.value.is_finite().then_some(result)
    }
}

/// Parse an unambiguous percentage or distance conversion request. Everything
/// else returns `None` for the ordinary routing path; the grammar never guesses.
pub fn parse_math_command(raw: &str) -> Option<MathCommand> {
    if raw.len() > MAX_UTTERANCE_BYTES {
        return None;
    }
    let text = raw.trim().to_ascii_lowercase();
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() == 3 && words[1] == "of" {
        let percent = words[0].strip_suffix('%')?.parse::<f64>().ok()?;
        let of = words[2]
            .trim_end_matches(['?', '.', '!'])
            .parse::<f64>()
            .ok()?;
        return finite(percent)
            .zip(finite(of))
            .map(|(percent, of)| MathCommand::Percentage { percent, of });
    }
    let words = if words.first() == Some(&"convert") {
        &words[1..]
    } else {
        &words[..]
    };
    if words.len() == 4 && words[2] == "to" {
        let value = finite(words[0].parse::<f64>().ok()?)?;
        return Some(MathCommand::Convert {
            value,
            from: Unit::parse(words[1])?,
            to: Unit::parse(words[3])?,
        });
    }
    None
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

/// Stable short display for a value card.  The calculation retains full f64
/// precision; presentation rounds to two decimals without scientific notation.
pub fn format_number(value: f64) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    if rounded.fract() == 0.0 {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentage_is_deterministic() {
        let answer = parse_math_command("15% of 230")
            .unwrap()
            .evaluate()
            .unwrap();
        assert_eq!(answer.expression, "15% of 230");
        assert_eq!(format_number(answer.value), "34.5");
        assert_eq!(answer.unit, None);
    }

    #[test]
    fn conversion_accepts_spoken_plural_units() {
        let answer = parse_math_command("convert 5 miles to km")
            .unwrap()
            .evaluate()
            .unwrap();
        assert_eq!(answer.unit, Some(Unit::Kilometre));
        assert_eq!(format_number(answer.value), "8.05");
    }

    #[test]
    fn ambiguous_or_unbounded_input_is_not_guessed() {
        assert!(parse_math_command("what is fifteen percent of two hundred and thirty").is_none());
        assert!(parse_math_command(&"1".repeat(MAX_UTTERANCE_BYTES + 1)).is_none());
        assert!(parse_math_command("NaN% of 1").is_none());
    }

    #[test]
    fn arithmetic_overflow_is_not_rendered_as_an_answer() {
        assert!(
            parse_math_command("1e308% of 1e308")
                .and_then(|command| command.evaluate())
                .is_none()
        );
        assert!(
            parse_math_command("convert 1e308 km to in")
                .and_then(|command| command.evaluate())
                .is_none()
        );
    }
}
