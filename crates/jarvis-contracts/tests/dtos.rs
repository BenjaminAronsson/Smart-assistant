//! F0.3: health/pairing/session/content DTO round-trips (docs/05 §1, §6; skill
//! `ws-contracts`).
//!
//! Note on `CreateSessionRequest`: docs/05 §1 documents the endpoint
//! (`POST /api/v1/sessions`) but not its body shape. The feature spec handed to this
//! test suite also does not enumerate its fields. This suite assumes the minimal
//! shape consistent with `SessionDto` (an optional `title`), matching the "Create a
//! session" description. **This is an assumption, not a confirmed contract** — flag
//! for the implementer/human to confirm against the agreed API; if the real shape
//! differs, `create_session_request` tests below are expected to need updating
//! alongside the implementation, not silently reinterpreted.

use jarvis_contracts::auth::{PairRequest, PairResponse};
use jarvis_contracts::content::ContentBlock;
use jarvis_contracts::health::{AdapterHealth, AdapterState, HealthResponse, ServiceStatus};
use jarvis_contracts::sessions::{
    CreateSessionRequest, SessionDto, SessionListResponse, SessionStatus,
};
use serde_json::json;

mod health {
    use super::*;

    #[test]
    fn health_response_round_trips_with_multiple_adapters() {
        let value = json!({
            "status": "degraded",
            "version": "0.1.0",
            "adapters": {
                "postgres": { "state": "up" },
                "home-assistant": { "state": "down", "detail": "connection refused" },
                "claude-cli": { "state": "disabled" }
            }
        });
        let health: HealthResponse =
            serde_json::from_value(value.clone()).expect("HealthResponse must deserialize");
        let round_tripped = serde_json::to_value(&health).unwrap();
        assert_eq!(round_tripped, value);
    }

    #[test]
    fn health_response_adapter_detail_is_omitted_when_absent() {
        let value = json!({
            "status": "ok",
            "version": "0.1.0",
            "adapters": {
                "postgres": { "state": "up" }
            }
        });
        let health: HealthResponse = serde_json::from_value(value.clone()).unwrap();
        let round_tripped = serde_json::to_value(&health).unwrap();
        let adapter = &round_tripped["adapters"]["postgres"];
        assert!(
            !adapter.as_object().unwrap().contains_key("detail"),
            "adapter detail must be omitted, not null, when absent"
        );
        assert_eq!(round_tripped, value);
    }

