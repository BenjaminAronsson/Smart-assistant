//! F1.1: the WS event union's persisted/transient classification (docs/05 §3).
//! This split is the resync contract (NFR-13): DomainEvents replay, transient
//! deltas never do — and every DomainEvent must be representable in the
//! timeline snapshot.

use jarvis_contracts::approvals::{
    ApprovalCardDto, ApprovalResolutionDto, DataEgressDto, RiskLevelDto,
};
use jarvis_contracts::events::{DomainEvent, TransientEvent};
use jarvis_contracts::media::MediaStateDto;
use jarvis_contracts::messages::{MessageDto, MessageRole};
use jarvis_contracts::providers::{ProviderDto, ProviderState};
use jarvis_contracts::runs::{RunOutcome, RunOutcomeKind, RunStateDto};
use jarvis_contracts::timeline::TimelineItem;
use serde_json::json;

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const SESSION: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
const MSG: &str = "01BX5ZZKBKACTAV9WEVGEMMVS0";
const APPROVAL: &str = "01BX5ZZKBKACTAV9WEVGEMMVS1";

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
        DomainEvent::ApprovalRequested {
            card: ApprovalCardDto {
                approval_id: APPROVAL.parse().unwrap(),
                run_id: RUN.parse().unwrap(),
                tool_id: "message.send".into(),
                exact_effect: "message.send {to=\"bob@example.com\"}".into(),
                proposed_arguments: json!({ "to": "bob@example.com" }),
                risk: RiskLevelDto::R2,
                reversible: false,
                egress: DataEgressDto::External,
            },
        },
        DomainEvent::ApprovalResolved {
            approval_id: APPROVAL.parse().unwrap(),
            run_id: RUN.parse().unwrap(),
            outcome: ApprovalResolutionDto::Approved,
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

fn every_transient_event() -> Vec<TransientEvent> {
    vec![
        TransientEvent::TextDelta {
            run_id: RUN.parse().unwrap(),
            text: "hel".into(),
        },
        // F3a.7: media state is a current-value readout with no run scope —
        // deliberately transient (docs/02 §11a, docs/05 §3).
        TransientEvent::MediaState {
            state: MediaStateDto {
                players: vec![],
                active_player: None,
                max_volume_pct: 70,
            },
        },
        TransientEvent::VoiceTranscript {
            stream_id: "stream-1".into(),
            text: "hello Jarvis".into(),
            is_final: false,
        },
        // S3/ADR-033 §4: how a run's answer may be spoken. Transient for the
        // same reason as `text.delta` — it describes an utterance in flight.
        TransientEvent::SpeechSensitive {
            run_id: RUN.parse().unwrap(),
        },
    ]
}

#[test]
fn transient_events_round_trip_and_carry_their_type_tag() {
    for event in every_transient_event() {
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], event.event_type());
        let back: TransientEvent = serde_json::from_value(value).unwrap();
        assert_eq!(back, event);
    }

    let delta = TransientEvent::TextDelta {
        run_id: RUN.parse().unwrap(),
        text: "hel".into(),
    };
    assert_eq!(
        serde_json::to_value(&delta).unwrap(),
        json!({ "type": "text.delta", "runId": RUN, "text": "hel" })
    );
    // S3: `runId` must sit at the payload's **top level**, because that is
    // where the hub's delivery rule looks for it after `split_tagged` strips
    // the tag (`delivers_to_owner_of`). If it nested or renamed, the node that
    // started the run would never be told the answer is sensitive — and would
    // speak it in the third-party voice. Fails open, so it is pinned here.
    assert_eq!(
        serde_json::to_value(TransientEvent::SpeechSensitive {
            run_id: RUN.parse().unwrap(),
        })
        .unwrap(),
        json!({ "type": "run.speech_sensitive", "runId": RUN })
    );
    assert_eq!(
        serde_json::to_value(TransientEvent::MediaState {
            state: MediaStateDto::default()
        })
        .unwrap(),
        json!({
            "type": "media.state",
            "state": { "players": [], "maxVolumePct": 0 }
        })
    );
    assert_eq!(
        serde_json::to_value(TransientEvent::VoiceTranscript {
            stream_id: "stream-1".into(),
            text: "hello Jarvis".into(),
            is_final: true,
        })
        .unwrap(),
        json!({
            "type": "voice.transcript",
            "streamId": "stream-1",
            "text": "hello Jarvis",
            "final": true,
        })
    );
}

#[test]
fn persisted_and_transient_type_tags_are_disjoint() {
    // docs/05 §3: an event is either replayable domain state or a disposable
    // delta — never ambiguously both.
    let domain: Vec<&str> = every_domain_event()
        .iter()
        .map(|e| e.event_type())
        .collect();
    let transient: Vec<&str> = every_transient_event()
        .iter()
        .map(|e| e.event_type())
        .collect();
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
            "approval.requested",
            "approval.resolved",
            "message.created",
            "provider.health_changed",
            "run.checkpoint_saved",
            "run.completed",
            "run.queued",
            "run.started",
            "run.state_changed",
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
