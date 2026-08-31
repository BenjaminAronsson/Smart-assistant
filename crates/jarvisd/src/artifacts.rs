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
use jarvis_application::ports::{
    ArtifactStore, BlobStore, BlobStoreError, MAX_SERVED_BLOB_BYTES, RepositoryError,
};
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

use crate::problem::{not_found, problem};

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

/// One mapping for every RepositoryError crossing the boundary (docs/05 §7).
/// Storage internals never reach the client.
fn repository_problem(error: RepositoryError) -> Response {
    crate::problem::repository_problem_merged_idempotency(
        error,
        "artifact",
        "artifact version conflict",
        "storage unavailable",
    )
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

    // F6.3 (closes CF-M3a-A): stream the blob instead of buffering it, under a
    // served-size cap. `open` verifies the whole blob *before* yielding a byte,
    // so the fail-closed behaviour below is unchanged — a corrupt blob is still
    // a 500 with no content — while peak memory drops from one blob to one
    // chunk. Bundles (M6) are the artifacts that made this necessary.
    let blob = match api
        .blobs
        .open(manifest.sha256(), MAX_SERVED_BLOB_BYTES)
        .await
    {
        Ok(Some(blob)) => blob,
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
        Err(BlobStoreError::TooLarge { len, max }) => {
            // Refused whole, never truncated: a prefix of a blob is not the
            // blob, and its bytes would not hash to the address in the URL.
            tracing::error!(artifact = %id, len, max, "artifact blob exceeds the served-size cap");
            return Err(problem(
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::ArtifactTooLarge,
                "artifact blob exceeds the served-size limit",
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
    // F6.4's sandboxed bundle route is a *separate* route; this one is never
    // relaxed to serve a renderable app.
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                manifest.media_type().as_str().to_owned(),
            ),
            // Known because the blob was verified end to end before streaming.
            (header::CONTENT_LENGTH, blob.len.to_string()),
            (header::ETAG, etag),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
            (header::CONTENT_DISPOSITION, "attachment".to_owned()),
            // Content-addressed ⇒ a given URL's bytes never change.
            (header::CACHE_CONTROL, "private, immutable".to_owned()),
        ],
        Body::from_stream(blob.chunks),
    )
        .into_response())
}

// --- the generated-app sandbox (F6.4, ADR-030) ------------------------------

/// The Content-Security-Policy every generated app is rendered under (docs/06 §6,
/// F6.4). Sent as a real response header **and** prepended to the document as a
/// `<meta http-equiv>`, because the shell renders the bytes through `srcdoc`,
/// where the response's own header no longer applies — the meta is what binds in
/// the frame, the header is what binds if the URL is ever navigated directly.
///
/// Every directive earns its place:
/// * `sandbox` (header only — `<meta>` may not carry it) forces an **opaque
///   origin** for a directly navigated document, independently of the frame
///   attribute the shell sets.
/// * `default-src 'none'` denies everything not named below, including
///   `connect-src`, so a rendered app cannot fetch, XHR or open a socket. An app
///   built from model output cannot phone home.
/// * `script-src 'unsafe-inline'` is required, not conceded: a single-file bundle
///   *is* one inline module script. Letting the app's own script run is the point
///   of rendering it; what the policy denies is everything that script could
///   reach.
/// * `img-src`/`font-src data:` keep inlined assets working while allowing no
///   off-box load.
/// * `base-uri 'none'` stops a `<base>` tag from re-pointing relative URLs;
///   `form-action 'none'` stops navigation-by-submit as an egress channel.
const APP_DOCUMENT_CSP: &str = "sandbox allow-scripts; default-src 'none'; \
script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:; \
connect-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'self'";

/// The same policy minus the directives `<meta http-equiv>` ignores (`sandbox`,
/// `frame-ancestors`). Kept separate rather than derived by string surgery so
/// what is served is what is written down.
const APP_DOCUMENT_META_CSP: &str = "default-src 'none'; \
script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:; \
connect-src 'none'; form-action 'none'; base-uri 'none'";

