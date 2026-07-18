//! JSON Schema export for `cargo xtask codegen` (docs/05 §5).
//!
//! Draft-07 with a `definitions` map so downstream TypeScript generation has a
//! single document to consume. Adding a root DTO to the crate means editing
//! BOTH the registration list below and `REQUIRED_DEFINITIONS` in
//! `tests/schema_snapshot.rs` — a type on neither list ships silently absent
//! from the wire schema; the snapshot test only keeps registered roots honest.

use schemars::{JsonSchema, generate::SchemaSettings};
use serde_json::{Value, json};

/// Schema stand-in for domain ULID newtypes (`#[schemars(with = …)]`): the
/// wire contract documents what the server actually enforces (docs/04 §2).
pub struct UlidString;

impl JsonSchema for UlidString {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "UlidString".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[0-9A-HJKMNP-TV-Z]{26}$",
            "description": "ULID: 26 chars of uppercase Crockford base32",
        })
    }
}

pub fn export() -> Value {
    let mut generator = SchemaSettings::draft07().into_generator();
    // Registering the roots pulls every referenced type into `definitions`.
    generator.subschema_for::<crate::envelope::EventEnvelope>();
    generator.subschema_for::<crate::errors::ProblemDetails>();
    generator.subschema_for::<crate::health::HealthResponse>();
    generator.subschema_for::<crate::auth::PairRequest>();
    generator.subschema_for::<crate::auth::PairResponse>();
    generator.subschema_for::<crate::sessions::SessionDto>();
    generator.subschema_for::<crate::sessions::CreateSessionRequest>();
    generator.subschema_for::<crate::sessions::SessionListResponse>();
    generator.subschema_for::<crate::content::ContentBlock>();

    let definitions: Value =
        serde_json::to_value(generator.definitions()).expect("schemas are valid JSON");
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "jarvis-contracts",
        "description": format!("Jarvis wire contract v{} (generated — do not edit)", crate::CONTRACT_VERSION),
        "definitions": definitions,
    })
}
