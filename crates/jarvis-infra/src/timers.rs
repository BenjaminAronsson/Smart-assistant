//! Timer persistence (FR-33, ADR-023, migration 0011, invariant #6).
//! The infra side of the [`TimerStore`] port.
//!
//! Two properties this module exists to guarantee:
//!
//! * **Restart safety (NFR-05).** [`TimerStore::list_live`] is the whole
//!   recovery mechanism: whatever was armed or ringing when the process died is
//!   still here, so a timer whose moment passed while jarvisd was down is fired
//!   (with a "missed" notice) instead of vanishing.
//! * **A timer rings exactly once.** [`TimerStore::apply`] is a compare-and-set:
//!   `UPDATE … WHERE id = $1 AND state = $expected`. If the row already moved —
//!   a second scheduler swept it, or the human dismissed it in the same instant
//!   — zero rows change, the transaction rolls back, and **nothing** is written:
//!   no audit row, no `timer.fired` outbox event, no second tone.
//!
//! Every write co-transacts its audit row, and a fire additionally writes its
//! domain event to the outbox in the same transaction (invariant #6 + skill
//! `sqlx-data` §5) — so the record the human hears, the event the HUD replays,
//! and the audit chain can never disagree.

use async_trait::async_trait;
use jarvis_application::ports::{DomainEventRecord, RepositoryError, TimerStore};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::TimerId;
use jarvis_domain::timers::{Timer, TimerKind, TimerName, TimerNote, TimerState};
use sqlx::PgPool;
use std::time::{Duration, SystemTime};
use time::OffsetDateTime;

/// Postgres-backed timer store.
pub struct PgTimerStore {
    pool: PgPool,
}

impl PgTimerStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn storage(context: &str) -> impl Fn(sqlx::Error) -> RepositoryError + '_ {
    move |e| {
        // A duplicate id is a conflict, not a generic failure — the caller
        // minted an id that is already in use.
        if let Some(db) = e.as_database_error()
            && db.code().as_deref() == Some("23505")
        {
            return RepositoryError::Conflict("timer already exists".to_owned());
        }
        RepositoryError::Storage(format!("{context}: {e}"))
    }
}

/// One stored row, before it becomes a domain [`Timer`].
struct TimerRow {
    id: String,
    name: String,
    kind: String,
    duration_secs: Option<i64>,
    note: Option<String>,
    state: String,
    fire_at: OffsetDateTime,
    created_at: OffsetDateTime,
}

impl TimerRow {
    /// Rebuild the domain type. Every column is re-validated: a row we cannot
    /// interpret is an error, never a default — in particular an unreadable
    /// `state` must not silently read as `pending` and fire.
    fn into_timer(self) -> Result<Timer, RepositoryError> {
        let id = self
            .id
            .parse::<TimerId>()
            .map_err(|e| RepositoryError::Storage(format!("bad timer id: {e}")))?;
        let name = TimerName::new(&self.name)
            .map_err(|e| RepositoryError::Storage(format!("bad timer name: {e}")))?;
        let state = TimerState::parse(&self.state).ok_or_else(|| {
            RepositoryError::Storage(format!("unknown timer state {:?}", self.state))
        })?;
        let kind = match self.kind.as_str() {
            "countdown" => {
                let secs = self.duration_secs.ok_or_else(|| {
                    RepositoryError::Storage("countdown row has no duration".to_owned())
                })?;
                TimerKind::Countdown {
                    duration: Duration::from_secs(secs.max(0).unsigned_abs()),
                }
            }
            "alarm" => TimerKind::Alarm,
            "reminder" => {
                let raw = self.note.as_deref().ok_or_else(|| {
                    RepositoryError::Storage("reminder row has no note".to_owned())
                })?;
                TimerKind::Reminder {
                    note: TimerNote::new(raw)
                        .map_err(|e| RepositoryError::Storage(format!("bad reminder note: {e}")))?,
                }
            }
            other => {
                return Err(RepositoryError::Storage(format!(
                    "unknown timer kind {other:?}"
                )));
            }
        };
        Ok(Timer::from_parts(
            id,
            name,
            kind,
            state,
            self.fire_at.into(),
            self.created_at.into(),
        ))
    }
}

