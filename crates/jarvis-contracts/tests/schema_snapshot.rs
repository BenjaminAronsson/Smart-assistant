//! F0.3: committed JSON Schema snapshot (docs/05 §5; skill `ws-contracts` step 6).
//!
//! `jarvis_contracts::schema::export()` must match the schema committed at
//! `crates/jarvis-contracts/schemas/jarvis-contracts.schema.json` byte-for-value; CI
//! diffs this to catch drift between the Rust types and the generated TypeScript
//! client (skill `ws-contracts` step 4). Adding a root DTO means editing BOTH
//! `schema::export()` and `REQUIRED_DEFINITIONS` below — a type on neither
//! list ships silently absent from the wire schema.

use std::path::Path;

const REQUIRED_DEFINITIONS: &[&str] = &[
    "EventEnvelope",
    "ProblemDetails",
    "HealthResponse",
    "PairRequest",
    "PairResponse",
    "SessionDto",
    "CreateSessionRequest",
    "SessionListResponse",
    "ContentBlock",
    "RunDto",
    "RunAck",
    "MessageDto",
    "SubmitMessageRequest",
    "TimelineResponse",
    "ProvidersResponse",
    "DomainEvent",
    "TransientEvent",
    "ApprovalCardDto",
    "ApprovalDecisionDto",
];

fn snapshot_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("schemas")
        .join("jarvis-contracts.schema.json")
}

#[test]
fn exported_schema_matches_committed_snapshot() {
    let path = snapshot_path();

    let committed_text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "committed schema snapshot not found at {}: {e}\n\
             Run `cargo xtask codegen` to generate it, then commit the result.",
            path.display()
        )
    });

    let committed_value: serde_json::Value =
        serde_json::from_str(&committed_text).unwrap_or_else(|e| {
            panic!(
                "committed schema snapshot at {} is not valid JSON: {e}",
                path.display()
            )
        });

    let exported = jarvis_contracts::schema::export();

    assert_eq!(
        exported,
        committed_value,
        "jarvis_contracts::schema::export() drifted from the committed snapshot at {}.\n\
         Run `cargo xtask codegen` and commit the regenerated schema.",
        path.display()
    );
}

#[test]
fn exported_schema_is_draft_07() {
    let exported = jarvis_contracts::schema::export();
    assert_eq!(
        exported.get("$schema").and_then(|v| v.as_str()),
        Some("http://json-schema.org/draft-07/schema#"),
        "jarvis-contracts schema must declare JSON Schema draft-07"
    );
}

#[test]
fn exported_schema_has_a_definitions_object() {
    let exported = jarvis_contracts::schema::export();
    assert!(
        exported
            .get("definitions")
            .and_then(|d| d.as_object())
            .is_some(),
        "schema document must have a top-level \"definitions\" object"
    );
}

#[test]
fn exported_schema_defines_every_required_dto() {
    let exported = jarvis_contracts::schema::export();
    let definitions = exported
        .get("definitions")
        .and_then(|d| d.as_object())
        .expect("schema document must have a \"definitions\" object");

    for name in REQUIRED_DEFINITIONS {
        assert!(
            definitions.contains_key(*name),
            "schema is missing definition for {name}; run `cargo xtask codegen`"
        );
    }
}

#[test]
fn committed_snapshot_defines_every_required_dto() {
    let path = snapshot_path();
    let committed_text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "committed schema snapshot not found at {}: {e}\n\
             Run `cargo xtask codegen` to generate it, then commit the result.",
            path.display()
        )
    });
    let committed_value: serde_json::Value =
        serde_json::from_str(&committed_text).expect("committed snapshot must be valid JSON");
    let definitions = committed_value
        .get("definitions")
        .and_then(|d| d.as_object())
        .expect("committed snapshot must have a \"definitions\" object");

    for name in REQUIRED_DEFINITIONS {
        assert!(
            definitions.contains_key(*name),
            "committed snapshot is missing definition for {name}"
        );
    }
}
