//! Artifact read surface (docs/05 §1, FR-08): list an artifact's versions with
//! provenance, and download a version's blob. Wire DTOs at the boundary; domain
//! types inside.
//!
//! Creation is not a client endpoint — artifacts are run outputs (the coding
//! worker F3a.6, deep-dive promotion F3b.6), produced through the
//! [`ArtifactStore`]/[`BlobStore`] ports, never POSTed by a client. This module
//! is the read half that "reopen the artifact after restart" (exit evidence #1)
//! and the HUD renderers (F3b.3) consume.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use jarvis_application::ports::{ArtifactStore, BlobStore, BlobStoreError, RepositoryError};
use jarvis_contracts::artifacts::{
    ArtifactKindDto, ArtifactManifestDto, ArtifactSensitivityDto, ArtifactSourceDto,
    ArtifactSourceKindDto, ArtifactVersionsResponse, BuildNetworkDto, BuildProvenanceDto,
};
use jarvis_contracts::errors::ErrorCode;
use jarvis_domain::artifact::{
    ArtifactKind, ArtifactManifest, ArtifactSource, ArtifactVersion, BuildNetwork,
};
use jarvis_domain::grants::Sha256;
use jarvis_domain::ids::ArtifactId;
use jarvis_domain::location::Sensitivity;

use crate::problem::problem;

/// The artifact read API: the manifest store plus the blob store, joined by the
/// content hash. Cloneable so it can be axum route state.
#[derive(Clone)]
pub struct ArtifactApi {
    store: Arc<dyn ArtifactStore>,
    blobs: Arc<dyn BlobStore>,
}

impl ArtifactApi {
    pub fn new(store: Arc<dyn ArtifactStore>, blobs: Arc<dyn BlobStore>) -> Self {
        Self { store, blobs }
    }
}

fn not_found(what: &str) -> Response {
    problem(
        StatusCode::NOT_FOUND,
        ErrorCode::ResourceNotFound,
        what,
        None,
    )
}

/// One mapping for every RepositoryError crossing the boundary (docs/05 §7).
/// Storage internals never reach the client.
fn repository_problem(error: RepositoryError) -> Response {
    match error {
        RepositoryError::Conflict(_) | RepositoryError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            ErrorCode::ResourceVersionConflict,
            "artifact version conflict",
            None,
        ),
        RepositoryError::Storage(e) => {
            tracing::error!(error = %e, "artifact storage failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "storage unavailable",
                None,
            )
        }
    }
}

/// `GET /api/v1/artifacts/{id}/versions` — all versions, oldest first (FR-08).
/// An unknown artifact is a 404, not an empty 200 — the id names nothing.
pub async fn list_versions(
    State(api): State<ArtifactApi>,
    Path(id): Path<String>,
) -> Result<Json<ArtifactVersionsResponse>, Response> {
    let id = id
        .parse::<ArtifactId>()
        .map_err(|_| not_found("no such artifact"))?;
    let versions = api
        .store
        .list_versions(&id)
        .await
        .map_err(repository_problem)?;
    if versions.is_empty() {
        return Err(not_found("no such artifact"));
    }
    Ok(Json(ArtifactVersionsResponse {
        artifact_id: id,
        versions: versions.iter().map(to_manifest_dto).collect(),
    }))
}

