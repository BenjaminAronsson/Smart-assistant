//! `PgAuditSink` — the production [`AuditSink`] (CF-2, orchestrator half).
//!
//! The orchestrator records policy decisions and tool executions through the
//! neutral [`AuditSink`] port (`policy.approval_requested`, `tool.executed`,
//! `tool.failed`, …). Until now no host wired that port, so those events were
//! dropped once a live `ToolStack` existed. This sink appends each event to the
//! hash-chained `audit.audit_events` table via [`crate::audit::append`], in the
//! sink's **own** transaction.
//!
//! Residual gap (tracked as CF-2): the port is `async fn record(&self, event) ->
//! ()` — fire-and-forget, with no error channel — so the append cannot be made
//! atomic with the run's checkpoint/outbox write. This sink therefore closes the
//! *durability* half (orchestrator-emitted events are now persisted and
//! chained), not the *atomicity* half; a crash after a side effect but before
//! `record` commits still leaves an unaudited effect. Making it atomic needs the
//! port to thread the caller's transaction — a domain/application port change
//! deferred out of F2.6. The security-critical grant lifecycle is already atomic
//! inside `PgGrantStore` (F2.4), so only best-effort observability events flow
//! here.

use async_trait::async_trait;
use jarvis_application::policy::AuditSink;
use jarvis_domain::audit::AuditEvent;
use sqlx::PgPool;

/// Appends orchestrator-emitted audit events to the hash-chained audit log.
pub struct PgAuditSink {
    pool: PgPool,
}

impl PgAuditSink {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AuditSink for PgAuditSink {
    async fn record(&self, event: AuditEvent) {
        // The port returns `()`: a failure has no channel to the run and must
        // never panic (that would abort the run task). Persist durably in this
        // sink's own transaction; on any fault, log the stable `event_type` (a
        // machine code like `tool.executed` — never the payload, invariant #5)
        // and drop the tx so it rolls back.
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(error) => {
                tracing::error!(%error, event_type = %event.event_type, "audit sink: begin failed");
                return;
            }
        };
        if let Err(error) = crate::audit::append(&mut tx, &event).await {
            tracing::error!(%error, event_type = %event.event_type, "audit sink: append failed");
            return; // tx dropped here → rollback; no partial chain write.
        }
        if let Err(error) = tx.commit().await {
            tracing::error!(%error, event_type = %event.event_type, "audit sink: commit failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn event(event_type: &str) -> AuditEvent {
        AuditEvent {
            occurred_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            actor: "system".to_owned(),
            event_type: event_type.to_owned(),
            target: "tool:example.light".to_owned(),
            correlation_id: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned()),
            payload_json: r#"{"detail":"ok"}"#.to_owned(),
        }
    }

    /// The sink durably appends through the hash chain: two events written back
    /// to back link (`prev_hash` of the second is the recorded head), proving the
    /// records committed and are chained — the orchestrator half of CF-2.
    #[sqlx::test(migrations = "../../migrations")]
    async fn record_appends_a_chained_event(pool: PgPool) {
        let sink = PgAuditSink::new(pool.clone());

        sink.record(event("tool.executed")).await;
        sink.record(event("tool.failed")).await;

        let rows = sqlx::query!(
            "SELECT event_type, prev_hash, hash FROM audit.audit_events ORDER BY seq ASC"
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(rows.len(), 2, "both events persisted");
        assert_eq!(rows[0].event_type, "tool.executed");
        assert_eq!(rows[1].event_type, "tool.failed");
        // The chain links: the second row's prev_hash is the first row's hash.
        assert_eq!(
            rows[1].prev_hash, rows[0].hash,
            "second event chains onto the first"
        );
        assert_ne!(rows[0].hash, rows[1].hash, "distinct chain heads");
    }
}
