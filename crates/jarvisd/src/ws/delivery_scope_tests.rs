use super::hub::*;
use jarvis_contracts::CONTRACT_VERSION;
use jarvis_contracts::envelope::{Channel, EventEnvelope};
use jarvis_domain::identity::DeviceClass;

const THIS_DEVICE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER_DEVICE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB9";

fn envelope(channel: Channel, event_type: &str, payload: serde_json::Value) -> EventEnvelope {
    EventEnvelope {
        v: CONTRACT_VERSION,
        seq: 1,
        channel,
        event_type: event_type.to_owned(),
        occurred_at: "2026-08-12T09:00:00Z".to_owned(),
        trace_id: None,
        resource_version: None,
        payload,
    }
}

/// **CF-8, stated as a table.** Rows are the events that actually travel
/// this channel; columns are the classes that can hold a socket. Every
/// cell is a deliberate decision rather than whatever the code happens to
/// do — which is the whole reason this is a table and not a handful of
/// examples.
#[test]
fn each_class_receives_exactly_its_own_channel() {
    let session = envelope(
        Channel::Session,
        "approval.requested",
        serde_json::json!({ "approvalId": "a", "exactEffect": "send mail to landlord" }),
    );
    let display = envelope(
        Channel::Display,
        "display.directive",
        serde_json::json!({ "surface": "canvas" }),
    );
    let voice_mine = envelope(
        Channel::Voice,
        "voice.transcript",
        serde_json::json!({ "streamId": "mine", "text": "turn on the lamp" }),
    );
    let voice_theirs = envelope(
        Channel::Voice,
        "voice.transcript",
        serde_json::json!({ "streamId": "theirs", "text": "my bank password is" }),
    );
    let voice_global = envelope(
        Channel::Voice,
        "voice.error",
        serde_json::json!({ "code": "voice.stt_unavailable" }),
    );

    // (class, owns "mine"): session, display, own voice, other voice, global voice
    let table = [
        (DeviceClass::OwnerUi, true, [true, true, true, false, true]),
        (
            DeviceClass::DisplayNode,
            false,
            [false, true, false, false, false],
        ),
        (
            DeviceClass::VoiceNode,
            true,
            [false, false, true, false, false],
        ),
        (
            DeviceClass::RoomNode,
            true,
            [false, true, true, false, false],
        ),
        // A voice-capable node that owns no stream hears nothing at all.
        (
            DeviceClass::RoomNode,
            false,
            [false, true, false, false, false],
        ),
    ];

    for (class, owns_mine, expected) in table {
        let owned = owns_mine.then_some("mine");
        let actual = [
            delivers_to(&session, class, Some(THIS_DEVICE), owned),
            delivers_to(&display, class, Some(THIS_DEVICE), owned),
            delivers_to(&voice_mine, class, Some(THIS_DEVICE), owned),
            delivers_to(&voice_theirs, class, Some(THIS_DEVICE), owned),
            delivers_to(&voice_global, class, Some(THIS_DEVICE), owned),
        ];
        assert_eq!(
            actual, expected,
            "{class} (owns_mine={owns_mine}) delivery matrix"
        );
    }
}

/// Builds the owned-id list a socket has after starting one voice turn.
fn owning(ids: &[&str]) -> std::collections::VecDeque<OwnedId> {
    ids.iter().map(|s| OwnedId::Run((*s).to_owned())).collect()
}

/// The same ids, but declared the way a *client* declares a capture stream.
fn owning_streams(ids: &[&str]) -> std::collections::VecDeque<OwnedId> {
    ids.iter()
        .map(|s| OwnedId::Stream((*s).to_owned()))
        .collect()
}

/// F8.5's named acceptance: **two nodes, each gets only its own answers.**
///
/// The kitchen asks a question and the study asks a different one at the
/// same time. Each node must hear its own answer and must not hear the
/// other's — which is both the feature (an answer comes back to the room
/// that spoke) and the privacy property (a satellite is not a household
/// broadcast receiver).
#[test]
fn two_nodes_each_get_only_their_own_answers() {
    let kitchen_run = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let study_run = "01BX5ZZKBKACTAV9WEVGEMMVRZ";

    let kitchen_answer = envelope(
        Channel::Session,
        "text.delta",
        serde_json::json!({ "runId": kitchen_run, "text": "Twenty minutes." }),
    );
    let study_answer = envelope(
        Channel::Session,
        "text.delta",
        serde_json::json!({ "runId": study_run, "text": "It is raining." }),
    );

    for class in [DeviceClass::VoiceNode, DeviceClass::RoomNode] {
        let kitchen = owning(&[kitchen_run]);
        let study = owning(&[study_run]);

        assert!(
            delivers_to_owner_of(&kitchen_answer, class, THIS_DEVICE, &kitchen),
            "{class} must hear the answer to the run it started"
        );
        assert!(
            !delivers_to_owner_of(&study_answer, class, THIS_DEVICE, &kitchen),
            "{class} must not hear another room's answer"
        );
        assert!(
            delivers_to_owner_of(&study_answer, class, THIS_DEVICE, &study),
            "the other node must hear its own answer"
        );
    }
}

