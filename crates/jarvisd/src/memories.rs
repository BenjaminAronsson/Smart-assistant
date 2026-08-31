//! Authenticated memory review/edit/forget surface (FR-16, docs/05 §1).
//!
//! Memory creation is intentionally not exposed as a free-form REST command:
//! explicit-confirmation and future candidate extraction own that decision.
//! This surface lets the owner inspect, edit, and forget an existing item.

use std::sync::Arc;
use std::time::SystemTime;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::{Extension, Json};
use jarvis_application::ports::{MemoryStore, RepositoryError};
use jarvis_contracts::errors::ErrorCode;
use jarvis_contracts::memories::{
    MemoryDto, MemoryLayerDto, MemoryListResponse, MemoryScopeDto, MemorySourceDto,
    PatchMemoryRequest, RetentionDto,
};
use jarvis_domain::ids::MemoryId;
use jarvis_domain::location::Sensitivity;
use jarvis_domain::memory::{Memory, MemoryLayer, MemoryScope, RetentionRule};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::auth::DeviceContext;
use crate::problem::{not_found, problem};
use crate::time::rfc3339;

const MAX_QUERY_BYTES: usize = 128;

#[derive(Clone)]
pub struct MemoryApi {
    store: Arc<dyn MemoryStore>,
}

impl MemoryApi {
    pub fn new(store: Arc<dyn MemoryStore>) -> Self {
        Self { store }
    }
}

#[derive(Debug, Deserialize)]
pub struct MemoryQuery {
    pub layer: Option<MemoryLayerDto>,
    pub query: Option<String>,
    pub limit: Option<u32>,
}

pub fn to_dto(memory: &Memory) -> MemoryDto {
    MemoryDto {
        id: memory.id.as_str().to_owned(),
        layer: layer_dto(memory.layer),
        text: memory.text.clone(),
        source: MemorySourceDto {
            kind: memory.source.kind().to_owned(),
            id: memory.source.id().map(str::to_owned),
        },
        scope: scope_dto(&memory.scope),
        retention: retention_dto(&memory.retention),
        confidence: memory.confidence,
        sensitivity: sensitivity_name(memory.sensitivity).to_owned(),
        pinned: memory.pinned,
        created_at: rfc3339(memory.created_at),
        updated_at: rfc3339(memory.updated_at),
    }
}

fn layer_dto(layer: MemoryLayer) -> MemoryLayerDto {
    match layer {
        MemoryLayer::Working => MemoryLayerDto::Working,
        MemoryLayer::Episodic => MemoryLayerDto::Episodic,
        MemoryLayer::Semantic => MemoryLayerDto::Semantic,
        MemoryLayer::Procedural => MemoryLayerDto::Procedural,
    }
}

fn layer(value: MemoryLayerDto) -> MemoryLayer {
    match value {
        MemoryLayerDto::Working => MemoryLayer::Working,
        MemoryLayerDto::Episodic => MemoryLayer::Episodic,
        MemoryLayerDto::Semantic => MemoryLayer::Semantic,
        MemoryLayerDto::Procedural => MemoryLayer::Procedural,
    }
}

fn scope_dto(scope: &MemoryScope) -> MemoryScopeDto {
    match scope {
        MemoryScope::User => MemoryScopeDto::User,
        MemoryScope::Session(id) => MemoryScopeDto::Session(id.as_str().to_owned()),
        MemoryScope::Project(name) => MemoryScopeDto::Project(name.clone()),
    }
}

fn retention_dto(retention: &RetentionRule) -> RetentionDto {
    match retention {
        RetentionRule::UntilForgotten => RetentionDto::UntilForgotten,
        RetentionRule::ExpiresAt(at) => RetentionDto::ExpiresAt(rfc3339(*at)),
        RetentionRule::Session => RetentionDto::Session,
    }
}

fn sensitivity_name(value: Sensitivity) -> &'static str {
    match value {
        Sensitivity::Normal => "normal",
        Sensitivity::Sensitive => "sensitive",
    }
}

fn parse_retention(value: RetentionDto) -> Result<RetentionRule, &'static str> {
    match value {
        RetentionDto::UntilForgotten => Ok(RetentionRule::UntilForgotten),
        RetentionDto::Session => Ok(RetentionRule::Session),
        RetentionDto::ExpiresAt(raw) => OffsetDateTime::parse(&raw, &Rfc3339)
            .map(SystemTime::from)
            .map(RetentionRule::ExpiresAt)
            .map_err(|_| "retention.expiresAt must be RFC 3339"),
    }
}

