//! PostgreSQL memory persistence (FR-16, migration 0013).
//!
//! This adapter intentionally uses dynamic SQL for the pgvector-bearing
//! schema: SQLx has no pgvector type in the workspace and the first memory
//! slice does not yet execute vector searches. Reads still strictly rebuild
//! domain values, and every mutation writes the append-only audit row in the
//! same transaction.

use async_trait::async_trait;
use jarvis_application::ports::{
    EmbeddedMemory, EmbeddedMemoryStore, MemoryContextStore, MemoryContextUse, MemoryHit,
    MemoryRetriever, MemoryStore, RepositoryError,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::{MemoryId, MessageId, RunId, UserId};
use jarvis_domain::location::Sensitivity;
use jarvis_domain::memory::{Memory, MemoryLayer, MemoryScope, MemorySource, RetentionRule};
use sqlx::{PgPool, Row};
use std::time::SystemTime;
use time::OffsetDateTime;

pub struct PgMemoryStore {
    pool: PgPool,
}

impl PgMemoryStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn storage(context: &'static str, error: sqlx::Error) -> RepositoryError {
    if let Some(db) = error.as_database_error()
        && db.code().as_deref() == Some("23505")
    {
        return RepositoryError::Conflict("memory already exists".to_owned());
    }
    RepositoryError::Storage(format!("{context}: {error}"))
}

fn utc(t: SystemTime) -> OffsetDateTime {
    OffsetDateTime::from(t)
}

fn layer_name(layer: MemoryLayer) -> &'static str {
    match layer {
        MemoryLayer::Working => "working",
        MemoryLayer::Episodic => "episodic",
        MemoryLayer::Semantic => "semantic",
        MemoryLayer::Procedural => "procedural",
    }
}

fn layer(raw: &str) -> Option<MemoryLayer> {
    match raw {
        "working" => Some(MemoryLayer::Working),
        "episodic" => Some(MemoryLayer::Episodic),
        "semantic" => Some(MemoryLayer::Semantic),
        "procedural" => Some(MemoryLayer::Procedural),
        _ => None,
    }
}

fn sensitivity_name(value: Sensitivity) -> &'static str {
    match value {
        Sensitivity::Normal => "normal",
        Sensitivity::Sensitive => "sensitive",
    }
}

fn sensitivity(raw: &str) -> Option<Sensitivity> {
    match raw {
        "normal" => Some(Sensitivity::Normal),
        "sensitive" => Some(Sensitivity::Sensitive),
        _ => None,
    }
}

fn source_values(source: &MemorySource) -> (&'static str, Option<&str>) {
    (source.kind(), source.id())
}

fn scope_values(scope: &MemoryScope) -> (&'static str, Option<&str>) {
    match scope {
        MemoryScope::User => ("user", None),
        MemoryScope::Session(id) => ("session", Some(id.as_str())),
        MemoryScope::Project(name) => ("project", Some(name.as_str())),
    }
}

fn retention_values(retention: &RetentionRule) -> (&'static str, Option<OffsetDateTime>) {
    match retention {
        RetentionRule::UntilForgotten => ("until_forgotten", None),
        RetentionRule::ExpiresAt(at) => ("expires_at", Some(utc(*at))),
        RetentionRule::Session => ("session", None),
    }
}

