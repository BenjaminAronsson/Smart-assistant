//! Transactional-outbox dispatcher (docs/02 §2, sqlx-data skill §5). Domain
//! events are written to `outbox.outbox_events` in the same transaction as the
//! state change; this dispatcher publishes them AFTER commit, in order, exactly
//! where they become visible.
//!
//! It is **event-driven, not polling** (perf-warden): a `LISTEN` on the
//! `outbox_events` channel (fired by the `AFTER INSERT` trigger, migration
//! 0007) wakes it; it then drains every undispatched row. On start it drains
//! once to clear any backlog inserted while it was down. Delivery is
//! at-least-once — a crash between publish and mark re-delivers, and clients
//! resync by `seq` (docs/05 §3) — so publishers must tolerate a repeat.

use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

/// One committed outbox event handed to the publisher. `created_at` is the row's
/// commit timestamp — the true occurrence time the WS/timeline surface as
/// `occurredAt`, so a replayed event keeps its original time rather than "now".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRecord {
    pub id: i64,
    pub event_type: String,
    pub payload: Value,
    pub created_at: OffsetDateTime,
}

/// A publisher failure (e.g. the WS hub could not accept the event). Returning
/// it fails the whole batch so nothing is marked dispatched and the events
/// re-deliver — never silently lost (perf-warden F1.4).
#[derive(Debug, thiserror::Error)]
#[error("outbox publish failed: {0}")]
pub struct PublishError(pub String);

/// Sink for committed domain events (the WS hub implements this in F1.5). Called
/// in `id` order; must not assume exactly-once. An `Err` aborts the batch before
/// it is marked dispatched, so the batch re-delivers on the next run.
#[async_trait::async_trait]
pub trait OutboxPublisher: Send + Sync {
    async fn publish(&self, record: &OutboxRecord) -> Result<(), PublishError>;
}

/// What ends the dispatch loop: a database/listener error or a publisher error.
/// Either way the host restarts the dispatcher, which re-delivers.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Publish(#[from] PublishError),
}

/// How many undispatched rows to claim per query (bounded so a large backlog
/// can never build an unbounded batch in memory — low-power discipline).
const BATCH: i64 = 100;

pub struct OutboxDispatcher {
    pool: PgPool,
}

impl OutboxDispatcher {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Run until `cancel` fires. Drains the backlog once, then reacts to each
    /// `NOTIFY`. On cancellation it drains ONE more time before returning, so
    /// events committed during a graceful shutdown (e.g. a run's terminal
    /// `run.completed`) are still published live rather than only recovered by a
    /// later `since`/timeline resync. A database/listener error ends the loop
    /// with `Err` (the host restarts it).
    pub async fn run(
        &self,
        publisher: &dyn OutboxPublisher,
        cancel: CancellationToken,
    ) -> Result<(), DispatchError> {
        let mut listener = PgListener::connect_with(&self.pool).await?;
        listener.listen("outbox_events").await?;

        // Backlog drain: catch anything inserted while we were down.
        self.drain(publisher).await?;

        loop {
            tokio::select! {
                biased;
                // Final drain publishes anything committed since the last wake.
                _ = cancel.cancelled() => return self.drain(publisher).await,
                recv = listener.recv() => {
                    recv?;
                    self.drain(publisher).await?;
                }
            }
        }
    }

    /// Publish every undispatched row, oldest first, until none remain. A
    /// publisher error aborts before the batch is marked dispatched, so the
    /// whole batch re-delivers on the next run (at-least-once).
    async fn drain(&self, publisher: &dyn OutboxPublisher) -> Result<(), DispatchError> {
        loop {
            let rows = sqlx::query!(
                r#"
                SELECT id, event_type, payload, created_at
                FROM outbox.outbox_events
                WHERE dispatched_at IS NULL
                ORDER BY id ASC
                LIMIT $1
                "#,
                BATCH,
            )
            .fetch_all(&self.pool)
            .await?;

            if rows.is_empty() {
                return Ok(());
            }

            let mut ids = Vec::with_capacity(rows.len());
            for row in &rows {
                // A publish failure aborts here — nothing in this batch is
                // marked dispatched, so it all re-delivers on the next run.
                publisher
                    .publish(&OutboxRecord {
                        id: row.id,
                        event_type: row.event_type.clone(),
                        payload: row.payload.clone(),
                        created_at: row.created_at,
                    })
                    .await?;
                ids.push(row.id);
            }

            // Mark the whole batch dispatched in one statement.
            sqlx::query!(
                "UPDATE outbox.outbox_events SET dispatched_at = now() WHERE id = ANY($1)",
                &ids,
            )
            .execute(&self.pool)
            .await?;
        }
    }
}
