//! F1.1: run DTO round-trips (docs/05 §1/§4).

use jarvis_contracts::runs::{
    RunAck, RunBudgetDto, RunDto, RunOutcome, RunOutcomeKind, RunStateDto,
};
use serde_json::json;

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const SESSION: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";

fn budget() -> RunBudgetDto {
    RunBudgetDto {
        max_model_turns: 6,
        max_tool_calls: 12,
        max_duration_secs: 600,
        max_artifact_bytes: 52_428_800,
    }
}

#[test]
fn all_run_states_round_trip_to_snake_case() {
    let cases = [
        (RunStateDto::Received, "received"),
        (RunStateDto::ContextReady, "context_ready"),
        (RunStateDto::ModelRunning, "model_running"),
        (RunStateDto::PolicyReview, "policy_review"),
        (RunStateDto::WaitingApproval, "waiting_approval"),
        (RunStateDto::ToolRunning, "tool_running"),
        (RunStateDto::Replanning, "replanning"),
        (RunStateDto::Responding, "responding"),
        (RunStateDto::Completed, "completed"),
        (RunStateDto::Failed, "failed"),
        (RunStateDto::Cancelled, "cancelled"),
    ];
    for (state, wire) in cases {
        assert_eq!(serde_json::to_value(state).unwrap(), json!(wire));
        let back: RunStateDto = serde_json::from_value(json!(wire)).unwrap();
        assert_eq!(back, state);
    }
}

#[test]
fn terminal_states_are_terminal() {
    assert!(RunStateDto::Completed.is_terminal());
    assert!(RunStateDto::Failed.is_terminal());
    assert!(RunStateDto::Cancelled.is_terminal());
    assert!(!RunStateDto::Received.is_terminal());
    assert!(!RunStateDto::Responding.is_terminal());
}

#[test]
fn run_dto_omits_outcome_until_terminal() {
    let value = json!({
        "id": RUN,
        "sessionId": SESSION,
        "state": "model_running",
        "budget": {
            "maxModelTurns": 6, "maxToolCalls": 12,
            "maxDurationSecs": 600, "maxArtifactBytes": 52428800
        },
        "createdAt": "2026-07-19T10:00:00Z",
        "updatedAt": "2026-07-19T10:00:01Z"
    });
    let dto: RunDto = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(dto.outcome, None);
    assert_eq!(serde_json::to_value(&dto).unwrap(), value);
}

#[test]
fn run_dto_round_trips_with_outcome() {
    let dto = RunDto {
        id: RUN.parse().unwrap(),
        session_id: SESSION.parse().unwrap(),
        state: RunStateDto::Failed,
        budget: budget(),
        outcome: Some(RunOutcome {
            kind: RunOutcomeKind::Failed,
            detail: Some("budget exceeded".into()),
        }),
        created_at: "2026-07-19T10:00:00Z".into(),
        updated_at: "2026-07-19T10:00:05Z".into(),
    };
    let back: RunDto = serde_json::from_value(serde_json::to_value(&dto).unwrap()).unwrap();
    assert_eq!(back, dto);
}

#[test]
fn run_ack_round_trips() {
    let ack = RunAck {
        run_id: RUN.parse().unwrap(),
        session_id: SESSION.parse().unwrap(),
        state: RunStateDto::Received,
    };
    let value = serde_json::to_value(&ack).unwrap();
    assert_eq!(
        value,
        json!({ "runId": RUN, "sessionId": SESSION, "state": "received" })
    );
    assert_eq!(serde_json::from_value::<RunAck>(value).unwrap(), ack);
}

#[test]
fn run_id_and_session_id_must_be_valid_ulids() {
    let bad = json!({
        "id": "not-a-ulid", "sessionId": SESSION, "state": "received",
        "budget": {"maxModelTurns":6,"maxToolCalls":12,"maxDurationSecs":600,"maxArtifactBytes":1},
        "createdAt": "2026-07-19T10:00:00Z", "updatedAt": "2026-07-19T10:00:00Z"
    });
    assert!(serde_json::from_value::<RunDto>(bad).is_err());
}