fn rebuild(row: &sqlx::postgres::PgRow) -> Result<Memory, RepositoryError> {
    let id = row
        .try_get::<String, _>("id")
        .map_err(|e| storage("memory id", e))?
        .parse::<MemoryId>()
        .map_err(|e| RepositoryError::Storage(format!("bad memory id: {e}")))?;
    let user_id = row
        .try_get::<String, _>("user_id")
        .map_err(|e| storage("memory user id", e))?
        .parse::<UserId>()
        .map_err(|e| RepositoryError::Storage(format!("bad memory user id: {e}")))?;
    let layer_raw = row
        .try_get::<String, _>("layer")
        .map_err(|e| storage("memory layer", e))?;
    let layer = layer(&layer_raw)
        .ok_or_else(|| RepositoryError::Storage(format!("unknown memory layer {layer_raw:?}")))?;
    let text = row
        .try_get::<String, _>("text")
        .map_err(|e| storage("memory text", e))?;
    let source_kind = row
        .try_get::<String, _>("source_kind")
        .map_err(|e| storage("memory source kind", e))?;
    let source_id = row
        .try_get::<Option<String>, _>("source_id")
        .map_err(|e| storage("memory source id", e))?;
    let source = match (source_kind.as_str(), source_id) {
        ("explicit", None) => MemorySource::Explicit,
        ("message", Some(raw)) => MemorySource::Message(
            raw.parse::<MessageId>()
                .map_err(|e| RepositoryError::Storage(format!("bad memory message source: {e}")))?,
        ),
        ("run", Some(raw)) => MemorySource::Run(
            raw.parse::<RunId>()
                .map_err(|e| RepositoryError::Storage(format!("bad memory run source: {e}")))?,
        ),
        _ => return Err(RepositoryError::Storage("invalid memory source".to_owned())),
    };
    let scope_kind = row
        .try_get::<String, _>("scope_kind")
        .map_err(|e| storage("memory scope kind", e))?;
    let scope_value = row
        .try_get::<Option<String>, _>("scope_value")
        .map_err(|e| storage("memory scope value", e))?;
    let scope = match (scope_kind.as_str(), scope_value) {
        ("user", None) => MemoryScope::User,
        ("session", Some(raw)) => MemoryScope::Session(
            raw.parse()
                .map_err(|e| RepositoryError::Storage(format!("bad memory session scope: {e}")))?,
        ),
        ("project", Some(raw)) => MemoryScope::Project(raw),
        _ => return Err(RepositoryError::Storage("invalid memory scope".to_owned())),
    };
    let retention_kind = row
        .try_get::<String, _>("retention_kind")
        .map_err(|e| storage("memory retention kind", e))?;
    let expires_at = row
        .try_get::<Option<OffsetDateTime>, _>("expires_at")
        .map_err(|e| storage("memory expiry", e))?;
    let retention = match (retention_kind.as_str(), expires_at) {
        ("until_forgotten", None) => RetentionRule::UntilForgotten,
        ("expires_at", Some(at)) => RetentionRule::ExpiresAt(at.into()),
        ("session", None) => RetentionRule::Session,
        _ => {
            return Err(RepositoryError::Storage(
                "invalid memory retention".to_owned(),
            ));
        }
    };
    let confidence = row
        .try_get::<f32, _>("confidence")
        .map_err(|e| storage("memory confidence", e))?;
    let sensitivity_raw = row
        .try_get::<String, _>("sensitivity")
        .map_err(|e| storage("memory sensitivity", e))?;
    let sensitivity = sensitivity(&sensitivity_raw)
        .ok_or_else(|| RepositoryError::Storage("invalid memory sensitivity".to_owned()))?;
    let pinned = row
        .try_get::<bool, _>("pinned")
        .map_err(|e| storage("memory pinned", e))?;
    let created_at: SystemTime = row
        .try_get::<OffsetDateTime, _>("created_at")
        .map_err(|e| storage("memory created_at", e))?
        .into();
    let updated_at: SystemTime = row
        .try_get::<OffsetDateTime, _>("updated_at")
        .map_err(|e| storage("memory updated_at", e))?
        .into();
    let mut memory = Memory::new(
        id,
        user_id,
        layer,
        text,
        source,
        scope,
        retention,
        confidence,
        sensitivity,
        pinned,
        created_at,
    )
    .map_err(|e| RepositoryError::Storage(format!("invalid stored memory: {e}")))?;
    memory.updated_at = updated_at;
    Ok(memory)
}

const SELECT: &str = "SELECT m.id, m.user_id, m.layer, m.text, m.confidence, m.sensitivity, m.scope_kind, m.scope_value, m.retention_kind, m.expires_at, m.pinned, m.created_at, m.updated_at, s.source_kind, s.source_id FROM memory.memories m JOIN memory.memory_sources s ON s.memory_id = m.id";

