//! F3a.3 artifact DTO wire-shape tests (docs/05 §1, FR-08): exact wire strings
//! for every enum variant, and `BuildProvenanceDto`'s omit-when-None behaviour.

use jarvis_contracts::artifacts::{
    ArtifactKindDto, ArtifactManifestDto, ArtifactSensitivityDto, ArtifactSourceDto,
    ArtifactSourceKindDto, BuildNetworkDto, BuildProvenanceDto,
};
use serde_json::json;

fn wire(value: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

#[test]
fn kind_wire_strings_are_snake_case() {
    assert_eq!(wire(ArtifactKindDto::MarkdownHtml), json!("markdown_html"));
    assert_eq!(wire(ArtifactKindDto::CodeText), json!("code_text"));
    assert_eq!(wire(ArtifactKindDto::Image), json!("image"));
    assert_eq!(wire(ArtifactKindDto::Chart), json!("chart"));
    assert_eq!(wire(ArtifactKindDto::Bundle), json!("bundle"));
}

#[test]
fn sensitivity_and_network_and_source_kind_wire_strings() {
    assert_eq!(wire(ArtifactSensitivityDto::Normal), json!("normal"));
    assert_eq!(wire(ArtifactSensitivityDto::Sensitive), json!("sensitive"));
    assert_eq!(wire(BuildNetworkDto::Disabled), json!("disabled"));
    assert_eq!(wire(BuildNetworkDto::Enabled), json!("enabled"));
    assert_eq!(wire(ArtifactSourceKindDto::Message), json!("message"));
    assert_eq!(wire(ArtifactSourceKindDto::Run), json!("run"));
    assert_eq!(wire(ArtifactSourceKindDto::Web), json!("web"));
}

#[test]
fn build_provenance_omits_none_fields() {
    let none = wire(BuildProvenanceDto {
        worker_image: None,
        lockfile_hash: None,
        network: BuildNetworkDto::Disabled,
    });
    // Absent, not present-as-null.
    assert_eq!(none, json!({ "network": "disabled" }));

    let full = wire(BuildProvenanceDto {
        worker_image: Some("jarvis-web-builder@sha256:abcd".to_owned()),
        lockfile_hash: Some("ab".repeat(32)),
        network: BuildNetworkDto::Enabled,
    });
    assert_eq!(full["workerImage"], json!("jarvis-web-builder@sha256:abcd"));
    assert_eq!(full["network"], json!("enabled"));
    assert!(full["lockfileHash"].is_string());
}

#[test]
fn source_dto_is_camel_case_kind_plus_reference() {
    let s = wire(ArtifactSourceDto {
        kind: ArtifactSourceKindDto::Web,
        reference: "https://en.wikipedia.org/wiki/Mitochondrion".to_owned(),
    });
    assert_eq!(s["kind"], json!("web"));
    assert_eq!(
        s["reference"],
        json!("https://en.wikipedia.org/wiki/Mitochondrion")
    );
}

#[test]
fn manifest_dto_round_trips_and_is_camel_case() {
    let dto = ArtifactManifestDto {
        id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
        version: 3,
        created_by_run: "01ARZ3NDEKTSV4RRFFQ69G5FB1".parse().unwrap(),
        sha256: "cd".repeat(32),
        media_type: "text/markdown".to_owned(),
        kind: ArtifactKindDto::MarkdownHtml,
        renderer: "markdown-html/v1".to_owned(),
        sources: vec![ArtifactSourceDto {
            kind: ArtifactSourceKindDto::Run,
            reference: "01ARZ3NDEKTSV4RRFFQ69G5FB1".to_owned(),
        }],
        sensitivity: ArtifactSensitivityDto::Sensitive,
        build: BuildProvenanceDto {
            worker_image: None,
            lockfile_hash: None,
            network: BuildNetworkDto::Disabled,
        },
        capabilities: vec!["artifact.read-own-data".to_owned()],
    };
    let v = wire(&dto);
    assert_eq!(v["createdByRun"], json!("01ARZ3NDEKTSV4RRFFQ69G5FB1"));
    assert_eq!(v["mediaType"], json!("text/markdown"));
    assert_eq!(v["version"], json!(3));
    assert_eq!(v["renderer"], json!("markdown-html/v1"));

    let back: ArtifactManifestDto = serde_json::from_value(v).unwrap();
    assert_eq!(back, dto);
}
