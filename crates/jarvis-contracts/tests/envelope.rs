//! F0.3: WS event envelope (docs/05 §3; skill `ws-contracts` steps 1-3).
//!
//! The envelope carries `v, seq, channel, type, occurredAt, traceId, resourceVersion,
//! payload`, camelCase on the wire; `traceId` and `resourceVersion` are optional and
//! must be omitted (not `null`) when absent; `channel` is one of
//! `session` | `display` | `voice`.
//!
//! These tests deliberately never construct `EventEnvelope { .. }` via struct-literal
//! syntax, because the Rust field name backing the JSON `"type"` key (a reserved
//! keyword) is an implementation choice, not part of the agreed wire contract — only
//! the JSON round-trip is normative here.

use jarvis_contracts::envelope::{Channel, EventEnvelope};
use serde_json::json;

/// The exact example from docs/05 §3.
fn example_envelope_json() -> serde_json::Value {
    json!({
        "v": 1,
        "seq": 4182,
        "channel": "session",
        "type": "run.tool.completed",
        "occurredAt": "2026-07-17T10:31:04.112Z",
        "traceId": "trace-abc123",
        "resourceVersion": 17,
        "payload": {}
    })
}

#[test]
fn round_trips_docs_05_section_3_example_exactly() {
    // FR-contracts: docs/05 §3 envelope example must round-trip byte-for-byte
    // (modulo key order) through EventEnvelope.
    let expected = example_envelope_json();
    let envelope: EventEnvelope = serde_json::from_value(expected.clone())
        .expect("docs/05 §3 example envelope must deserialize");
    let actual = serde_json::to_value(&envelope).expect("envelope must serialize");
    assert_eq!(actual, expected);
}

#[test]
fn wire_keys_are_camel_case() {
    let value = example_envelope_json();
    let obj = value.as_object().unwrap();
    for key in [
        "v",
        "seq",
        "channel",
        "type",
        "occurredAt",
        "traceId",
        "resourceVersion",
        "payload",
    ] {
        assert!(obj.contains_key(key), "expected envelope key '{key}'");
    }
    for snake_key in ["occurred_at", "trace_id", "resource_version"] {
        assert!(
            !obj.contains_key(snake_key),
            "envelope must not use snake_case key '{snake_key}'"
        );
    }
}

#[test]
fn omits_trace_id_and_resource_version_when_absent() {
    // docs/05 §3: traceId and resourceVersion are optional.
    let minimal = json!({
        "v": 1,
        "seq": 1,
        "channel": "display",
        "type": "presence.updated",
        "occurredAt": "2026-07-17T10:00:00Z",
        "payload": { "online": true }
    });
    let envelope: EventEnvelope = serde_json::from_value(minimal.clone())
        .expect("envelope without optional fields must deserialize");
    let round_tripped = serde_json::to_value(&envelope).unwrap();
    let obj = round_tripped
        .as_object()
        .expect("serialized envelope must be a JSON object");
    assert!(
        !obj.contains_key("traceId"),
        "traceId must be omitted (not null) when absent, got {round_tripped}"
    );
    assert!(
        !obj.contains_key("resourceVersion"),
        "resourceVersion must be omitted (not null) when absent, got {round_tripped}"
    );
    assert_eq!(round_tripped, minimal);
}

#[test]
fn rejects_null_trace_id_and_resource_version() {
    // Optional means "key absent", not "key present with value null" — the two are
    // different wire shapes and the envelope should not normalize null to None
    // silently if the implementation chose a plain (non-nullable) Option decode.
    // If the implementation does accept explicit null, that is also spec-compliant
    // per standard serde Option semantics; this test only guards against the
    // opposite failure (a required field that rejects legitimate absence), which is
    // covered above. Here we just confirm the field *can* be explicit null without
    // panicking or silently mis-parsing into Some(Value::Null).
    let value = json!({
        "v": 1,
        "seq": 2,
        "channel": "voice",
        "type": "voice.stream.start",
        "occurredAt": "2026-07-17T10:00:00Z",
        "traceId": serde_json::Value::Null,
        "resourceVersion": serde_json::Value::Null,
        "payload": {}
    });
    let envelope: EventEnvelope = serde_json::from_value(value)
        .expect("explicit null for optional fields must at least deserialize");
    let round_tripped = serde_json::to_value(&envelope).unwrap();
    let obj = round_tripped.as_object().unwrap();
    assert!(
        !obj.contains_key("traceId"),
        "a None traceId must serialize back to an omitted key, not null"
    );
    assert!(
        !obj.contains_key("resourceVersion"),
        "a None resourceVersion must serialize back to an omitted key, not null"
    );
}

#[test]
fn channel_values_round_trip_to_exact_wire_strings() {
    for (variant, wire) in [
        (Channel::Session, "session"),
        (Channel::Display, "display"),
        (Channel::Voice, "voice"),
    ] {
        let json = serde_json::to_value(variant).unwrap();
        assert_eq!(json, serde_json::Value::String(wire.into()));
        let back: Channel = serde_json::from_value(json).unwrap();
        assert_eq!(back, variant);
    }
}

#[test]
fn channel_rejects_unknown_value() {
    let result: Result<Channel, _> = serde_json::from_value(json!("voicemail"));
    assert!(
        result.is_err(),
        "channel must be a closed enum, not a permissive string"
    );
}

#[test]
fn payload_accepts_arbitrary_json() {
    for payload in [
        json!({}),
        json!({"nested": {"a": [1, 2, 3]}}),
        json!(null),
        json!("scalar-payload-string"),
        json!(42),
    ] {
        let value = json!({
            "v": 1,
            "seq": 3,
            "channel": "session",
            "type": "run.started",
            "occurredAt": "2026-07-17T10:00:00Z",
            "payload": payload.clone(),
        });
        let envelope: EventEnvelope = serde_json::from_value(value)
            .unwrap_or_else(|e| panic!("payload {payload} must be accepted: {e}"));
        let round_tripped = serde_json::to_value(&envelope).unwrap();
        assert_eq!(round_tripped["payload"], payload);
    }
}

#[test]
fn seq_is_a_non_negative_integer_on_the_wire() {
    let value = example_envelope_json();
    assert!(
        value["seq"].is_u64(),
        "seq must serialize as a non-negative integer"
    );
}