/// The terminal events matter as much as the deltas: without them the
/// clause queue never closes and the node holds an utterance open until the
/// socket dies, having spoken everything but never finishing.
#[test]
fn a_node_hears_its_own_runs_terminal_events() {
    let run = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let owned = owning(&[run]);

    for event_type in ["run.completed", "run.queued", "degraded.queued"] {
        let terminal = envelope(
            Channel::Session,
            event_type,
            serde_json::json!({ "runId": run }),
        );
        assert!(
            delivers_to_owner_of(&terminal, DeviceClass::RoomNode, THIS_DEVICE, &owned),
            "{event_type} closes the spoken answer and must reach the node"
        );
    }
}

/// Owning a run buys the answer to it and **nothing else on that channel**.
///
/// This is the test that keeps F8.5 from becoming a hole: the exemption is
/// keyed on an allowlist of event types as well as on ownership, so a
/// Session event *about the very run the node started* is still refused
/// unless it is part of the spoken answer.
#[test]
fn owning_a_run_does_not_hand_a_node_the_rest_of_that_run() {
    let run = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let owned = owning(&[run]);

    // An approval card names its run at the payload root here — exactly the
    // shape the nesting under `card` accidentally protects against today.
    let approval = envelope(
        Channel::Session,
        "approval.requested",
        serde_json::json!({
            "runId": run,
            "approvalId": "01BX5ZZKBKACTAV9WEVGEMMVRZ",
            "exactEffect": "email landlord@example.com",
        }),
    );
    // And a tool result for the same run, which can carry anything the tool
    // read.
    let tool_result = envelope(
        Channel::Session,
        "tool.completed",
        serde_json::json!({ "runId": run, "output": "the safe code is 1234" }),
    );

    for class in [DeviceClass::VoiceNode, DeviceClass::RoomNode] {
        assert!(
            !delivers_to_owner_of(&approval, class, THIS_DEVICE, &owned),
            "{class} must never receive an approval card, own run or not"
        );
        assert!(
            !delivers_to_owner_of(&tool_result, class, THIS_DEVICE, &owned),
            "{class} must not receive tool output for a run it started"
        );
    }
}

/// A node that started nothing hears nothing, even for a well-formed
/// answer — so the exemption cannot be reached by guessing a run id.
#[test]
fn a_node_that_started_no_run_hears_no_answer() {
    let answer = envelope(
        Channel::Session,
        "text.delta",
        serde_json::json!({ "runId": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "text": "…" }),
    );
    assert!(!delivers_to_owner_of(
        &answer,
        DeviceClass::RoomNode,
        THIS_DEVICE,
        &owning(&[])
    ));
}

/// S2 from the M8 security audit: a client-chosen `streamId` cannot
/// impersonate a daemon-minted run id.
///
/// Capture-stream ids come from the client; run ids are minted here. Held in
/// one untagged list, a socket could open a stream named after somebody
/// else's run and be treated as that run's owner — receiving the whole
/// spoken answer to a question it never asked, past the Session channel's
/// `ui` rule. Not reachable when this was written, because run ids are ULIDs
/// and nothing a node may receive discloses another run's id; tagged anyway,
/// because that is one leaked field away from being reachable.
#[test]
fn a_client_declared_stream_id_cannot_impersonate_a_run() {
    let run = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let answer = envelope(
        Channel::Session,
        "text.delta",
        serde_json::json!({ "runId": run, "text": "Twenty minutes." }),
    );

    for class in [DeviceClass::VoiceNode, DeviceClass::RoomNode] {
        assert!(
            !delivers_to_owner_of(&answer, class, THIS_DEVICE, &owning_streams(&[run])),
            "{class} declared a capture stream named after a run; that must buy nothing"
        );
        // And the legitimate route still works.
        assert!(delivers_to_owner_of(
            &answer,
            class,
            THIS_DEVICE,
            &owning(&[run])
        ));
    }
}

/// The converse, so the tagging is not satisfied by a rule that simply
/// stopped matching: a real capture stream still reaches its owner.
#[test]
fn a_real_capture_stream_still_reaches_its_owner() {
    let transcript = envelope(
        Channel::Voice,
        "voice.transcript",
        serde_json::json!({ "streamId": "mine", "text": "turn on the lamp" }),
    );
    assert!(delivers_to_owner_of(
        &transcript,
        DeviceClass::RoomNode,
        THIS_DEVICE,
        &owning_streams(&["mine"])
    ));
    // ...and a run id of the same name does not stand in for it.
    assert!(!delivers_to_owner_of(
        &transcript,
        DeviceClass::RoomNode,
        THIS_DEVICE,
        &owning(&["mine"])
    ));
}

