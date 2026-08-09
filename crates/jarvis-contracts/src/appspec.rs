//! Generated-app spec wire DTOs (FR-18, docs/06 §6, ADR-029).
//!
//! Two deliberately different shapes, for two deliberately different trust
//! positions:
//!
//! * **Inbound** ([`AppSpecDto`]) — a *model-authored* document. Its `template`
//!   and `capability` fields are plain strings, and that is not laziness: a
//!   closed serde enum on the way in would turn "you named a capability that
//!   does not exist" into an opaque deserialization failure at the edge, with
//!   no typed reason for the orchestrator to replan on and nothing meaningful
//!   for the audit trail. The strings are handed straight to
//!   [`jarvis_domain::appspec::AppSpec::validate`], which owns every rejection
//!   (invariant #1: the boundary parses, the domain decides).
//! * **Outbound** ([`CapabilityDto`]) — what a *validated* manifest carries.
//!   Closed, because by then only host vocabulary can be present, and a client
//!   reading a manifest should get an exhaustive union it can `switch` on
//!   rather than a `string` it has to guess about.

use jarvis_domain::appspec::{AppLimitsDraft, AppSpecDraft, DataBindingDraft};
use jarvis_domain::artifact::Capability;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A capability a validated artifact manifest declares (docs/04 §4
/// `capabilities`). Exhaustive — the host vocabulary, mirrored on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityDto {
    /// `home.read_state` — read Home Assistant entity state (R0).
    HomeReadState,
    /// `home.set_light` — set a single allowlisted light (R1).
    HomeSetLight,
    /// `home.execute_scene` — activate an allowlisted scene (R2).
    HomeExecuteScene,
}

impl From<Capability> for CapabilityDto {
    fn from(capability: Capability) -> Self {
        match capability {
            Capability::HomeReadState => CapabilityDto::HomeReadState,
            Capability::HomeSetLight => CapabilityDto::HomeSetLight,
            Capability::HomeExecuteScene => CapabilityDto::HomeExecuteScene,
        }
    }
}

impl From<CapabilityDto> for Capability {
    fn from(dto: CapabilityDto) -> Self {
        match dto {
            CapabilityDto::HomeReadState => Capability::HomeReadState,
            CapabilityDto::HomeSetLight => Capability::HomeSetLight,
            CapabilityDto::HomeExecuteScene => Capability::HomeExecuteScene,
        }
    }
}

/// One data binding the app declares (see [`AppSpecDto`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DataBindingDto {
    /// Identifier the template binds this data under (`[a-z][a-z0-9_]*`).
    pub name: String,
    /// The declared capability that supplies it — validated host-side against
    /// the closed vocabulary.
    pub capability: String,
    /// The resource it addresses (e.g. an entity id). Opaque to the host; the
    /// backing tool's own allowlist resolves it at call time.
    pub target: String,
}

/// Build limits the spec requests (docs/06 §6 "size/time limits"). Omitted
/// fields mean the host ceiling; a value **above** the ceiling is rejected, not
/// clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppLimitsDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bundle_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_build_seconds: Option<u32>,
}

/// The app spec as it arrives from a model (FR-18 "validated templates").
/// Untrusted in full: nothing here is authority, and everything here is
/// validated by the domain before a build starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppSpecDto {
    /// Host template id, e.g. `dashboard/v1`. Unknown ids are rejected.
    pub template: String,
    /// Single-line title shown in the app's window chrome.
    pub title: String,
    /// Capabilities the app declares it needs. May be empty — an app that needs
    /// no authority is the best kind. Unknown names are rejected.
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub bindings: Vec<DataBindingDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<AppLimitsDto>,
}

impl From<AppSpecDto> for AppSpecDraft {
    fn from(dto: AppSpecDto) -> Self {
        AppSpecDraft {
            template: dto.template,
            title: dto.title,
            capabilities: dto.capabilities,
            bindings: dto
                .bindings
                .into_iter()
                .map(|b| DataBindingDraft {
                    name: b.name,
                    capability: b.capability,
                    target: b.target,
                })
                .collect(),
            limits: dto.limits.map(|l| AppLimitsDraft {
                max_bundle_bytes: l.max_bundle_bytes,
                max_build_seconds: l.max_build_seconds,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::appspec::{AppSpec, TemplateId};

    #[test]
    fn capability_dto_round_trips_through_the_domain_for_every_variant() {
        // Exhaustive by construction: driven off the domain's own `ALL`, so a
        // new host capability without a DTO variant fails here rather than
        // shipping a manifest the client cannot name.
        for capability in Capability::ALL {
            let dto = CapabilityDto::from(capability);
            assert_eq!(Capability::from(dto), capability);
        }
    }

    #[test]
    fn capability_dto_serializes_to_the_domain_wire_name() {
        for capability in Capability::ALL {
            let dto = CapabilityDto::from(capability);
            let json = serde_json::to_string(&dto).expect("serializes");
            let expected = format!("\"{}\"", capability.as_str().replace('.', "_"));
            assert_eq!(json, expected, "wire name drift for {capability}");
        }
    }

    /// The fixture-vs-caller check (M5 lesson): a spec built the way the REAL
    /// producer builds it — a JSON document deserialized into [`AppSpecDto`],
    /// converted to a draft, validated with that document's own byte length —
    /// must be ACCEPTED. A test that hand-builds an `AppSpecDraft` would prove
    /// nothing about the path the model's output actually takes.
    #[test]
    fn a_spec_deserialized_from_model_shaped_json_validates_end_to_end() {
        let document = r#"{
            "template": "dashboard/v1",
            "title": "Kitchen Dashboard",
            "capabilities": ["home.read_state"],
            "bindings": [
                { "name": "kitchen_temp",
                  "capability": "home.read_state",
                  "target": "sensor.kitchen_temperature" }
            ]
        }"#;
        let dto: AppSpecDto = serde_json::from_str(document).expect("model-shaped JSON parses");
        let spec = AppSpec::validate(AppSpecDraft::from(dto), document.len())
            .expect("a well-formed model spec must validate");

        assert_eq!(spec.template(), TemplateId::Dashboard);
        assert_eq!(spec.capabilities(), &[Capability::HomeReadState]);
        assert_eq!(
            spec.bindings()[0].target().as_str(),
            "sensor.kitchen_temperature"
        );
    }

    #[test]
    fn optional_collections_default_so_a_minimal_spec_parses() {
        let document = r#"{ "template": "dashboard/v1", "title": "Clock" }"#;
        let dto: AppSpecDto = serde_json::from_str(document).expect("minimal JSON parses");
        assert!(dto.capabilities.is_empty());
        assert!(dto.bindings.is_empty());
        assert_eq!(dto.limits, None);

        let spec = AppSpec::validate(AppSpecDraft::from(dto), document.len())
            .expect("a capability-free app is valid");
        assert_eq!(spec.max_declared_risk(), None);
    }

    #[test]
    fn an_unknown_capability_in_model_json_is_rejected_by_the_domain_not_by_serde() {
        let document = r#"{
            "template": "dashboard/v1",
            "title": "Sneaky",
            "capabilities": ["shell.exec"]
        }"#;
        let dto: AppSpecDto = serde_json::from_str(document).expect("parses — strings are strings");
        let err = AppSpec::validate(AppSpecDraft::from(dto), document.len())
            .expect_err("an invented capability must not validate");
        assert!(
            matches!(
                err,
                jarvis_domain::appspec::AppSpecError::UnknownCapability(_)
            ),
            "expected a typed domain rejection, got {err:?}"
        );
    }
}
