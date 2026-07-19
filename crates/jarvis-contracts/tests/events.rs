//! F1.1: the WS event union's persisted/transient classification (docs/05 §3).
//! This split is the resync contract (NFR-13): DomainEvents replay, transient
//! deltas never do — and every DomainEvent must be representable in the
//! timeline snapshot.

use jarvis_contracts::events::{DomainEvent, TransientEvent};
use jarvis_contracts::messages::{MessageDto, MessageRole};
use jarvis_contracts::providers::{ProviderDto, ProviderState};
use jarvis_contracts::runs::{RunOutcome, RunOutcomeKind, RunStateDto};
use jarvis_contracts::timeline::TimelineItem;
use serde_json::json;

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const SESSION: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
const MSG: &str = "01BX5ZZKBKACTAV9WEVGEMMVS0";

fn every_domain_event() -> Vec<DomainEvent> {
    vec![
        DomainEvent::RunStarted {
            run_id: RUN.parse().unwrap(),
            session_id: SESSION.parse().unwrap(),
        },
        DomainEvent::RunStateChanged {
            run_id: RUN.parse().unwrap(),
            state: RunStateDto::ModelRunning,
        },
        DomainEvent::RunQueued {
            run_id: RUN.parse().unwrap(),
            reason: "provider quota exhausted".into(),
        },
        DomainEvent::RunCompleted {
            run_id: RUN.parse().unwrap(),
            outcome: RunOutcome {
                kind: RunOutcomeKind::Completed,
                detail: None,
            },
        },
        DomainEvent::MessageCreated {
            message: MessageDto {
                id: MSG.parse().unwrap(),
                session_id: SESSION.parse().unwrap(),
                role: MessageRole::Assistant,
                content: vec![],
                created_at: "2026-07-19T10:00:00Z".into(),
            },
        },
        DomainEvent::ProviderHealthChanged {
            provider: ProviderDto {
                id: "claude-cli".into(),
                state: ProviderState::Unavailable,
                quota: None,
                reason: Some("unreachable".into()),
            },
        },
        DomainEvent::CheckpointSaved {
            run_id: RUN.parse().unwrap(),
            state: RunStateDto::Responding,
        },
    ]
}

#[test]
fn domain_events_round_trip_and_carry_their_type_tag() {
    for event in every_domain_event() {
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            value["type"],
            event.event_type(),
            "envelope type tag must match the serialized tag"
        );
        let back: DomainEvent = serde_json::from_value(value).unwrap();
        assert_eq!(back, event);
    }
}

#[test]
fn transient_events_round_trip() {
    let delta = TransientEvent::TextDelta {
        run_id: RUN.parse().unwrap(),
        text: "hel".into(),
    };
    let value = serde_json::to_value(&delta).unwrap();
    assert_eq!(
        value,
        json!({ "type": "text_delta", "runId": RUN, "text": "hel" })
    );
    assert_eq!(delta.event_type(), "text_delta");
    let back: TransientEvent = serde_json::from_value(value).unwrap();
    assert_eq!(back, delta);
}

#[test]
fn persisted_and_transient_type_tags_are_disjoint() {
    // docs/05 §3: an event is either replayable domain state or a disposable
    // delta — never ambiguously both.
    let domain: Vec<&str> = every_domain_event()
        .iter()
        .map(|e| e.event_type())
        .collect();
    let transient = [TransientEvent::TextDelta {
        run_id: RUN.parse().unwrap(),
        text: String::new(),
    }
    .event_type()];
    for t in transient {
        assert!(
            !domain.contains(&t),
            "type tag {t:?} appears in both DomainEvent and TransientEvent"
        );
    }
    // The persisted set is exactly docs/05 §3's list — guard against a variant
    // being added without a deliberate classification decision.
    let mut sorted = domain.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        [
            "checkpoint_saved",
            "message_created",
            "provider_health_changed",
            "run_completed",
            "run_queued",
            "run_started",
            "run_state_changed",
        ]
    );
}

#[test]
fn every_domain_event_is_representable_in_the_timeline() {
    // The resync guarantee: anything replayable on the socket can be recovered
    // from the timeline snapshot (docs/05 §3). Transient events, by type, cannot
    // be placed in a TimelineItem at all — enforced at compile time.
    for event in every_domain_event() {
        let item = TimelineItem::RunEvent {
            event: event.clone(),
        };
        let value = serde_json::to_value(&item).unwrap();
        assert_eq!(value["type"], "run_event");
        let back: TimelineItem = serde_json::from_value(value).unwrap();
        assert_eq!(back, item);
    }
}
