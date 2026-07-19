//! Postgres-backed `SessionStore` (docs/04 §3, sqlx-data skill). Repositories
//! return domain types, never rows; the conversation repo touches only the
//! conversation schema — audit/outbox writes go through their own modules,
//! composed here inside one transaction (invariant 6).

use jarvis_application::ports::{RepositoryError, SessionStore};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::conversations::{Session, SessionStatus};
use jarvis_domain::ids::SessionId;
use sqlx::PgPool;
use time::OffsetDateTime;

pub struct PgSessionStore {
    pool: PgPool,
}

impl PgSessionStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl SessionStore for PgSessionStore {
    async fn create(&self, session: &Session, audit: &AuditEvent) -> Result<(), RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;

        sqlx::query!(
            r#"
            INSERT INTO conversation.sessions (id, title, status, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            session.id.as_str(),
            session.title.as_deref(),
            status_str(session.status),
            OffsetDateTime::from(session.created_at),
            OffsetDateTime::from(session.updated_at),
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                RepositoryError::Conflict(format!("session {} already exists", session.id))
            }
            _ => storage(e),
        })?;

        // Same transaction as the domain change (invariant 6): if the audit
        // append fails, the session create rolls back with it.
        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(e.to_string()))?;

        // Outbox in the same transaction (docs/02 §2); the dispatcher (M1)
        // publishes after commit, never before.
        sqlx::query!(
            r#"
            INSERT INTO outbox.outbox_events (event_type, payload)
            VALUES ($1, $2)
            "#,
            "session.created",
            serde_json::json!({ "sessionId": session.id.as_str() }),
        )
        .execute(&mut *tx)
        .await
        .map_err(storage)?;

        tx.commit().await.map_err(storage)
    }

    async fn get(&self, id: &SessionId) -> Result<Option<Session>, RepositoryError> {
        let row = sqlx::query!(
            r#"
            SELECT id, title, status, created_at, updated_at
            FROM conversation.sessions WHERE id = $1
            "#,
            id.as_str(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;

        row.map(|r| map_session(&r.id, r.title, &r.status, r.created_at, r.updated_at))
            .transpose()
    }

    async fn list(&self, limit: u32) -> Result<Vec<Session>, RepositoryError> {
        let rows = sqlx::query!(
            r#"
            SELECT id, title, status, created_at, updated_at
            FROM conversation.sessions
            ORDER BY created_at DESC
            LIMIT $1
            "#,
            i64::from(limit),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;

        rows.into_iter()
            .map(|r| map_session(&r.id, r.title, &r.status, r.created_at, r.updated_at))
            .collect()
    }
}

fn storage(e: sqlx::Error) -> RepositoryError {
    RepositoryError::Storage(e.to_string())
}

fn status_str(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Active => "active",
        SessionStatus::Archived => "archived",
    }
}

fn map_session(
    id: &str,
    title: Option<String>,
    status: &str,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
) -> Result<Session, RepositoryError> {
    let id: SessionId = id
        .parse()
        .map_err(|e| RepositoryError::Storage(format!("stored session id invalid: {e}")))?;
    let status = match status {
        "active" => SessionStatus::Active,
        "archived" => SessionStatus::Archived,
        other => {
            return Err(RepositoryError::Storage(format!(
                "stored session status invalid: {other:?}"
            )));
        }
    };
    Ok(Session {
        id,
        title,
        status,
        created_at: created_at.into(),
        updated_at: updated_at.into(),
    })
}
