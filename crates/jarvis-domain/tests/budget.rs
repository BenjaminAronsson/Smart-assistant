//! F1.2 run-budget table (NFR-12, state-machine skill rule 4). The budget check
//! is pure: the domain never reads a clock, so elapsed time is passed in. The
//! orchestrator (F1.3) calls this at the top of the loop; "exceeded" is strictly
//! greater than the cap so usage exactly at the cap is still within budget.

use jarvis_domain::run::{BudgetDimension, RunBudget, RunUsage};
use std::time::Duration;

fn budget() -> RunBudget {
    RunBudget {
        max_model_turns: 4,
        max_tool_calls: 8,
        max_duration: Duration::from_secs(30),
        max_artifact_bytes: 1024,
    }
}

fn zero_usage() -> RunUsage {
    RunUsage {
        model_turns: 0,
        tool_calls: 0,
        elapsed: Duration::ZERO,
        artifact_bytes: 0,
    }
}

#[test]
fn within_budget_returns_none() {
    let b = budget();
    assert_eq!(b.exceeded(&zero_usage()), None);
    let u = RunUsage {
        model_turns: 2,
        tool_calls: 4,
        elapsed: Duration::from_secs(10),
        artifact_bytes: 512,
    };
    assert_eq!(b.exceeded(&u), None);
}

#[test]
fn usage_exactly_at_each_cap_is_within_budget() {
    // Boundary: == cap is NOT exceeded; only strictly greater trips.
    let b = budget();
    let u = RunUsage {
        model_turns: 4,
        tool_calls: 8,
        elapsed: Duration::from_secs(30),
        artifact_bytes: 1024,
    };
    assert_eq!(b.exceeded(&u), None);
}

#[test]
fn one_past_each_cap_reports_that_dimension() {
    let b = budget();
    assert_eq!(
        b.exceeded(&RunUsage {
            model_turns: 5,
            ..zero_usage()
        }),
        Some(BudgetDimension::ModelTurns)
    );
    assert_eq!(
        b.exceeded(&RunUsage {
            tool_calls: 9,
            ..zero_usage()
        }),
        Some(BudgetDimension::ToolCalls)
    );
    assert_eq!(
        b.exceeded(&RunUsage {
            elapsed: Duration::from_secs(31),
            ..zero_usage()
        }),
        Some(BudgetDimension::Duration)
    );
    assert_eq!(
        b.exceeded(&RunUsage {
            artifact_bytes: 1025,
            ..zero_usage()
        }),
        Some(BudgetDimension::ArtifactBytes)
    );
}

#[test]
fn precedence_is_turns_then_tools_then_duration_then_bytes() {
    let b = budget();
    // All four past their caps -> ModelTurns wins (checked first).
    assert_eq!(
        b.exceeded(&RunUsage {
            model_turns: 99,
            tool_calls: 99,
            elapsed: Duration::from_secs(999),
            artifact_bytes: 9999,
        }),
        Some(BudgetDimension::ModelTurns)
    );
    // Turns fine; tools/duration/bytes past -> ToolCalls wins.
    assert_eq!(
        b.exceeded(&RunUsage {
            model_turns: 0,
            tool_calls: 99,
            elapsed: Duration::from_secs(999),
            artifact_bytes: 9999,
        }),
        Some(BudgetDimension::ToolCalls)
    );
    // Only duration and bytes past -> Duration wins.
    assert_eq!(
        b.exceeded(&RunUsage {
            model_turns: 0,
            tool_calls: 0,
            elapsed: Duration::from_secs(999),
            artifact_bytes: 9999,
        }),
        Some(BudgetDimension::Duration)
    );
}

#[test]
fn default_interactive_is_a_sane_nonzero_budget() {
    let b = RunBudget::default_interactive();
    assert!(b.max_model_turns >= 1);
    assert!(b.max_tool_calls >= 1);
    assert!(b.max_duration > Duration::ZERO);
    assert!(b.max_artifact_bytes > 0);
}