    #[test]
    fn service_status_values_round_trip() {
        for (variant, wire) in [
            (ServiceStatus::Ok, "ok"),
            (ServiceStatus::Degraded, "degraded"),
        ] {
            let json = serde_json::to_value(variant).unwrap();
            assert_eq!(json, serde_json::Value::String(wire.into()));
            let back: ServiceStatus = serde_json::from_value(json).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn service_status_rejects_unknown_value() {
        let result: Result<ServiceStatus, _> = serde_json::from_value(json!("healthy"));
        assert!(result.is_err());
    }

    #[test]
    fn adapter_state_values_round_trip() {
        for (variant, wire) in [
            (AdapterState::Up, "up"),
            (AdapterState::Down, "down"),
            (AdapterState::Disabled, "disabled"),
        ] {
            let json = serde_json::to_value(variant).unwrap();
            assert_eq!(json, serde_json::Value::String(wire.into()));
            let back: AdapterState = serde_json::from_value(json).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn adapter_state_rejects_unknown_value() {
        let result: Result<AdapterState, _> = serde_json::from_value(json!("starting"));
        assert!(result.is_err());
    }

    #[test]
    fn adapter_health_round_trips_with_detail() {
        let value = json!({ "state": "down", "detail": "timeout after 5s" });
        let adapter: AdapterHealth = serde_json::from_value(value.clone()).unwrap();
        let round_tripped = serde_json::to_value(&adapter).unwrap();
        assert_eq!(round_tripped, value);
    }
}

mod auth {
    use super::*;

    #[test]
    fn pair_request_round_trips() {
        let value = json!({
            "pairingCode": "123-456",
            "deviceName": "owner-laptop"
        });
        let req: PairRequest =
            serde_json::from_value(value.clone()).expect("PairRequest must deserialize");
        let round_tripped = serde_json::to_value(&req).unwrap();
        assert_eq!(round_tripped, value);
    }

    #[test]
    fn pair_request_rejects_missing_pairing_code() {
        let value = json!({ "deviceName": "owner-laptop" });
        let result: Result<PairRequest, _> = serde_json::from_value(value);
        assert!(result.is_err());
    }

    #[test]
    fn pair_response_round_trips() {
        let value = json!({
            "deviceId": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "deviceToken": "opaque-256-bit-token",
            // F7.1: the server states the class it assigned, for the same
            // reason it states the scopes — a client is told its authority.
            "deviceClass": "owner-ui",
            "scopes": ["ui", "display-agent"]
        });
        let resp: PairResponse =
            serde_json::from_value(value.clone()).expect("PairResponse must deserialize");
        let round_tripped = serde_json::to_value(&resp).unwrap();
        assert_eq!(round_tripped, value);
    }

    #[test]
    fn pair_response_device_id_must_be_a_valid_ulid() {
        // deviceId is documented as "ULID string" (docs/05 §6 read alongside
        // docs/04 §2 "All IDs are ULIDs exposed as opaque strings").
        let value = json!({
            "deviceId": "not-a-ulid",
            "deviceToken": "opaque-256-bit-token",
            "scopes": []
        });
        let result: Result<PairResponse, _> = serde_json::from_value(value);
        assert!(
            result.is_err(),
            "PairResponse.deviceId must reject a non-ULID string"
        );
    }

    #[test]
    fn pair_response_scopes_accepts_empty_list() {
        let value = json!({
            "deviceId": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "deviceToken": "opaque-256-bit-token",
            "deviceClass": "display-node",
            "scopes": []
        });
        let resp: PairResponse = serde_json::from_value(value.clone()).unwrap();
        let round_tripped = serde_json::to_value(&resp).unwrap();
        assert_eq!(round_tripped, value);
    }
}

mod sessions {
    use super::*;

    #[test]
    fn session_dto_round_trips_with_title() {
        let value = json!({
            "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "title": "Kitchen automation planning",
            "status": "active",
            "createdAt": "2026-07-17T10:00:00Z",
            "updatedAt": "2026-07-18T09:15:30Z"
        });
        let dto: SessionDto =
            serde_json::from_value(value.clone()).expect("SessionDto must deserialize");
        let round_tripped = serde_json::to_value(&dto).unwrap();
        assert_eq!(round_tripped, value);
    }

    #[test]
    fn session_dto_omits_title_when_absent() {
        let value = json!({
            "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "status": "archived",
            "createdAt": "2026-07-17T10:00:00Z",
            "updatedAt": "2026-07-18T09:15:30Z"
        });
        let dto: SessionDto = serde_json::from_value(value.clone()).unwrap();
        let round_tripped = serde_json::to_value(&dto).unwrap();
        assert!(!round_tripped.as_object().unwrap().contains_key("title"));
        assert_eq!(round_tripped, value);
    }

    #[test]
    fn session_dto_id_must_be_a_valid_ulid() {
        let value = json!({
            "id": "definitely-not-a-ulid",
            "status": "active",
            "createdAt": "2026-07-17T10:00:00Z",
            "updatedAt": "2026-07-18T09:15:30Z"
        });
        let result: Result<SessionDto, _> = serde_json::from_value(value);
        assert!(
            result.is_err(),
            "SessionDto.id must reject a non-ULID string"
        );
    }

    #[test]
    fn session_status_values_round_trip() {
        for (variant, wire) in [
            (SessionStatus::Active, "active"),
            (SessionStatus::Archived, "archived"),
        ] {
            let json = serde_json::to_value(variant).unwrap();
            assert_eq!(json, serde_json::Value::String(wire.into()));
            let back: SessionStatus = serde_json::from_value(json).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn session_status_rejects_unknown_value() {
        let result: Result<SessionStatus, _> = serde_json::from_value(json!("deleted"));
        assert!(result.is_err());
    }

    #[test]
    fn session_list_response_round_trips_with_cursor() {
        let value = json!({
            "sessions": [
                {
                    "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                    "status": "active",
                    "createdAt": "2026-07-17T10:00:00Z",
                    "updatedAt": "2026-07-18T09:15:30Z"
                }
            ],
            "nextCursor": "opaque-cursor-token"
        });
        let list: SessionListResponse =
            serde_json::from_value(value.clone()).expect("SessionListResponse must deserialize");
        let round_tripped = serde_json::to_value(&list).unwrap();
        assert_eq!(round_tripped, value);
    }

    #[test]
    fn session_list_response_omits_cursor_when_no_more_pages() {
        let value = json!({ "sessions": [] });
        let list: SessionListResponse = serde_json::from_value(value.clone()).unwrap();
        let round_tripped = serde_json::to_value(&list).unwrap();
        assert!(
            !round_tripped
                .as_object()
                .unwrap()
                .contains_key("nextCursor")
        );
        assert_eq!(round_tripped, value);
    }

    /// ASSUMPTION (see module-level doc comment): CreateSessionRequest is assumed to
    /// carry only an optional `title`, mirroring SessionDto. Confirm against the
    /// agreed API before relying on this test as ground truth.
    #[test]
    fn create_session_request_round_trips_with_title() {
        let value = json!({ "title": "New session" });
        let req: CreateSessionRequest = serde_json::from_value(value.clone())
            .expect("CreateSessionRequest with a title must deserialize");
        let round_tripped = serde_json::to_value(&req).unwrap();
        assert_eq!(round_tripped, value);
    }

    /// ASSUMPTION (see module-level doc comment): title is optional, so the empty
    /// request body is valid ("Create a session" with no explicit title).
    #[test]
    fn create_session_request_allows_missing_title() {
        let value = json!({});
        let result: Result<CreateSessionRequest, _> = serde_json::from_value(value);
        assert!(
            result.is_ok(),
            "CreateSessionRequest must allow an absent/optional title"
        );
    }
}

mod content {
    use super::*;

    #[test]
    fn text_block_serializes_with_snake_case_type_tag() {
        let value = json!({ "type": "text", "text": "hello, jarvis" });
        let block: ContentBlock =
            serde_json::from_value(value.clone()).expect("ContentBlock::Text must deserialize");
        let round_tripped = serde_json::to_value(&block).unwrap();
        assert_eq!(round_tripped, value);
    }

    #[test]
    fn unknown_type_tag_is_tolerated_as_the_unknown_variant() {
        // Additive evolution (docs/05 §5): a newer peer's variant must degrade
        // gracefully for older readers, never fail the whole message.
        let value = json!({ "type": "not_a_real_block", "text": "hi" });
        let block: ContentBlock = serde_json::from_value(value).unwrap();
        assert_eq!(block, ContentBlock::Unknown);
    }

    #[test]
    fn text_block_rejects_missing_type_tag() {
        let value = json!({ "text": "hi" });
        let result: Result<ContentBlock, _> = serde_json::from_value(value);
        assert!(result.is_err());
    }

    #[test]
    fn text_block_rejects_missing_text_field() {
        let value = json!({ "type": "text" });
        let result: Result<ContentBlock, _> = serde_json::from_value(value);
        assert!(result.is_err());
    }
}

/// F7.1 (FR-19): the device-management surface. Pins the exact wire strings —
/// `crates/jarvisd/tests/devices_api.rs` drives these through a live router,
/// which is the caller-side check; this is the contract-side one.
mod devices {
    use super::*;
    use jarvis_contracts::devices::{DeviceDto, DeviceListResponse, RevokeDeviceRequest};

    fn a_device() -> serde_json::Value {
        json!({
            "deviceId": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "name": "kitchen screen",
            "deviceClass": "room-node",
            "scopes": ["display-agent", "voice-capture"],
            "executesTools": false,
            "createdAt": "2026-08-12T09:00:00Z",
            "lastSeenAt": null,
            "revokedAt": null,
            "revokedReason": null
        })
    }

    #[test]
    fn device_dto_round_trips() {
        let value = a_device();
        let dto: DeviceDto =
            serde_json::from_value(value.clone()).expect("DeviceDto must deserialize");
        assert_eq!(serde_json::to_value(&dto).unwrap(), value);
    }

    #[test]
    fn device_id_must_be_a_valid_ulid() {
        let mut value = a_device();
        value["deviceId"] = json!("not-a-ulid");
        let result: Result<DeviceDto, _> = serde_json::from_value(value);
        assert!(result.is_err(), "DeviceDto.deviceId must reject a non-ULID");
    }

    /// A device list is a management read, not a credential dump: the DTO has
    /// no field that could carry a token or its hash, and this test fails if
    /// one is ever added.
    #[test]
    fn device_dto_carries_no_credential_material() {
        let dto: DeviceDto = serde_json::from_value(a_device()).expect("deserializes");
        let wire = serde_json::to_value(&dto).unwrap();
        let fields: Vec<&String> = wire.as_object().expect("object").keys().collect();
        for forbidden in ["token", "tokenHash", "secret", "publicKey", "userId"] {
            assert!(
                !fields.iter().any(|f| f.as_str() == forbidden),
                "DeviceDto must not carry `{forbidden}`: {fields:?}"
            );
        }
    }

    #[test]
    fn a_revoked_device_carries_its_timestamp_and_reason() {
        let mut value = a_device();
        value["revokedAt"] = json!("2026-08-12T10:00:00Z");
        value["revokedReason"] = json!("sold the screen");
        let dto: DeviceDto = serde_json::from_value(value.clone()).expect("deserializes");
        assert_eq!(serde_json::to_value(&dto).unwrap(), value);
    }

    #[test]
    fn device_list_round_trips_and_accepts_an_empty_list() {
        let value = json!({ "devices": [] });
        let dto: DeviceListResponse = serde_json::from_value(value.clone()).expect("deserializes");
        assert_eq!(serde_json::to_value(&dto).unwrap(), value);
    }

    /// The reason is optional: revoking a lost device must not require an
    /// essay, and an empty body is the common case.
    #[test]
    fn revoke_request_reason_is_optional() {
        let empty: RevokeDeviceRequest =
            serde_json::from_value(json!({})).expect("an empty body is valid");
        assert_eq!(empty.reason, None);
        let given: RevokeDeviceRequest =
            serde_json::from_value(json!({ "reason": "stolen" })).expect("deserializes");
        assert_eq!(given.reason.as_deref(), Some("stolen"));
    }
}
