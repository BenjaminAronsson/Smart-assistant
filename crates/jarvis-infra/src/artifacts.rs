//! Artifact manifest + provenance persistence (FR-08, docs/04 §4, invariant #6).
//! The infra side of the [`ArtifactStore`] port.
//!
//! A manifest is immutable and a new version is a new row (the DB enforces this
//! via the append-only trigger in migration 0010). `create_version` writes the
//! manifest and its `artifact.created` audit event in ONE transaction: an
//! artifact that cannot be audited is not persisted (invariant #6). The blob
//! bytes live in the CAS ([`crate::artifact_cas::FileBlobStore`]); this table
//! stores only metadata, joined to the blob by `sha256`.

use async_trait::async_trait;
use jarvis_application::ports::{ArtifactStore, RepositoryError};
use jarvis_domain::artifact::{
    ArtifactContent, ArtifactKind, ArtifactManifest, ArtifactSource, ArtifactVersion, BuildNetwork,
    BuildProvenance, Capability, MediaType,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::grants::Sha256;
use jarvis_domain::ids::{ArtifactId, MessageId, RunId};
use jarvis_domain::location::Sensitivity;
use sqlx::PgPool;
use time::OffsetDateTime;

/// Postgres-backed artifact manifest store.
pub struct PgArtifactStore {
    pool: PgPool,
}

impl PgArtifactStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn storage(context: &str) -> impl Fn(sqlx::Error) -> RepositoryError + '_ {
    move |e| {
        // Unique-violation on the (artifact_id, version) PK is a version
        // conflict, not a generic failure — versions are append-only.
        if let Some(db) = e.as_database_error()
            && db.code().as_deref() == Some("23505")
        {
            return RepositoryError::Conflict("artifact version already exists".to_owned());
        }
        RepositoryError::Storage(format!("{context}: {e}"))
    }
}

fn kind_to_str(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::MarkdownHtml => "markdown_html",
        ArtifactKind::CodeText => "code_text",
        ArtifactKind::Image => "image",
        ArtifactKind::Chart => "chart",
        ArtifactKind::Bundle => "bundle",
    }
}

fn kind_from_str(s: &str) -> Result<ArtifactKind, RepositoryError> {
    match s {
        "markdown_html" => Ok(ArtifactKind::MarkdownHtml),
        "code_text" => Ok(ArtifactKind::CodeText),
        "image" => Ok(ArtifactKind::Image),
        "chart" => Ok(ArtifactKind::Chart),
        "bundle" => Ok(ArtifactKind::Bundle),
        other => Err(RepositoryError::Storage(format!(
            "unknown artifact kind {other:?}"
        ))),
    }
}

fn sensitivity_to_str(s: Sensitivity) -> &'static str {
    match s {
        Sensitivity::Normal => "normal",
        Sensitivity::Sensitive => "sensitive",
    }
}

fn sensitivity_from_str(s: &str) -> Result<Sensitivity, RepositoryError> {
    match s {
        "normal" => Ok(Sensitivity::Normal),
        "sensitive" => Ok(Sensitivity::Sensitive),
        other => Err(RepositoryError::Storage(format!(
            "unknown sensitivity {other:?}"
        ))),
    }
}

fn network_to_str(n: BuildNetwork) -> &'static str {
    match n {
        BuildNetwork::Disabled => "disabled",
        BuildNetwork::Enabled => "enabled",
    }
}

fn network_from_str(s: &str) -> Result<BuildNetwork, RepositoryError> {
    match s {
        "disabled" => Ok(BuildNetwork::Disabled),
        "enabled" => Ok(BuildNetwork::Enabled),
        other => Err(RepositoryError::Storage(format!(
            "unknown build network {other:?}"
        ))),
    }
}

/// Map the typed [`ArtifactSource`] list to the stored JSON array
/// (`[{"kind","ref"}, …]`), preserving order.
fn sources_to_json(sources: &[ArtifactSource]) -> serde_json::Value {
    let items: Vec<serde_json::Value> = sources
        .iter()
        .map(|s| match s {
            ArtifactSource::Message(id) => {
                serde_json::json!({ "kind": "message", "ref": id.as_str() })
            }
            ArtifactSource::Run(id) => serde_json::json!({ "kind": "run", "ref": id.as_str() }),
            ArtifactSource::Web { url } => serde_json::json!({ "kind": "web", "ref": url }),
        })
        .collect();
    serde_json::Value::Array(items)
}

fn sources_from_json(value: &serde_json::Value) -> Result<Vec<ArtifactSource>, RepositoryError> {
    let arr = value
        .as_array()
        .ok_or_else(|| RepositoryError::Storage("sources column is not a JSON array".to_owned()))?;
    arr.iter()
        .map(|item| {
            let kind = item.get("kind").and_then(|v| v.as_str());
            let r = item.get("ref").and_then(|v| v.as_str());
            match (kind, r) {
                (Some("message"), Some(r)) => r
                    .parse::<MessageId>()
                    .map(ArtifactSource::Message)
                    .map_err(|e| RepositoryError::Storage(format!("bad message source id: {e}"))),
                (Some("run"), Some(r)) => r
                    .parse::<RunId>()
                    .map(ArtifactSource::Run)
                    .map_err(|e| RepositoryError::Storage(format!("bad run source id: {e}"))),
                (Some("web"), Some(r)) => Ok(ArtifactSource::Web { url: r.to_owned() }),
                _ => Err(RepositoryError::Storage(format!(
                    "malformed artifact source entry: {item}"
                ))),
            }
        })
        .collect()
}