/// `GET /api/v1/apps/{id}/versions/{version}/document` — a `Bundle` artifact
/// rendered as an app document (F6.4, ADR-030).
///
/// **A separate route on purpose.** The blob route (`…/blob`) serves every
/// artifact as `attachment` + `nosniff` so it is never rendered inline — an M3a
/// control that stays exactly as it is. This route is the one deliberately
/// renderable path, and it is narrow: only `ArtifactKind::Bundle`, only under
/// [`APP_DOCUMENT_CSP`], only to an authenticated device.
///
/// The document is prefixed with a `<meta http-equiv>` CSP before it is served.
/// The prefix is host-authored and comes first, so a policy the bundle itself
/// declares can only *intersect* with it (CSP composes; a second policy can
/// never loosen the first).
pub async fn get_app_document(
    State(api): State<ArtifactApi>,
    Path((id, version)): Path<(String, String)>,
) -> Result<Response, Response> {
    let id = id
        .parse::<ArtifactId>()
        .map_err(|_| not_found("no such artifact"))?;
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

    // Server-side half of "every kind has exactly one render path". Without it a
    // markdown note built from a fetched page could be requested as an app and
    // would suddenly be executable content.
    if !manifest.kind().renders_in_app_sandbox() {
        return Err(not_found("this artifact is not a generated app"));
    }

    let blob = match api
        .blobs
        .open(manifest.sha256(), MAX_SERVED_BLOB_BYTES)
        .await
    {
        Ok(Some(blob)) => blob,
        Ok(None) => {
            tracing::warn!(artifact = %id, "app manifest present but its blob is missing");
            return Err(not_found("app bundle is unavailable"));
        }
        Err(BlobStoreError::IntegrityMismatch) => {
            tracing::error!(artifact = %id, "app bundle failed integrity verification");
            return Err(problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::ArtifactIntegrityFailed,
                "artifact blob failed integrity verification",
                None,
            ));
        }
        Err(BlobStoreError::TooLarge { len, max }) => {
            tracing::error!(artifact = %id, len, max, "app bundle exceeds the served-size cap");
            return Err(problem(
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::ArtifactTooLarge,
                "artifact blob exceeds the served-size limit",
                None,
            ));
        }
        Err(BlobStoreError::Io(e)) => {
            tracing::error!(error = %e, "app bundle read failure");
            return Err(problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "storage unavailable",
                None,
            ));
        }
    };

    let prefix = format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"{APP_DOCUMENT_META_CSP}\">\n"
    );
    let total = blob.len + prefix.len() as u64;
    let body = Body::from_stream(PrefixedChunks {
        prefix: Some(prefix.into_bytes()),
        rest: blob.chunks,
    });

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_owned()),
            (header::CONTENT_LENGTH, total.to_string()),
            (header::ETAG, format!("\"{}\"", manifest.sha256())),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
            (header::CONTENT_SECURITY_POLICY, APP_DOCUMENT_CSP.to_owned()),
            // Content-addressed ⇒ these bytes never change; but an app document
            // is a privileged fetch, so it stays out of any shared cache.
            (header::CACHE_CONTROL, "private, immutable".to_owned()),
        ],
        body,
    )
        .into_response())
}

/// Emit one host-authored prefix, then the verified blob stream. A small
/// hand-rolled stream rather than a combinator crate: prepending a header to a
/// body is not worth a dependency, and keeping the blob stream *behind* the
/// prefix is what guarantees the CSP meta is the first thing parsed.
struct PrefixedChunks {
    prefix: Option<Vec<u8>>,
    rest: jarvis_application::ports::BlobChunks,
}

impl futures_util::Stream for PrefixedChunks {
    type Item = Result<Vec<u8>, BlobStoreError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = std::pin::Pin::into_inner(self);
        if let Some(prefix) = this.prefix.take() {
            return std::task::Poll::Ready(Some(Ok(prefix)));
        }
        std::pin::Pin::new(&mut this.rest).poll_next(cx)
    }
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
        capabilities: m.capabilities().iter().copied().map(Into::into).collect(),
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
