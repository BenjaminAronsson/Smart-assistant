//! Deterministic M4 evaluation fixtures.

use crate::calendar::classify_calendar_query;
use jarvis_domain::math::{format_number, parse_math_command};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationCase {
    pub name: &'static str,
    pub passed: bool,
}

pub fn m4_cases() -> [EvaluationCase; 3] {
    [
        EvaluationCase {
            name: "math_without_model",
            passed: parse_math_command("15% of 230")
                .and_then(|command| command.evaluate())
                .is_some_and(|result| format_number(result.value) == "34.5"),
        },
        EvaluationCase {
            name: "calendar_classifier_without_model",
            passed: classify_calendar_query("what's on today?").is_some(),
        },
        EvaluationCase {
            name: "bounded_case_set",
            passed: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_m4_deterministic_cases_pass() {
        assert!(m4_cases().iter().all(|case| case.passed));
    }
}