#[async_trait]
impl MemoryStore for PgMemoryStore {
    async fn create(&self, memory: &Memory, audit: &AuditEvent) -> Result<(), RepositoryError> {
        let (scope_kind, scope_value) = scope_values(&memory.scope);
        let (retention_kind, expires_at) = retention_values(&memory.retention);
        let (source_kind, source_id) = source_values(&memory.source);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| storage("memory create begin", e))?;
        sqlx::query("INSERT INTO memory.memories (id,user_id,layer,text,confidence,sensitivity,scope_kind,scope_value,retention_kind,expires_at,pinned,created_at,updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$12)")
            .bind(memory.id.as_str()).bind(memory.user_id.as_str()).bind(layer_name(memory.layer)).bind(&memory.text)
            .bind(memory.confidence).bind(sensitivity_name(memory.sensitivity)).bind(scope_kind).bind(scope_value)
            .bind(retention_kind).bind(expires_at).bind(memory.pinned).bind(utc(memory.created_at)).execute(&mut *tx).await
            .map_err(|e| storage("memory create insert", e))?;
        sqlx::query(
            "INSERT INTO memory.memory_sources (memory_id,source_kind,source_id) VALUES ($1,$2,$3)",
        )
        .bind(memory.id.as_str())
        .bind(source_kind)
        .bind(source_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| storage("memory create source", e))?;
        if let Some(expires_at) = expires_at {
            sqlx::query("INSERT INTO memory.retention_jobs (memory_id,expires_at) VALUES ($1,$2)")
                .bind(memory.id.as_str())
                .bind(expires_at)
                .execute(&mut *tx)
                .await
                .map_err(|e| storage("memory create retention", e))?;
        }
        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("memory create audit: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| storage("memory create commit", e))
    }

    async fn get(
        &self,
        user_id: &UserId,
        id: &MemoryId,
    ) -> Result<Option<Memory>, RepositoryError> {
        let row = sqlx::query(&format!("{SELECT} WHERE m.user_id = $1 AND m.id = $2 AND (m.expires_at IS NULL OR m.expires_at > now())"))
            .bind(user_id.as_str())
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| storage("memory get", e))?;
        row.as_ref().map(rebuild).transpose()
    }

    async fn list(
        &self,
        user_id: &UserId,
        selected_layer: Option<MemoryLayer>,
        query: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Memory>, RepositoryError> {
        let mut sql = format!(
            "{SELECT} WHERE m.user_id = $1 AND (m.expires_at IS NULL OR m.expires_at > now())"
        );
        let mut next = 2;
        if selected_layer.is_some() {
            sql.push_str(&format!(" AND m.layer = ${next}"));
            next += 1;
        }
        if query.is_some() {
            sql.push_str(&format!(" AND m.text ILIKE ${next} ESCAPE '\\'"));
            next += 1;
        }
        sql.push_str(&format!(
            " ORDER BY m.updated_at DESC, m.id DESC LIMIT ${next}"
        ));
        let mut request = sqlx::query(&sql).bind(user_id.as_str());
        if let Some(value) = selected_layer {
            request = request.bind(layer_name(value));
        }
        if let Some(value) = query {
            let escaped = value
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            request = request.bind(format!("%{escaped}%"));
        }
        let limit = i64::from(limit.clamp(1, 100));
        request = request.bind(limit);
        let rows = request
            .fetch_all(&self.pool)
            .await
            .map_err(|e| storage("memory list", e))?;
        rows.iter().map(rebuild).collect()
    }

    async fn replace(&self, memory: &Memory, audit: &AuditEvent) -> Result<(), RepositoryError> {
        let (scope_kind, scope_value) = scope_values(&memory.scope);
        let (retention_kind, expires_at) = retention_values(&memory.retention);
        let (source_kind, source_id) = source_values(&memory.source);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| storage("memory replace begin", e))?;
        let changed = sqlx::query("UPDATE memory.memories SET layer=$1,text=$2,confidence=$3,sensitivity=$4,scope_kind=$5,scope_value=$6,retention_kind=$7,expires_at=$8,pinned=$9,updated_at=$10 WHERE id=$11 AND user_id=$12")
            .bind(layer_name(memory.layer)).bind(&memory.text).bind(memory.confidence).bind(sensitivity_name(memory.sensitivity)).bind(scope_kind).bind(scope_value).bind(retention_kind).bind(expires_at).bind(memory.pinned).bind(utc(memory.updated_at)).bind(memory.id.as_str()).bind(memory.user_id.as_str()).execute(&mut *tx).await.map_err(|e| storage("memory replace update", e))?;
        if changed.rows_affected() == 0 {
            return Err(RepositoryError::Conflict(
                "memory not found or changed".to_owned(),
            ));
        }
        sqlx::query("DELETE FROM memory.memory_sources WHERE memory_id = $1")
            .bind(memory.id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|e| storage("memory replace source delete", e))?;
        sqlx::query(
            "INSERT INTO memory.memory_sources (memory_id,source_kind,source_id) VALUES ($1,$2,$3)",
        )
        .bind(memory.id.as_str())
        .bind(source_kind)
        .bind(source_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| storage("memory replace source", e))?;
        sqlx::query("DELETE FROM memory.embeddings WHERE memory_id = $1")
            .bind(memory.id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|e| storage("memory replace embedding", e))?;
        sqlx::query("DELETE FROM memory.retention_jobs WHERE memory_id = $1")
            .bind(memory.id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|e| storage("memory replace retention delete", e))?;
        if let Some(expires_at) = expires_at {
            sqlx::query("INSERT INTO memory.retention_jobs (memory_id,expires_at) VALUES ($1,$2)")
                .bind(memory.id.as_str())
                .bind(expires_at)
                .execute(&mut *tx)
                .await
                .map_err(|e| storage("memory replace retention", e))?;
        }
        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("memory replace audit: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| storage("memory replace commit", e))
    }

    async fn forget(
        &self,
        user_id: &UserId,
        id: &MemoryId,
        audit: &AuditEvent,
    ) -> Result<bool, RepositoryError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| storage("memory forget begin", e))?;
        let deleted = sqlx::query("DELETE FROM memory.memories WHERE id = $1 AND user_id = $2")
            .bind(id.as_str())
            .bind(user_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|e| storage("memory forget delete", e))?;
        if deleted.rows_affected() == 0 {
            tx.rollback()
                .await
                .map_err(|e| storage("memory forget rollback", e))?;
            return Ok(false);
        }
        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("memory forget audit: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| storage("memory forget commit", e))?;
        Ok(true)
    }
}