/// The manifest row as stored. Reconstructed back into a domain
/// [`ArtifactManifest`] via [`ArtifactManifest::from_parts`].
struct ManifestRow {
    artifact_id: String,
    version: i64,
    created_by_run: String,
    sha256: String,
    media_type: String,
    kind: String,
    sensitivity: String,
    build_worker_image: Option<String>,
    build_lockfile_hash: Option<String>,
    build_network: String,
    sources: serde_json::Value,
    capabilities: Vec<String>,
}

impl ManifestRow {
    fn into_manifest(self) -> Result<ArtifactManifest, RepositoryError> {
        let id = self
            .artifact_id
            .parse::<ArtifactId>()
            .map_err(|e| RepositoryError::Storage(format!("bad artifact_id: {e}")))?;
        let version = u32::try_from(self.version)
            .ok()
            .and_then(ArtifactVersion::new)
            .ok_or_else(|| RepositoryError::Storage(format!("bad version {}", self.version)))?;
        let created_by_run = self
            .created_by_run
            .parse::<RunId>()
            .map_err(|e| RepositoryError::Storage(format!("bad created_by_run: {e}")))?;
        let sha256 = self
            .sha256
            .parse::<Sha256>()
            .map_err(|e| RepositoryError::Storage(format!("bad sha256: {e}")))?;
        let media_type = self
            .media_type
            .parse::<MediaType>()
            .map_err(|e| RepositoryError::Storage(format!("bad media_type: {e}")))?;
        let lockfile_hash = match self.build_lockfile_hash {
            Some(h) => Some(
                h.parse::<Sha256>()
                    .map_err(|e| RepositoryError::Storage(format!("bad lockfile hash: {e}")))?,
            ),
            None => None,
        };
        let capabilities = self
            .capabilities
            .iter()
            .map(|c| {
                c.parse::<Capability>()
                    .map_err(|e| RepositoryError::Storage(format!("bad capability: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let content = ArtifactContent {
            sha256,
            media_type,
            kind: kind_from_str(&self.kind)?,
            sources: sources_from_json(&self.sources)?,
            sensitivity: sensitivity_from_str(&self.sensitivity)?,
            build: BuildProvenance {
                worker_image: self.build_worker_image,
                lockfile_hash,
                network: network_from_str(&self.build_network)?,
            },
            capabilities,
        };
        Ok(ArtifactManifest::from_parts(
            id,
            version,
            created_by_run,
            content,
        ))
    }
}

#[async_trait]
impl ArtifactStore for PgArtifactStore {
    async fn create_version(
        &self,
        manifest: &ArtifactManifest,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        let build = manifest.build();
        let version = i64::from(manifest.version().get());
        let sources = sources_to_json(manifest.sources());
        let capabilities: Vec<String> = manifest
            .capabilities()
            .iter()
            .map(|c| c.as_str().to_owned())
            .collect();
        let lockfile_hash = build.lockfile_hash.as_ref().map(|h| h.to_string());

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(storage("artifact create: begin"))?;
        sqlx::query!(
            r#"
            INSERT INTO artifacts.manifests
                (artifact_id, version, created_by_run, sha256, media_type, kind,
                 sensitivity, build_worker_image, build_lockfile_hash,
                 build_network, sources, capabilities, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            "#,
            manifest.id().as_str(),
            version,
            manifest.created_by_run().as_str(),
            manifest.sha256().to_string(),
            manifest.media_type().as_str(),
            kind_to_str(manifest.kind()),
            sensitivity_to_str(manifest.sensitivity()),
            build.worker_image.as_deref(),
            lockfile_hash.as_deref(),
            network_to_str(build.network),
            sources,
            &capabilities,
            OffsetDateTime::now_utc(),
        )
        .execute(&mut *tx)
        .await
        .map_err(storage("artifact create: insert"))?;

        // Same transaction as the manifest: no manifest without its audit trail
        // (invariant #6). A rollback here leaves neither.
        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("artifact create: audit: {e}")))?;

        tx.commit()
            .await
            .map_err(storage("artifact create: commit"))?;
        Ok(())
    }

    async fn get(
        &self,
        id: &ArtifactId,
        version: ArtifactVersion,
    ) -> Result<Option<ArtifactManifest>, RepositoryError> {
        let row = sqlx::query_as!(
            ManifestRow,
            r#"
            SELECT artifact_id, version, created_by_run, sha256, media_type, kind,
                   sensitivity, build_worker_image, build_lockfile_hash, build_network,
                   sources, capabilities
            FROM artifacts.manifests
            WHERE artifact_id = $1 AND version = $2
            "#,
            id.as_str(),
            i64::from(version.get()),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(storage("artifact get"))?;
        row.map(ManifestRow::into_manifest).transpose()
    }

    async fn latest(&self, id: &ArtifactId) -> Result<Option<ArtifactManifest>, RepositoryError> {
        let row = sqlx::query_as!(
            ManifestRow,
            r#"
            SELECT artifact_id, version, created_by_run, sha256, media_type, kind,
                   sensitivity, build_worker_image, build_lockfile_hash, build_network,
                   sources, capabilities
            FROM artifacts.manifests
            WHERE artifact_id = $1
            ORDER BY version DESC
            LIMIT 1
            "#,
            id.as_str(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(storage("artifact latest"))?;
        row.map(ManifestRow::into_manifest).transpose()
    }

    async fn list_versions(
        &self,
        id: &ArtifactId,
    ) -> Result<Vec<ArtifactManifest>, RepositoryError> {
        let rows = sqlx::query_as!(
            ManifestRow,
            r#"
            SELECT artifact_id, version, created_by_run, sha256, media_type, kind,
                   sensitivity, build_worker_image, build_lockfile_hash, build_network,
                   sources, capabilities
            FROM artifacts.manifests
            WHERE artifact_id = $1
            ORDER BY version ASC
            "#,
            id.as_str(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage("artifact list_versions"))?;
        rows.into_iter().map(ManifestRow::into_manifest).collect()
    }
}
