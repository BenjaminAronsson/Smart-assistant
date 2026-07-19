//! F0.3: error-code registry (docs/05 §7; skill `ws-contracts` step 7).
//!
//! `ErrorCode` is a closed, stable set of dotted machine codes for client logic;
//! `ProblemDetails` is the RFC 9457 problem body carrying one of those codes.
//!
//! As with the envelope's `type` field, `ProblemDetails`'s `type` member name
//! collides with the Rust keyword; these tests only assert the JSON wire shape, never
//! a struct-literal field name, so they do not lock in an implementation choice for
//! that identifier.

use jarvis_contracts::errors::{ErrorCode, ProblemDetails};
use serde_json::json;

/// The registry of docs/05 §7, in table order. Grows additively with the
/// features that introduce new failure modes (auth.pairing_invalid: F0.7).
const REGISTRY_CODES: &[&str] = &[
    "auth.invalid_token",
    "auth.scope_missing",
    "auth.pairing_invalid",
    "validation.failed",
    "idempotency.conflict",
    "resource.version_conflict",
    "resource.not_found",
    "run.budget_exceeded",
    "run.not_cancellable",
    "provider.unavailable",
    "provider.quota_exhausted",
    "policy.denied",
    "grant.expired",
    "grant.args_mismatch",
    "grant.consumed",
    "tool.timeout",
    "tool.result_invalid",
    "artifact.too_large",
    "degraded.queued",
];

#[test]
fn registry_fixture_matches_docs_05_table_size() {
    // Sanity check on the fixture itself against docs/05 §7's table.
    assert_eq!(REGISTRY_CODES.len(), 19);
}

#[test]
fn every_registry_code_round_trips_to_its_exact_wire_string() {
    // docs/05 §7: "stable machine codes for client logic".
    for code in REGISTRY_CODES {
        let wire = json!(code);
        let parsed: ErrorCode = serde_json::from_value(wire.clone())
            .unwrap_or_else(|e| panic!("ErrorCode must parse docs/05 §7 code '{code}': {e}"));
        let back = serde_json::to_value(parsed).unwrap();
        assert_eq!(
            back, wire,
            "ErrorCode for '{code}' did not round-trip to the exact wire string"
        );
    }
}

#[test]
fn unknown_error_code_is_rejected() {
    // ErrorCode must be a closed enum, not a permissive string wrapper — otherwise a
    // typo'd/unregistered code would silently pass client validation.
    let result: Result<ErrorCode, _> = serde_json::from_value(json!("made.up_code"));
    assert!(result.is_err());
}

#[test]
fn empty_error_code_is_rejected() {
    let result: Result<ErrorCode, _> = serde_json::from_value(json!(""));
    assert!(result.is_err());
}

#[test]
fn problem_details_round_trips_all_fields() {
    let value = json!({
        "type": "https://jarvis.dev/problems/auth-invalid-token",
        "title": "Invalid device token",
        "status": 401,
        "detail": "The device token has been revoked.",
        "instance": "/api/v1/sessions",
        "code": "auth.invalid_token"
    });
    let problem: ProblemDetails =
        serde_json::from_value(value.clone()).expect("full ProblemDetails must deserialize");
    let round_tripped = serde_json::to_value(&problem).unwrap();
    assert_eq!(round_tripped, value);
}

#[test]
fn problem_details_omits_optional_fields_when_absent() {
    let value = json!({
        "type": "https://jarvis.dev/problems/validation-failed",
        "title": "Validation failed",
        "status": 400,
        "code": "validation.failed"
    });
    let problem: ProblemDetails =
        serde_json::from_value(value.clone()).expect("minimal ProblemDetails must deserialize");
    let round_tripped = serde_json::to_value(&problem).unwrap();
    let obj = round_tripped.as_object().unwrap();
    assert!(
        !obj.contains_key("detail"),
        "detail must be omitted, not null, when absent"
    );
    assert!(
        !obj.contains_key("instance"),
        "instance must be omitted, not null, when absent"
    );
    assert_eq!(round_tripped, value);
}

#[test]
fn problem_details_status_serializes_as_a_number() {
    let value = json!({
        "type": "about:blank",
        "title": "Not found",
        "status": 404,
        "code": "resource.not_found"
    });
    let problem: ProblemDetails = serde_json::from_value(value).unwrap();
    let round_tripped = serde_json::to_value(&problem).unwrap();
    assert!(
        round_tripped["status"].is_u64(),
        "status must serialize as a number, not a string: {round_tripped}"
    );
    assert_eq!(round_tripped["status"], json!(404));
}

#[test]
fn problem_details_code_must_be_a_registered_error_code() {
    let value = json!({
        "type": "about:blank",
        "title": "Bogus",
        "status": 500,
        "code": "not.a.real.code"
    });
    let result: Result<ProblemDetails, _> = serde_json::from_value(value);
    assert!(
        result.is_err(),
        "ProblemDetails.code must reject a code outside the ErrorCode registry"
    );
}

#[test]
fn problem_details_status_rejects_out_of_range_u16() {
    // status is documented as u16; HTTP statuses fit in u16, but this guards the
    // representation choice against a value that overflows it.
    let value = json!({
        "type": "about:blank",
        "title": "overflow",
        "status": 70000,
        "code": "validation.failed"
    });
    let result: Result<ProblemDetails, _> = serde_json::from_value(value);
    assert!(
        result.is_err(),
        "status must reject values that don't fit in u16"
    );
}