fn vector_literal(values: &[f32]) -> Result<String, RepositoryError> {
    if values.len() != 384 || values.iter().any(|value| !value.is_finite()) {
        return Err(RepositoryError::Storage(
            "invalid embedding dimensions".to_owned(),
        ));
    }
    Ok(format!(
        "[{}]",
        values
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(",")
    ))
}

fn stored_embedding(value: &EmbeddedMemory) -> Result<String, RepositoryError> {
    if value.dimensions != value.embedding.len()
        || value.dimensions != 384
        || value.model_id.trim().is_empty()
        || value
            .embedding
            .iter()
            .any(|component| !component.is_finite())
    {
        return Err(RepositoryError::Storage(
            "invalid embedding dimensions or model".to_owned(),
        ));
    }
    Ok(format!(
        "[{}]",
        value
            .embedding
            .iter()
            .map(|component| component.to_string())
            .collect::<Vec<_>>()
            .join(",")
    ))
}

fn text_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(text.as_bytes()))
}

#[async_trait]
impl EmbeddedMemoryStore for PgMemoryStore {
    async fn create_embedded(
        &self,
        memory: &Memory,
        embedding: &EmbeddedMemory,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        let vector = stored_embedding(embedding)?;
        let (scope_kind, scope_value) = scope_values(&memory.scope);
        let (retention_kind, expires_at) = retention_values(&memory.retention);
        let (source_kind, source_id) = source_values(&memory.source);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| storage("memory create embedded begin", e))?;
        sqlx::query("INSERT INTO memory.memories (id,user_id,layer,text,confidence,sensitivity,scope_kind,scope_value,retention_kind,expires_at,pinned,created_at,updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$12)")
            .bind(memory.id.as_str()).bind(memory.user_id.as_str()).bind(layer_name(memory.layer)).bind(&memory.text)
            .bind(memory.confidence).bind(sensitivity_name(memory.sensitivity)).bind(scope_kind).bind(scope_value)
            .bind(retention_kind).bind(expires_at).bind(memory.pinned).bind(utc(memory.created_at)).execute(&mut *tx).await
            .map_err(|e| storage("memory create embedded insert", e))?;
        sqlx::query(
            "INSERT INTO memory.memory_sources (memory_id,source_kind,source_id) VALUES ($1,$2,$3)",
        )
        .bind(memory.id.as_str())
        .bind(source_kind)
        .bind(source_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| storage("memory create embedded source", e))?;
        sqlx::query("INSERT INTO memory.embeddings (memory_id,model_id,dimensions,text_sha256,embedding,created_at) VALUES ($1,$2,$3,$4,$5::vector,$6)")
            .bind(memory.id.as_str()).bind(&embedding.model_id).bind(embedding.dimensions as i32)
            .bind(text_hash(&memory.text)).bind(vector).bind(utc(memory.updated_at)).execute(&mut *tx).await
            .map_err(|e| storage("memory create embedded vector", e))?;
        if let Some(expires_at) = expires_at {
            sqlx::query("INSERT INTO memory.retention_jobs (memory_id,expires_at) VALUES ($1,$2)")
                .bind(memory.id.as_str())
                .bind(expires_at)
                .execute(&mut *tx)
                .await
                .map_err(|e| storage("memory create embedded retention", e))?;
        }
        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("memory create embedded audit: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| storage("memory create embedded commit", e))
    }

