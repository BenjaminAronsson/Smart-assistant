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

use jarvis_domain::appspec::{
    AppLimitsDraft, AppSpec, AppSpecDraft, AppSpecError, DataBindingDraft, MAX_APP_SPEC_BYTES,
};
use jarvis_domain::artifact::Capability;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A capability a validated artifact manifest declares (docs/04 §4
/// `capabilities`). Exhaustive — the host vocabulary, mirrored on the wire.
/// One capability has **one** name on every surface: the dotted form the domain
/// uses, the DB column stores, and an inbound `AppSpecDto.capabilities` string
/// must contain. `rename_all = "snake_case"` would have produced
/// `home_read_state` here and `home.read_state` everywhere else, so a client
/// reading a manifest could not put that string back into a spec (F6.1 review).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum CapabilityDto {
    /// Read Home Assistant entity state (R0).
    #[serde(rename = "home.read_state")]
    HomeReadState,
    /// Set a single allowlisted light (R1).
    #[serde(rename = "home.set_light")]
    HomeSetLight,
    /// Activate an allowlisted scene (R2).
    #[serde(rename = "home.execute_scene")]
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

/// Why a spec document was rejected: it was not JSON of the right shape, or it
/// was well-formed but invalid.
#[derive(Debug, thiserror::Error)]
pub enum AppSpecParseError {
    #[error("app spec is not a valid spec document: {0}")]
    Malformed(String),
    #[error(transparent)]
    Invalid(#[from] AppSpecError),
}

/// **The** way to turn a spec document into an [`AppSpec`] — parse, then
/// validate against the document's own length.
///
/// [`AppSpec::validate`] takes `source_bytes` as a separate argument, which
/// makes the size gate impossible to *omit* but easy to get *wrong*: passing
/// `0`, or a stale length, silently disables `MAX_APP_SPEC_BYTES` with no
/// compile or test signal (F6.1 review). This function is the single place that
/// pairing is made, so callers never have to get it right themselves.
///
/// The byte cap is checked **before** deserialization, so a hostile document is
/// refused without building a `serde_json` value tree for it.
pub fn parse_and_validate(document: &str) -> Result<AppSpec, AppSpecParseError> {
    if document.len() > MAX_APP_SPEC_BYTES {
        return Err(AppSpecError::SpecTooLarge {
            bytes: document.len(),
            max: MAX_APP_SPEC_BYTES,
        }
        .into());
    }
    let dto: AppSpecDto = serde_json::from_str(document).map_err(|e| {
        // serde's message quotes the offending input, which is model-authored;
        // report position and expectation only.
        AppSpecParseError::Malformed(format!(
            "line {}, column {}: {}",
            e.line(),
            e.column(),
            e.classify_str()
        ))
    })?;
    Ok(AppSpec::validate(AppSpecDraft::from(dto), document.len())?)
}

/// A category name for a `serde_json` failure that quotes none of the input.
trait ClassifyStr {
    fn classify_str(&self) -> &'static str;
}

impl ClassifyStr for serde_json::Error {
    fn classify_str(&self) -> &'static str {
        use serde_json::error::Category;
        match self.classify() {
            Category::Io => "read error",
            Category::Syntax => "not well-formed JSON",
            Category::Data => "does not match the app-spec shape",
            Category::Eof => "ended unexpectedly",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::appspec::TemplateId;

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

    /// One name everywhere: the DTO's wire form is **exactly** the domain's
    /// `as_str()`, not a transformation of it. Asserting equality (rather than
    /// equality-after-a-`replace`) is what makes this test able to fail.
    #[test]
    fn capability_dto_serializes_to_exactly_the_domain_wire_name() {
        for capability in Capability::ALL {
            let dto = CapabilityDto::from(capability);
            let json = serde_json::to_string(&dto).expect("serializes");
            assert_eq!(
                json,
                format!("\"{}\"", capability.as_str()),
                "wire name drift for {capability}"
            );
            // And it round-trips from that same name.
            let back: CapabilityDto =
                serde_json::from_str(&json).expect("the emitted name deserializes");
            assert_eq!(back, dto);
        }
    }

    /// The surfaces are joined: a capability read off a manifest can be put
    /// straight back into a spec. Before the F6.1 review this was false.
    #[test]
    fn a_capability_from_a_manifest_is_accepted_in_a_spec() {
        for capability in Capability::ALL {
            let from_manifest = serde_json::to_value(CapabilityDto::from(capability))
                .expect("serializes")
                .as_str()
                .expect("a string")
                .to_owned();
            assert_eq!(
                from_manifest.parse::<Capability>().expect("round-trips"),
                capability
            );
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
        let spec = parse_and_validate(document).expect("a well-formed model spec must validate");

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

        let spec = parse_and_validate(document).expect("a capability-free app is valid");
        assert_eq!(spec.max_declared_risk(), None);
    }

    #[test]
    fn an_unknown_capability_in_model_json_is_rejected_by_the_domain_not_by_serde() {
        let document = r#"{
            "template": "dashboard/v1",
            "title": "Sneaky",
            "capabilities": ["shell.exec"]
        }"#;
        // It parses — strings are strings — and the DOMAIN rejects it, typed.
        let dto: AppSpecDto = serde_json::from_str(document).expect("parses");
        assert_eq!(dto.capabilities, vec!["shell.exec".to_owned()]);
        let err =
            parse_and_validate(document).expect_err("an invented capability must not validate");
        assert!(
            matches!(
                err,
                AppSpecParseError::Invalid(AppSpecError::UnknownCapability(_))
            ),
            "expected a typed domain rejection, got {err:?}"
        );
    }

    /// `AppLimitsDto` is the one DTO here whose field names differ between Rust
    /// and the wire, so camelCase is asserted directly rather than left to the
    /// schema snapshot (contract-keeper, F6.1).
    #[test]
    fn app_spec_dto_serializes_camel_case_including_limits() {
        let dto = AppSpecDto {
            template: "dashboard/v1".to_owned(),
            title: "Kitchen Dashboard".to_owned(),
            capabilities: vec!["home.read_state".to_owned()],
            bindings: vec![DataBindingDto {
                name: "kitchen_temp".to_owned(),
                capability: "home.read_state".to_owned(),
                target: "sensor.kitchen_temperature".to_owned(),
            }],
            limits: Some(AppLimitsDto {
                max_bundle_bytes: Some(1024),
                max_build_seconds: Some(30),
            }),
        };
        let v = serde_json::to_value(&dto).expect("serializes");
        assert_eq!(v["limits"]["maxBundleBytes"], serde_json::json!(1024));
        assert_eq!(v["limits"]["maxBuildSeconds"], serde_json::json!(30));
        assert_eq!(v["capabilities"][0], serde_json::json!("home.read_state"));
        assert_eq!(
            v["bindings"][0]["target"],
            serde_json::json!("sensor.kitchen_temperature")
        );

        let back: AppSpecDto = serde_json::from_value(v).expect("round-trips");
        assert_eq!(back, dto);
    }

    #[test]
    fn requested_limits_reach_the_validated_spec() {
        let document = r#"{
            "template": "dashboard/v1",
            "title": "Small",
            "limits": { "maxBundleBytes": 1024, "maxBuildSeconds": 30 }
        }"#;
        let spec = parse_and_validate(document).expect("below the ceiling is valid");
        assert_eq!(spec.limits().max_bundle_bytes(), 1024);
        assert_eq!(spec.limits().max_build_seconds(), 30);
    }

    /// The whole reason `parse_and_validate` exists: the size gate is paired
    /// with the document here, so no caller can pass a length that disagrees
    /// with what it parsed.
    #[test]
    fn an_oversized_document_is_refused_before_it_is_deserialized() {
        let padding = "x".repeat(MAX_APP_SPEC_BYTES);
        let document =
            format!(r#"{{ "template": "dashboard/v1", "title": "Short", "_pad": "{padding}" }}"#);
        assert!(document.len() > MAX_APP_SPEC_BYTES);
        match parse_and_validate(&document) {
            Err(AppSpecParseError::Invalid(AppSpecError::SpecTooLarge { bytes, max })) => {
                assert_eq!(bytes, document.len());
                assert_eq!(max, MAX_APP_SPEC_BYTES);
            }
            other => panic!("expected SpecTooLarge, got {other:?}"),
        }
    }

    /// A malformed document reports position and category only — never the
    /// model-authored text serde would otherwise quote (invariant 5).
    #[test]
    fn a_malformed_document_reports_no_untrusted_content() {
        let err = parse_and_validate(r#"{ "template": "dashboard/v1", "title": SECRETVALUE }"#)
            .expect_err("not JSON");
        let rendered = err.to_string();
        assert!(matches!(err, AppSpecParseError::Malformed(_)));
        assert!(
            !rendered.contains("SECRETVALUE"),
            "serde's quoted input leaked: {rendered}"
        );
    }
}