/// `GET /api/v1/artifacts/{id}/versions/{version}/blob` — the version's bytes,
/// content-addressed. The ETag is the blob's sha256 (immutable content ⇒ a
/// strong validator); a matching `If-None-Match` short-circuits to 304. A blob
/// that fails verify-on-read is a 500 that returns no bytes (fail closed).
pub async fn get_blob(
    State(api): State<ArtifactApi>,
    Path((id, version)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let id = id
        .parse::<ArtifactId>()
        .map_err(|_| not_found("no such artifact"))?;
    // Parse the version inside the handler (not via `Path<(_, u32)>`) so a
    // malformed version is our RFC 9457 problem body, not axum's default
    // plain-text 400 that leaks the param type.
    let version = version
        .parse::<u32>()
        .ok()
        .and_then(ArtifactVersion::new)
        .ok_or_else(|| not_found("no such artifact version"))?;

    let manifest = api
        .store
        .get(&id, version)
        .await
        .map_err(repository_problem)?
        .ok_or_else(|| not_found("no such artifact version"))?;

    let sha_hex = manifest.sha256().to_string();
    let etag = format!("\"{sha_hex}\"");
    // Content-addressed caching: if the client already holds this exact blob,
    // don't resend it. Any of the comma-separated If-None-Match tags may match.
    if let Some(inm) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        && inm.split(',').any(|t| t.trim() == etag || t.trim() == "*")
    {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    let bytes = match api.blobs.get(manifest.sha256()).await {
        Ok(Some(bytes)) => bytes,
        // Manifest exists but its blob does not — a dangling manifest. The
        // invariant is blob-before-manifest, so this is a data-integrity
        // condition worth surfacing, not a routine miss; warn, then 404.
        Ok(None) => {
            tracing::warn!(artifact = %id, "manifest present but its blob is missing (dangling)");
            return Err(not_found("artifact blob is unavailable"));
        }
        Err(BlobStoreError::IntegrityMismatch) => {
            tracing::error!(artifact = %id, "artifact blob failed integrity verification");
            return Err(problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::ArtifactIntegrityFailed,
                "artifact blob failed integrity verification",
                None,
            ));
        }
        Err(BlobStoreError::Io(e)) => {
            tracing::error!(error = %e, "artifact blob read failure");
            return Err(problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "storage unavailable",
                None,
            ));
        }
    };

    // Anti-execution guard (docs/06 §6): artifact bytes are run outputs derived
    // from untrusted input (fetched pages, model output) and are served from the
    // SAME origin as the control UI. `text/html` or `image/svg+xml` would
    // otherwise execute script in that origin on direct navigation. `nosniff`
    // pins the declared type and `attachment` forces download, not inline render
    // — the HUD renderer (F3b.3) is the only sanctioned place artifacts render.
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                manifest.media_type().as_str().to_owned(),
            ),
            (header::ETAG, etag),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
            (header::CONTENT_DISPOSITION, "attachment".to_owned()),
            // Content-addressed ⇒ a given URL's bytes never change.
            (header::CACHE_CONTROL, "private, immutable".to_owned()),
        ],
        Body::from(bytes),
    )
        .into_response())
}

fn to_manifest_dto(m: &ArtifactManifest) -> ArtifactManifestDto {
    let build = m.build();
    ArtifactManifestDto {
        id: m.id().clone(),
        version: m.version().get(),
        created_by_run: m.created_by_run().clone(),
        sha256: m.sha256().to_string(),
        media_type: m.media_type().as_str().to_owned(),
        kind: kind_dto(m.kind()),
        renderer: m.renderer_id().to_owned(),
        sources: m.sources().iter().map(source_dto).collect(),
        sensitivity: sensitivity_dto(m.sensitivity()),
        build: BuildProvenanceDto {
            worker_image: build.worker_image.clone(),
            lockfile_hash: build.lockfile_hash.as_ref().map(Sha256::to_string),
            network: network_dto(build.network),
        },
        capabilities: m
            .capabilities()
            .iter()
            .map(|c| c.as_str().to_owned())
            .collect(),
    }
}

fn kind_dto(kind: ArtifactKind) -> ArtifactKindDto {
    match kind {
        ArtifactKind::MarkdownHtml => ArtifactKindDto::MarkdownHtml,
        ArtifactKind::CodeText => ArtifactKindDto::CodeText,
        ArtifactKind::Image => ArtifactKindDto::Image,
        ArtifactKind::Chart => ArtifactKindDto::Chart,
        ArtifactKind::Bundle => ArtifactKindDto::Bundle,
    }
}

fn sensitivity_dto(s: Sensitivity) -> ArtifactSensitivityDto {
    match s {
        Sensitivity::Normal => ArtifactSensitivityDto::Normal,
        Sensitivity::Sensitive => ArtifactSensitivityDto::Sensitive,
    }
}

fn network_dto(n: BuildNetwork) -> BuildNetworkDto {
    match n {
        BuildNetwork::Disabled => BuildNetworkDto::Disabled,
        BuildNetwork::Enabled => BuildNetworkDto::Enabled,
    }
}

fn source_dto(s: &ArtifactSource) -> ArtifactSourceDto {
    match s {
        ArtifactSource::Message(id) => ArtifactSourceDto {
            kind: ArtifactSourceKindDto::Message,
            reference: id.as_str().to_owned(),
        },
        ArtifactSource::Run(id) => ArtifactSourceDto {
            kind: ArtifactSourceKindDto::Run,
            reference: id.as_str().to_owned(),
        },
        ArtifactSource::Web { url } => ArtifactSourceDto {
            kind: ArtifactSourceKindDto::Web,
            reference: url.clone(),
        },
    }
}
