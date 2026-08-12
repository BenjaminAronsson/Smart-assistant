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
    // Device management surface (F7.1, FR-19). Referenced by no event — each
    // root must be registered here or it ships absent from the wire schema.
    generator.subschema_for::<crate::devices::DeviceDto>();
    generator.subschema_for::<crate::devices::DeviceListResponse>();
    generator.subschema_for::<crate::devices::RevokeDeviceRequest>();
    generator.subschema_for::<crate::sessions::SessionDto>();
    generator.subschema_for::<crate::sessions::CreateSessionRequest>();
    generator.subschema_for::<crate::sessions::SessionListResponse>();
    generator.subschema_for::<crate::content::ContentBlock>();
    // M1 run/message/timeline/provider surface + typed WS events (F1.1).
    generator.subschema_for::<crate::runs::RunDto>();
    generator.subschema_for::<crate::runs::RunAck>();
    generator.subschema_for::<crate::messages::MessageDto>();
    generator.subschema_for::<crate::messages::SubmitMessageRequest>();
    generator.subschema_for::<crate::timeline::TimelineResponse>();
    generator.subschema_for::<crate::providers::ProvidersResponse>();
    generator.subschema_for::<crate::events::DomainEvent>();
    generator.subschema_for::<crate::events::TransientEvent>();
    // Approval surface (F2.5). The card rides in `DomainEvent::ApprovalRequested`,
    // but the decision body is a REST request DTO referenced by no event — it must
    // be registered as its own root or it ships absent from the wire schema.
    generator.subschema_for::<crate::approvals::ApprovalCardDto>();
    generator.subschema_for::<crate::approvals::ApprovalDecisionDto>();
    // Artifact read surface (F3a.3, FR-08). The manifest rides inside the
    // versions response, but register both so the manifest is a named root the
    // web shell can import for a single-version render (F3b.3).
    generator.subschema_for::<crate::artifacts::ArtifactManifestDto>();
    generator.subschema_for::<crate::artifacts::ArtifactVersionsResponse>();
    // Generated-app spec surface (F6.1, FR-18, ADR-029). `CapabilityDto` comes
    // along by reference from the manifest, but the inbound spec DTOs are
    // referenced by no event or response — each must be its own root or it
    // ships absent from the wire schema.
    generator.subschema_for::<crate::appbridge::CapabilityResultDto>();
    generator.subschema_for::<crate::appbridge::InvokeCapabilityRequest>();
    generator.subschema_for::<crate::appbridge::MintCapabilityTokenRequest>();
    generator.subschema_for::<crate::appbridge::CapabilityTokenDto>();
    generator.subschema_for::<crate::appspec::AppSpecDto>();
    generator.subschema_for::<crate::appspec::CapabilityDto>();
    // Display surface (F3a.4, FR-09/10). The directive is the display-channel
    // command to the agent; the open request/response is the REST entry point
    // that places an artifact's canvas on a selected monitor.
    generator.subschema_for::<crate::display::DisplayDirective>();
    generator.subschema_for::<crate::display::OpenArtifactRequest>();
    generator.subschema_for::<crate::display::OpenArtifactResponse>();
    // Media surface (F3a.7, FR-22). `MediaStateDto` rides inside the transient
    // `media.state` event, but the bar also reads it once over REST on connect
    // (transient events are never replayed), and the command request/response
    // are referenced by no event — each must be its own root or it ships absent
    // from the wire schema.
    generator.subschema_for::<crate::media::MediaStateDto>();
    generator.subschema_for::<crate::media::MediaStateResponse>();
    generator.subschema_for::<crate::media::MediaCommandRequest>();
    generator.subschema_for::<crate::media::MediaCommandResponse>();
    // HUD card grammar v1 (F3b.2, docs/12 §2.3). Registered as its own root
    // even though F3b.6's `hud.canvas` event now carries it, so the union stays
    // a named type the shell imports directly for a single-card render.
    generator.subschema_for::<crate::cards::HudCardDto>();
    generator.subschema_for::<crate::cards::AgendaEventDto>();
    // Deep-dive surface (F3b.6, FR-27, ADR-017). The canvas instruction rides
    // inside the transient `hud.canvas` event, but the findings request and the
    // promotion response are REST-only and referenced by no event — each must
    // be its own root or it ships absent from the wire schema.
    generator.subschema_for::<crate::deepdive::HudCanvasDto>();
    generator.subschema_for::<crate::deepdive::DeepDiveFindingsRequest>();
    generator.subschema_for::<crate::deepdive::DeepDiveFindingsResponse>();
    generator.subschema_for::<crate::deepdive::PromoteNotesResponse>();
    // Map surface (F3b.5, FR-25, ADR-013). Coverage is a REST-only read the map
    // card makes before it draws anything — it rides in no event, so it must be
    // its own root. `MapBoundsDto` comes along by reference and is a named type
    // the card uses on its own (in-region vs out-of-region, docs/12 §3).
    generator.subschema_for::<crate::maps::MapCoverageResponse>();
    // Timer surface (F3b.7, FR-33, ADR-023). `TimerDto` rides inside the
    // persisted `timer.fired` event, but the HUD also lists timers over REST on
    // connect and the create/action request+response are referenced by no event
    // — each must be its own root or it ships absent from the wire schema.
    generator.subschema_for::<crate::timers::TimerDto>();
    generator.subschema_for::<crate::timers::TimerListResponse>();
    generator.subschema_for::<crate::timers::CreateTimerRequest>();
    generator.subschema_for::<crate::timers::TimerActionRequest>();
    generator.subschema_for::<crate::timers::TimerActionResponse>();
    // List surface (F3b.8, FR-34, ADR-024). `ListDto` rides inside
    // `HudCardDto::List`, but the shell also reads the index over REST and the
    // create/add/check/command/promote request+response types are referenced by
    // no event — each must be its own root or it ships absent from the wire
    // schema, same reasoning as the timer surface above.
    generator.subschema_for::<crate::lists::ListDto>();
    generator.subschema_for::<crate::lists::ListIndexResponse>();
    generator.subschema_for::<crate::lists::CreateListRequest>();
    generator.subschema_for::<crate::lists::AddListItemRequest>();
    generator.subschema_for::<crate::lists::CheckListItemRequest>();
    generator.subschema_for::<crate::lists::ListCommandRequest>();
    generator.subschema_for::<crate::lists::ListCommandResponse>();
    generator.subschema_for::<crate::lists::PromoteListResponse>();
    // Memory review/edit/forget surface (FR-16, docs/02 §7). Memory records
    // are authenticated REST reads/writes and are not referenced by a WS
    // event, so register each root explicitly for schema/codegen coverage.
    generator.subschema_for::<crate::memories::MemoryDto>();
    generator.subschema_for::<crate::memories::MemoryListResponse>();
    generator.subschema_for::<crate::memories::PatchMemoryRequest>();
    // Voice push-to-talk control frames bracket binary PCM on the shared WS.
    generator.subschema_for::<crate::voice::VoiceControlDto>();

    let definitions: Value =
        serde_json::to_value(generator.definitions()).expect("schemas are valid JSON");
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "jarvis-contracts",
        "description": format!("Jarvis wire contract v{} (generated — do not edit)", crate::CONTRACT_VERSION),
        "definitions": definitions,
    })
}
