//! Read side of the transactional outbox for the WS hub and timeline resync
//! (docs/05 §3). The outbox IS the persisted event log: `message.created` and
//! every `run.*` domain event live there in commit order under a global,
//! monotonic `id`. That id is the resync cursor — the WS envelope `seq` and the
//! timeline `since` share it (rows are retained after dispatch, so a
//! reconnecting client replays from any past cursor).
//!
//! Reads return raw rows ([`OutboxRecord`]: id + type + payload); the mapping to
//! the wire `DomainEvent`/`TimelineItem` happens in jarvisd, which owns the
//! contracts — infra must not depend on `jarvis-contracts` (arch-test).

use crate::dispatcher::OutboxRecord;
use jarvis_application::ports::RepositoryError;
use sqlx::PgPool;

pub struct PgEventLog {
    pool: PgPool,
}

impl PgEventLog {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Every committed event with `id > since`, oldest first, capped at `limit`.
    /// The full owner stream across all sessions — the WS `?since=` reconnect
    /// replay (single-owner v1: one connection sees everything).
    pub async fn since(
        &self,
        since: i64,
        limit: i64,
    ) -> Result<Vec<OutboxRecord>, RepositoryError> {
        let rows = sqlx::query!(
            r#"
            SELECT id, event_type, payload, created_at
            FROM outbox.outbox_events
            WHERE id > $1
            ORDER BY id ASC
            LIMIT $2
            "#,
            since,
            limit,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;

        Ok(rows
            .into_iter()
            .map(|r| OutboxRecord {
                id: r.id,
                event_type: r.event_type,
                payload: r.payload,
                created_at: r.created_at,
            })
            .collect())
    }

    /// The persisted timeline for one session with `id > since`, oldest first.
    /// The session is resolved from whichever locator a payload carries:
    /// `run.started` has a top-level `sessionId`; `message.created` nests it
    /// under `message`; the other `run.*` rows carry only `runId`, so they are
    /// scoped through the runs table (docs/05 §2-§3 payload shapes).
    pub async fn timeline(
        &self,
        session_id: &str,
        since: i64,
        limit: i64,
    ) -> Result<Vec<OutboxRecord>, RepositoryError> {
        let rows = sqlx::query!(
            r#"
            SELECT o.id, o.event_type, o.payload, o.created_at
            FROM outbox.outbox_events o
            LEFT JOIN orchestration.runs r ON o.payload ->> 'runId' = r.id
            WHERE o.id > $1
              AND (
                  $2 = o.payload ->> 'sessionId'
                  OR $2 = o.payload -> 'message' ->> 'sessionId'
                  OR $2 = r.session_id
              )
            ORDER BY o.id ASC
            LIMIT $3
            "#,
            since,
            session_id,
            limit,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;

        Ok(rows
            .into_iter()
            .map(|r| OutboxRecord {
                id: r.id,
                event_type: r.event_type,
                payload: r.payload,
                created_at: r.created_at,
            })
            .collect())
    }
}

fn storage(e: sqlx::Error) -> RepositoryError {
    RepositoryError::Storage(e.to_string())
}
