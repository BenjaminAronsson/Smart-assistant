//! Approval-surface persistence (F2.5, docs/06 §3, invariant #6).
//!
//! The human-approval seam records its lifecycle the same way every other
//! domain change does: the `approval.requested` / `approval.resolved` domain
//! event goes to the transactional **outbox** (so the dispatcher publishes it to
//! the WS and a reconnecting client recovers it from the timeline — docs/05 §3),
//! and its **audit** row goes to the hash chain — both in ONE transaction. The
//! record a human is shown and the record the audit keeps can never diverge
//! (skill `sqlx-data` §5/§6). This reuses the existing `insert_outbox` +
//! `audit::append` queries, so it adds no new compile-time-checked SQL.

use jarvis_domain::audit::AuditEvent;
use sqlx::PgPool;

#[derive(Debug, thiserror::Error)]
pub enum ApprovalPersistError {
    #[error("approval storage failure: {0}")]
    Storage(#[from] sqlx::Error),
    #[error(transparent)]
    Audit(#[from] crate::audit::AuditError),
}

/// Persist one approval-lifecycle event atomically: the domain event to the
/// outbox (published to the WS by the dispatcher) and its audit row to the hash
/// chain, in a single transaction. `outbox_payload` is the wire payload MINUS
/// the `type` discriminator — the envelope carries it — matching the run-event
/// convention the timeline reader folds back (`jarvisd::runs::domain_event`).
pub async fn record_approval_event(
    pool: &PgPool,
    event_type: &str,
    outbox_payload: serde_json::Value,
    audit: &AuditEvent,
) -> Result<(), ApprovalPersistError> {
    let mut tx = pool.begin().await?;
    crate::runs::insert_outbox(&mut tx, event_type, outbox_payload).await?;
    crate::audit::append(&mut tx, audit).await?;
    tx.commit().await?;
    Ok(())
}
