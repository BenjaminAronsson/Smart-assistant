//! Artifact wire DTOs (docs/05 §1, FR-08). The **read** surface: list an
//! artifact's versions with full provenance. Manifests are immutable — a new
//! version is a new entry, never a mutation (docs/04 §4). The blob bytes are
//! fetched separately (`GET …/versions/{v}/blob`), not inlined here.
//!
//! The `artifact.created` WS event and any create/promote request DTOs land
//! with their first producer (F3a.6 coding worker / F3b.6 deep-dive promotion) —
//! no producer-less replayable event ships ahead of its emitter (the F2.5→F2.6
//! precedent).

use jarvis_domain::ids::{ArtifactId, RunId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The logical kind of an artifact, selecting its renderer (docs/02 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKindDto {
    MarkdownHtml,
    CodeText,
    Image,
    Chart,
    Bundle,
}

/// Sensitivity class of the artifact (NFR-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSensitivityDto {
    Normal,
    Sensitive,
}

/// The network policy under which the artifact was built (docs/04 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BuildNetworkDto {
    Disabled,
    Enabled,
}

/// What kind of thing a provenance source refers to (docs/04 §4 `sources`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSourceKindDto {
    Message,
    Run,
    Web,
}

/// One provenance source: what it is plus its reference (a ULID for
/// message/run, a URL for web). The web shell renders these as a sources card
/// (F3b.6), each with its own attribution (FR-27/ADR-017).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactSourceDto {
    pub kind: ArtifactSourceKindDto,
    /// The message/run ULID or the web URL, per `kind`.
    pub reference: String,
}

/// How the artifact was built (docs/04 §4 `build`). Carries only hashes and
/// image references — never secrets (invariant 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuildProvenanceDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lockfile_hash: Option<String>,
    pub network: BuildNetworkDto,
}

/// One immutable artifact-version manifest (docs/02 §6, docs/04 §4, FR-08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactManifestDto {
    #[schemars(with = "crate::schema::UlidString")]
    pub id: ArtifactId,
    /// Monotonic version, 1-based.
    pub version: u32,
    #[schemars(with = "crate::schema::UlidString")]
    pub created_by_run: RunId,
    /// Content address of the blob (lowercase hex sha256) — also the blob's
    /// download ETag.
    pub sha256: String,
    pub media_type: String,
    pub kind: ArtifactKindDto,
    /// Versioned renderer id (docs/04 §4), derived from `kind`.
    pub renderer: String,
    pub sources: Vec<ArtifactSourceDto>,
    pub sensitivity: ArtifactSensitivityDto,
    pub build: BuildProvenanceDto,
    pub capabilities: Vec<String>,
}

/// `GET /api/v1/artifacts/{id}/versions` — every version of one artifact,
/// oldest first (docs/05 §1, FR-08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactVersionsResponse {
    #[schemars(with = "crate::schema::UlidString")]
    pub artifact_id: ArtifactId,
    pub versions: Vec<ArtifactManifestDto>,
}