    async fn replace_embedded(
        &self,
        memory: &Memory,
        embedding: &EmbeddedMemory,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        let vector = stored_embedding(embedding)?;
        let (scope_kind, scope_value) = scope_values(&memory.scope);
        let (retention_kind, expires_at) = retention_values(&memory.retention);
        let (source_kind, source_id) = source_values(&memory.source);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| storage("memory replace embedded begin", e))?;
        let changed = sqlx::query("UPDATE memory.memories SET layer=$1,text=$2,confidence=$3,sensitivity=$4,scope_kind=$5,scope_value=$6,retention_kind=$7,expires_at=$8,pinned=$9,updated_at=$10 WHERE id=$11 AND user_id=$12")
            .bind(layer_name(memory.layer)).bind(&memory.text).bind(memory.confidence).bind(sensitivity_name(memory.sensitivity)).bind(scope_kind).bind(scope_value).bind(retention_kind).bind(expires_at).bind(memory.pinned).bind(utc(memory.updated_at)).bind(memory.id.as_str()).bind(memory.user_id.as_str()).execute(&mut *tx).await.map_err(|e| storage("memory replace embedded update", e))?;
        if changed.rows_affected() == 0 {
            return Err(RepositoryError::Conflict(
                "memory not found or changed".to_owned(),
            ));
        }
        sqlx::query("DELETE FROM memory.memory_sources WHERE memory_id=$1")
            .bind(memory.id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|e| storage("memory replace embedded source delete", e))?;
        sqlx::query(
            "INSERT INTO memory.memory_sources (memory_id,source_kind,source_id) VALUES ($1,$2,$3)",
        )
        .bind(memory.id.as_str())
        .bind(source_kind)
        .bind(source_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| storage("memory replace embedded source", e))?;
        sqlx::query("INSERT INTO memory.embeddings (memory_id,model_id,dimensions,text_sha256,embedding,created_at) VALUES ($1,$2,$3,$4,$5::vector,$6) ON CONFLICT (memory_id) DO UPDATE SET model_id=EXCLUDED.model_id,dimensions=EXCLUDED.dimensions,text_sha256=EXCLUDED.text_sha256,embedding=EXCLUDED.embedding,created_at=EXCLUDED.created_at")
            .bind(memory.id.as_str()).bind(&embedding.model_id).bind(embedding.dimensions as i32).bind(text_hash(&memory.text)).bind(vector).bind(utc(memory.updated_at)).execute(&mut *tx).await.map_err(|e| storage("memory replace embedded vector", e))?;
        sqlx::query("DELETE FROM memory.retention_jobs WHERE memory_id=$1")
            .bind(memory.id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|e| storage("memory replace embedded retention delete", e))?;
        if let Some(expires_at) = expires_at {
            sqlx::query("INSERT INTO memory.retention_jobs (memory_id,expires_at) VALUES ($1,$2)")
                .bind(memory.id.as_str())
                .bind(expires_at)
                .execute(&mut *tx)
                .await
                .map_err(|e| storage("memory replace embedded retention", e))?;
        }
        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("memory replace embedded audit: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| storage("memory replace embedded commit", e))
    }
}