/// The browser is unaffected: it holds `ui` and still sees the session
/// whether or not it owns the run.
#[test]
fn the_owner_ui_still_sees_runs_it_did_not_start() {
    let answer = envelope(
        Channel::Session,
        "text.delta",
        serde_json::json!({ "runId": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "text": "…" }),
    );
    assert!(delivers_to_owner_of(
        &answer,
        DeviceClass::OwnerUi,
        THIS_DEVICE,
        &owning(&[])
    ));
}

/// The single most important cell, called out on its own so a future edit
/// to the table cannot quietly relax it: an approval card carries the exact
/// effect, the real arguments, and an id that is a decision oracle.
#[test]
fn no_node_class_ever_receives_an_approval_card() {
    let card = envelope(
        Channel::Session,
        "approval.requested",
        serde_json::json!({
            "approvalId": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "exactEffect": "email landlord@example.com",
            "proposedArguments": { "to": "landlord@example.com", "body": "…" }
        }),
    );
    for class in [
        DeviceClass::DisplayNode,
        DeviceClass::VoiceNode,
        DeviceClass::RoomNode,
    ] {
        assert!(
            !delivers_to(&card, class, Some(THIS_DEVICE), Some("mine")),
            "{class} must never be handed an approval card"
        );
    }
    assert!(delivers_to(
        &card,
        DeviceClass::OwnerUi,
        Some(THIS_DEVICE),
        None
    ));
}

/// A timer rings in exactly the room it was set in (F8.5). Without this,
/// a kitchen timer either rings in every room at once or — as it did before
/// M8 — rings only on the daemon's own host, at the desk.
#[test]
fn a_timer_alert_reaches_only_the_room_it_was_set_in() {
    let addressed = envelope(
        Channel::Voice,
        "timer.fired",
        serde_json::json!({ "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "targetDeviceId": THIS_DEVICE }),
    );
    // Both satellite classes can ring: a voice node is a speaker with no
    // screen, which is exactly the device a kitchen timer needs.
    for class in [DeviceClass::VoiceNode, DeviceClass::RoomNode] {
        assert!(
            delivers_to(&addressed, class, Some(THIS_DEVICE), None),
            "{class} did not receive the alert addressed to it"
        );
        assert!(
            !delivers_to(&addressed, class, Some(OTHER_DEVICE), None),
            "{class} received an alert addressed to another room"
        );
    }
}

/// The device address is checked *before* the stream rule, because a timer
/// belongs to a room rather than to a conversation: nothing has a capture
/// stream open when a timer goes off in an empty kitchen.
#[test]
fn a_timer_alert_needs_no_open_capture_stream() {
    let addressed = envelope(
        Channel::Voice,
        "timer.fired",
        serde_json::json!({ "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "targetDeviceId": THIS_DEVICE }),
    );
    // `None` for the owned stream: this socket is idle, as a kitchen node
    // is for all but a few seconds a day.
    assert!(delivers_to(
        &addressed,
        DeviceClass::VoiceNode,
        Some(THIS_DEVICE),
        None
    ));
}

/// An addressed placement reaches exactly one screen (F7.5). Without this,
/// "put it on the kitchen screen" would light up every screen in the house.
#[test]
fn an_addressed_placement_reaches_only_its_target() {
    let addressed = envelope(
        Channel::Display,
        "display.place_surface",
        serde_json::json!({ "monitor": "DP-1", "targetDeviceId": THIS_DEVICE }),
    );
    let unaddressed = envelope(
        Channel::Display,
        "display.place_surface",
        serde_json::json!({ "monitor": "DP-1" }),
    );
    for class in [
        DeviceClass::DisplayNode,
        DeviceClass::RoomNode,
        DeviceClass::OwnerUi,
    ] {
        assert!(delivers_to(&addressed, class, Some(THIS_DEVICE), None));
        assert!(
            !delivers_to(&addressed, class, Some(OTHER_DEVICE), None),
            "{class} received a placement addressed elsewhere"
        );
        // Unaddressed keeps the pre-node behaviour: every presenter.
        assert!(delivers_to(&unaddressed, class, Some(OTHER_DEVICE), None));
    }
    // And a class with no screen is still out, addressed or not.
    assert!(!delivers_to(
        &addressed,
        DeviceClass::VoiceNode,
        Some(THIS_DEVICE),
        None
    ));
}

/// A satellite's microphone must not become a household-wide listening
/// device — the M5 carry-forward.
#[test]
fn one_satellites_transcript_never_reaches_another() {
    let kitchen = envelope(
        Channel::Voice,
        "voice.transcript",
        serde_json::json!({ "streamId": "kitchen", "text": "read me the message" }),
    );
    assert!(delivers_to(
        &kitchen,
        DeviceClass::RoomNode,
        Some(THIS_DEVICE),
        Some("kitchen")
    ));
    assert!(!delivers_to(
        &kitchen,
        DeviceClass::RoomNode,
        Some(THIS_DEVICE),
        Some("bedroom")
    ));
    assert!(!delivers_to(
        &kitchen,
        DeviceClass::VoiceNode,
        Some(THIS_DEVICE),
        None
    ));
}