fn audit(
    device: &DeviceContext,
    memory: &Memory,
    operation: &str,
) -> jarvis_domain::audit::AuditEvent {
    jarvis_domain::audit::AuditEvent {
        occurred_at: SystemTime::now(),
        actor: format!("device:{}", device.device_id),
        event_type: format!("memory.{operation}"),
        target: format!("memory:{}", memory.id),
        correlation_id: None,
        payload_json: serde_json::json!({"memoryId": memory.id.as_str(), "operation": operation})
            .to_string(),
    }
}

pub async fn list(
    State(api): State<MemoryApi>,
    Extension(device): Extension<DeviceContext>,
    Query(query): Query<MemoryQuery>,
) -> Result<Json<MemoryListResponse>, Response> {
    if let Some(query) = &query.query
        && (query.trim().len() < 2 || query.len() > MAX_QUERY_BYTES)
    {
        return Err(bad_request("memory query must be 2 to 128 bytes"));
    }
    let memories = api
        .store
        .list(
            &device.user_id,
            query.layer.map(layer),
            query.query.as_deref(),
            query.limit.unwrap_or(50),
        )
        .await
        .map_err(repository_problem)?;
    Ok(Json(MemoryListResponse {
        memories: memories.iter().map(to_dto).collect(),
        next_cursor: None,
    }))
}

pub async fn patch(
    State(api): State<MemoryApi>,
    Path(id): Path<String>,
    Extension(device): Extension<DeviceContext>,
    Json(request): Json<PatchMemoryRequest>,
) -> Result<Json<MemoryDto>, Response> {
    let id: MemoryId = id
        .parse()
        .map_err(|_| bad_request("memory id is not a ULID"))?;
    let current = api
        .store
        .get(&device.user_id, &id)
        .await
        .map_err(repository_problem)?
        .ok_or_else(|| not_found("no such memory"))?;
    let retention = request
        .retention
        .map(parse_retention)
        .transpose()
        .map_err(bad_request)?
        .unwrap_or_else(|| current.retention.clone());
    let mut updated = Memory::new(
        current.id.clone(),
        current.user_id.clone(),
        current.layer,
        request.text.unwrap_or_else(|| current.text.clone()),
        current.source.clone(),
        current.scope.clone(),
        retention,
        current.confidence,
        current.sensitivity,
        request.pinned.unwrap_or(current.pinned),
        current.created_at,
    )
    .map_err(memory_problem)?;
    updated.updated_at = SystemTime::now();
    api.store
        .replace(&updated, &audit(&device, &updated, "updated"))
        .await
        .map_err(repository_problem)?;
    Ok(Json(to_dto(&updated)))
}

pub async fn forget(
    State(api): State<MemoryApi>,
    Path(id): Path<String>,
    Extension(device): Extension<DeviceContext>,
) -> Result<StatusCode, Response> {
    let id: MemoryId = id
        .parse()
        .map_err(|_| bad_request("memory id is not a ULID"))?;
    let current = api
        .store
        .get(&device.user_id, &id)
        .await
        .map_err(repository_problem)?
        .ok_or_else(|| not_found("no such memory"))?;
    let removed = api
        .store
        .forget(&device.user_id, &id, &audit(&device, &current, "forgotten"))
        .await
        .map_err(repository_problem)?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found("no such memory"))
    }
}

fn memory_problem(error: jarvis_domain::memory::MemoryError) -> Response {
    let code = match error {
        jarvis_domain::memory::MemoryError::SecretLike => ErrorCode::MemorySecretRejected,
        _ => ErrorCode::MemoryInvalid,
    };
    problem(
        StatusCode::UNPROCESSABLE_ENTITY,
        code,
        "memory content rejected",
        Some(error.to_string()),
    )
}

fn repository_problem(error: RepositoryError) -> Response {
    crate::problem::repository_problem_distinct_idempotency(
        error,
        "memory",
        "memory changed; refresh and retry",
        "idempotency conflict",
        "memory storage unavailable",
    )
}

fn bad_request(detail: &'static str) -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        ErrorCode::ValidationFailed,
        detail,
        None,
    )
}