#[async_trait]
impl MemoryContextStore for PgMemoryStore {
    async fn record_context(
        &self,
        user_id: &UserId,
        uses: &[MemoryContextUse],
    ) -> Result<(), RepositoryError> {
        // The application bounds retrieval at eight results; keep this port
        // defensive because it may also be called by a future assembler.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| storage("memory context begin", e))?;
        for use_ in uses.iter().take(8) {
            if !use_.similarity.is_finite() || !(0..=8).contains(&use_.rank) {
                return Err(RepositoryError::Storage(
                    "invalid memory context rank or similarity".to_owned(),
                ));
            }
            sqlx::query("INSERT INTO memory.context_provenance (user_id,run_id,memory_id,rank,similarity,used_at) SELECT $1,$2,$3,$4,$5,$6 WHERE EXISTS (SELECT 1 FROM memory.memories WHERE id=$3 AND user_id=$1) ON CONFLICT (run_id,memory_id) DO NOTHING")
                .bind(user_id.as_str())
                .bind(use_.run_id.as_str())
                .bind(use_.memory_id.as_str())
                .bind(use_.rank)
                .bind(use_.similarity)
                .bind(utc(use_.used_at))
                .execute(&mut *tx)
                .await
                .map_err(|e| storage("memory context record", e))?;
        }
        tx.commit()
            .await
            .map_err(|e| storage("memory context commit", e))
    }
}

#[async_trait]
impl MemoryRetriever for PgMemoryStore {
    async fn retrieve(
        &self,
        user_id: &UserId,
        selected_layer: Option<MemoryLayer>,
        embedding: &[f32],
        limit: u32,
    ) -> Result<Vec<MemoryHit>, RepositoryError> {
        let vector = vector_literal(embedding)?;
        let mut sql = "SELECT m.id, m.user_id, m.layer, m.text, m.confidence, m.sensitivity, m.scope_kind, m.scope_value, m.retention_kind, m.expires_at, m.pinned, m.created_at, m.updated_at, s.source_kind, s.source_id, 1 - (e.embedding <=> $1::vector) AS similarity FROM memory.memories m JOIN memory.memory_sources s ON s.memory_id = m.id JOIN memory.embeddings e ON e.memory_id = m.id WHERE m.user_id = $2 AND (m.expires_at IS NULL OR m.expires_at > now())".to_owned();
        if selected_layer.is_some() {
            sql.push_str(" AND m.layer = $3");
        }
        let limit_position = if selected_layer.is_some() { 4 } else { 3 };
        sql.push_str(&format!(
            " ORDER BY e.embedding <=> $1::vector, m.id ASC LIMIT ${limit_position}"
        ));
        let mut request = sqlx::query(&sql).bind(vector).bind(user_id.as_str());
        if let Some(value) = selected_layer {
            request = request.bind(layer_name(value));
        }
        request = request.bind(i64::from(limit.clamp(1, 20)));
        let rows = request
            .fetch_all(&self.pool)
            .await
            .map_err(|e| storage("memory retrieve", e))?;
        rows.iter()
            .map(|row| {
                let similarity = row
                    .try_get::<f64, _>("similarity")
                    .map_err(|e| storage("memory similarity", e))?;
                if !similarity.is_finite() {
                    return Err(RepositoryError::Storage(
                        "invalid memory similarity".to_owned(),
                    ));
                }
                Ok(MemoryHit {
                    memory: rebuild(row)?,
                    similarity: similarity as f32,
                })
            })
            .collect()
    }
}