/// The two kind-dependent columns, in the shape the CHECK constraints require:
/// exactly one is populated, decided by the kind.
fn kind_columns(kind: &TimerKind) -> (Option<i64>, Option<&str>) {
    match kind {
        TimerKind::Countdown { duration } => (
            Some(i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)),
            None,
        ),
        TimerKind::Alarm => (None, None),
        TimerKind::Reminder { note } => (None, Some(note.as_str())),
    }
}

fn utc(t: SystemTime) -> OffsetDateTime {
    OffsetDateTime::from(t)
}

#[async_trait]
impl TimerStore for PgTimerStore {
    async fn create(&self, timer: &Timer, audit: &AuditEvent) -> Result<(), RepositoryError> {
        let (duration_secs, note) = kind_columns(timer.kind());
        let now = utc(timer.created_at());

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(storage("timer create: begin"))?;
        sqlx::query!(
            r#"
            INSERT INTO timers.timers
                (id, name, kind, duration_secs, note, state, fire_at, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)
            "#,
            timer.id().as_str(),
            timer.name().as_str(),
            timer.kind().as_str(),
            duration_secs,
            note,
            timer.state().as_str(),
            utc(timer.fire_at()),
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(storage("timer create: insert"))?;

        // Same transaction as the row (invariant #6): a timer that cannot be
        // audited is not set at all.
        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("timer create: audit: {e}")))?;

        tx.commit().await.map_err(storage("timer create: commit"))?;
        Ok(())
    }

    async fn get(&self, id: &TimerId) -> Result<Option<Timer>, RepositoryError> {
        let row = sqlx::query_as!(
            TimerRow,
            r#"
            SELECT id, name, kind, duration_secs, note, state, fire_at, created_at
            FROM timers.timers
            WHERE id = $1
            "#,
            id.as_str(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(storage("timer get"))?;
        row.map(TimerRow::into_timer).transpose()
    }

    async fn list_live(&self) -> Result<Vec<Timer>, RepositoryError> {
        // The restart worklist AND the scheduler worklist — one query, so the
        // two can never disagree about what is outstanding. Terminal rows are
        // excluded by the same predicate as the partial index.
        let rows = sqlx::query_as!(
            TimerRow,
            r#"
            SELECT id, name, kind, duration_secs, note, state, fire_at, created_at
            FROM timers.timers
            WHERE state IN ('pending', 'snoozed', 'fired')
            ORDER BY fire_at ASC, id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage("timer list_live"))?;
        rows.into_iter().map(TimerRow::into_timer).collect()
    }

    async fn apply(
        &self,
        next: &Timer,
        expected: TimerState,
        audit: &AuditEvent,
        event: Option<&DomainEventRecord>,
    ) -> Result<bool, RepositoryError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(storage("timer apply: begin"))?;

        // The compare-and-set. `fire_at` moves only because a snooze moved it;
        // for every other transition `next.fire_at()` equals the stored value.
        let updated = sqlx::query!(
            r#"
            UPDATE timers.timers
            SET state = $2, fire_at = $3, updated_at = $4
            WHERE id = $1 AND state = $5
            "#,
            next.id().as_str(),
            next.state().as_str(),
            utc(next.fire_at()),
            utc(audit.occurred_at),
            expected.as_str(),
        )
        .execute(&mut *tx)
        .await
        .map_err(storage("timer apply: update"))?
        .rows_affected();

        if updated == 0 {
            // Lost the race (or unknown id). Roll back so NOTHING is written —
            // no audit row for a change that did not happen, and no event the
            // HUD would replay as a second ring.
            tx.rollback()
                .await
                .map_err(storage("timer apply: rollback"))?;
            return Ok(false);
        }

        if let Some(event) = event {
            let payload: serde_json::Value =
                serde_json::from_str(&event.payload_json).map_err(|e| {
                    RepositoryError::Storage(format!("timer apply: event payload: {e}"))
                })?;
            crate::runs::insert_outbox(&mut tx, &event.event_type, payload)
                .await
                .map_err(storage("timer apply: outbox"))?;
        }

        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("timer apply: audit: {e}")))?;

        tx.commit().await.map_err(storage("timer apply: commit"))?;
        Ok(true)
    }
}
