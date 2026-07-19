//! Session REST surface (docs/05 §1, FR-02): create / get / list, mounted
//! behind the bearer middleware. Wire DTOs only at this boundary; domain
//! types inside.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::{Extension, Json};
use jarvis_application::ports::{CreateOutcome, RepositoryError, SessionStore};
use jarvis_contracts::errors::ErrorCode;
use jarvis_contracts::sessions::{
    CreateSessionRequest, SessionDto, SessionListResponse, SessionStatus as WireStatus,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::conversations::{Session, SessionStatus};
use jarvis_domain::ids::SessionId;
use serde::Deserialize;
use std::sync::Arc;
use std::time::SystemTime;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::auth::DeviceContext;
use crate::problem::problem;

#[derive(Clone)]
pub struct SessionApi {
    store: Arc<dyn SessionStore>,
}

impl SessionApi {
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self { store }
    }
}

pub async fn create(
    State(api): State<SessionApi>,
    Extension(device): Extension<DeviceContext>,
    headers: HeaderMap,
    Json(request): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<SessionDto>), Response> {
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Truncate to timestamptz precision so the create response and every
    // later fetch render the identical RFC 3339 string.
    let now = truncate_to_micros(SystemTime::now());
    let session = Session::new(
        crate::auth::fresh_id::<SessionId>(),
        request.title.clone(),
        now,
    );
    let audit = AuditEvent {
        occurred_at: now,
        actor: format!("device:{}", device.device_id),
        event_type: "session.created".into(),
        target: format!("session:{}", session.id),
        correlation_id: current_trace_id(),
        payload_json: format!(
            r#"{{"title":{}}}"#,
            serde_json::to_string(&session.title).expect("option string serializes"),
        ),
    };

    match api
        .store
        .create(&session, idempotency_key.as_deref(), &audit)
        .await
    {
        Ok(CreateOutcome::Created(s)) => Ok((StatusCode::CREATED, Json(to_dto(&s)))),
        // Safe replay: same key, same payload — return the original (200).
        Ok(CreateOutcome::AlreadyExists(s)) => Ok((StatusCode::OK, Json(to_dto(&s)))),
        Err(e) => Err(repository_problem(e)),
    }
}

pub async fn get(
    State(api): State<SessionApi>,
    Path(id): Path<String>,
) -> Result<Json<SessionDto>, Response> {
    let id: SessionId = id.parse().map_err(|_| {
        problem(
            StatusCode::NOT_FOUND,
            ErrorCode::ResourceNotFound,
            "no such session",
            None,
        )
    })?;
    match api.store.get(&id).await {
        Ok(Some(session)) => Ok(Json(to_dto(&session))),
        Ok(None) => Err(problem(
            StatusCode::NOT_FOUND,
            ErrorCode::ResourceNotFound,
            "no such session",
            None,
        )),
        Err(e) => Err(repository_problem(e)),
    }
}

#[derive(Deserialize)]
pub struct ListParams {
    limit: Option<u32>,
}

pub async fn list(
    State(api): State<SessionApi>,
    Query(params): Query<ListParams>,
) -> Result<Json<SessionListResponse>, Response> {
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    match api.store.list(limit).await {
        Ok(sessions) => Ok(Json(SessionListResponse {
            sessions: sessions.iter().map(to_dto).collect(),
            // Cursor pagination lands with session search (M1+).
            next_cursor: None,
        })),
        Err(e) => Err(repository_problem(e)),
    }
}

/// One mapping for every RepositoryError crossing the boundary (docs/05 §7).
/// Storage details never reach the client — they can carry driver internals.
fn repository_problem(error: RepositoryError) -> Response {
    match error {
        RepositoryError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            ErrorCode::IdempotencyConflict,
            "idempotency key reused with a different payload",
            None,
        ),
        RepositoryError::Conflict(_) => problem(
            StatusCode::CONFLICT,
            ErrorCode::ResourceVersionConflict,
            "resource conflict",
            None,
        ),
        RepositoryError::Storage(e) => {
            tracing::error!(error = %e, "session storage failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "storage unavailable",
                None,
            )
        }
    }
}

fn to_dto(session: &Session) -> SessionDto {
    SessionDto {
        id: session.id.clone(),
        title: session.title.clone(),
        status: match session.status {
            SessionStatus::Active => WireStatus::Active,
            SessionStatus::Archived => WireStatus::Archived,
        },
        created_at: rfc3339(session.created_at),
        updated_at: rfc3339(session.updated_at),
    }
}

fn truncate_to_micros(t: SystemTime) -> SystemTime {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => std::time::UNIX_EPOCH + std::time::Duration::from_micros(d.as_micros() as u64),
        Err(_) => t, // pre-epoch clock: leave untouched, storage will reject
    }
}

fn rfc3339(t: SystemTime) -> String {
    OffsetDateTime::from(t)
        .format(&Rfc3339)
        .expect("UTC timestamp formats")
}

fn current_trace_id() -> Option<String> {
    // Correlation via the active tracing span's OTel trace id arrives with
    // the run pipeline (M1); session creates carry the actor for now.
    None
}
