//! F1.1: message / timeline / provider DTO round-trips (docs/05 §1).

use jarvis_contracts::content::ContentBlock;
use jarvis_contracts::events::DomainEvent;
use jarvis_contracts::messages::{MessageDto, MessageRole, SubmitMessageRequest};
use jarvis_contracts::providers::{ProviderDto, ProviderState, ProvidersResponse, QuotaState};
use jarvis_contracts::timeline::{TimelineItem, TimelineResponse};
use serde_json::json;

const SESSION: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
const MSG: &str = "01BX5ZZKBKACTAV9WEVGEMMVS0";
const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn message() -> MessageDto {
    MessageDto {
        id: MSG.parse().unwrap(),
        session_id: SESSION.parse().unwrap(),
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: "what's the weather".into(),
        }],
        created_at: "2026-07-19T10:00:00Z".into(),
    }
}

#[test]
fn message_dto_round_trips() {
    let value = serde_json::to_value(message()).unwrap();
    assert_eq!(value["role"], "user");
    assert_eq!(
        value["content"][0],
        json!({ "type": "text", "text": "what's the weather" })
    );
    let back: MessageDto = serde_json::from_value(value).unwrap();
    assert_eq!(back, message());
}

#[test]
fn submit_message_request_round_trips() {
    let value = json!({ "content": [{ "type": "text", "text": "hi" }] });
    let req: SubmitMessageRequest = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(req.content.len(), 1);
    assert_eq!(serde_json::to_value(&req).unwrap(), value);
}

#[test]
fn timeline_mixes_messages_and_run_events_and_omits_cursor_at_head() {
    let response = TimelineResponse {
        items: vec![
            TimelineItem::Message { message: message() },
            TimelineItem::RunEvent {
                event: DomainEvent::RunStarted {
                    run_id: RUN.parse().unwrap(),
                    session_id: SESSION.parse().unwrap(),
                },
            },
        ],
        next_since: None,
    };
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["items"][0]["type"], "message");
    assert_eq!(value["items"][1]["type"], "run_event");
    assert!(!value.as_object().unwrap().contains_key("nextSince"));
    let back: TimelineResponse = serde_json::from_value(value).unwrap();
    assert_eq!(back, response);
}

#[test]
fn timeline_carries_a_cursor_when_more_remains() {
    let response = TimelineResponse {
        items: vec![],
        next_since: Some(4182),
    };
    assert_eq!(serde_json::to_value(&response).unwrap()["nextSince"], 4182);
}

#[test]
fn provider_dto_round_trips_and_omits_absent_quota() {
    let healthy = ProviderDto {
        id: "claude-cli".into(),
        state: ProviderState::Healthy,
        quota: None,
        reason: None,
    };
    let value = serde_json::to_value(&healthy).unwrap();
    assert_eq!(value, json!({ "id": "claude-cli", "state": "healthy" }));
    assert_eq!(
        serde_json::from_value::<ProviderDto>(value).unwrap(),
        healthy
    );
}

#[test]
fn provider_dto_carries_quota_reset_window() {
    let response = ProvidersResponse {
        providers: vec![ProviderDto {
            id: "claude-cli".into(),
            state: ProviderState::Unavailable,
            quota: Some(QuotaState {
                reset_at: Some("2026-07-19T11:00:00Z".into()),
            }),
            reason: Some("quota_exhausted".into()),
        }],
    };
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(
        value["providers"][0]["quota"]["resetAt"],
        "2026-07-19T11:00:00Z"
    );
    let back: ProvidersResponse = serde_json::from_value(value).unwrap();
    assert_eq!(back, response);
}
