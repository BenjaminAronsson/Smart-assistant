//! Append-only, hash-chained audit writes (invariant 6; sqlx-data skill §6).
//!
//! `append` runs INSIDE the caller's transaction so the audit row commits or
//! rolls back with the domain change it describes. Chain integrity:
//! `hash = sha256(prev_hash || canonical_json(event))`, appends serialized by
//! a transaction-scoped advisory lock.

use jarvis_domain::audit::AuditEvent;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("audit storage failure: {0}")]
    Storage(#[from] sqlx::Error),
    #[error("audit payload is not valid JSON: {0}")]
    InvalidPayload(#[from] serde_json::Error),
    #[error("audit timestamp not representable: {0}")]
    Timestamp(#[from] time::error::Format),
}

/// Append one event to the chain inside `tx`. Returns the new chain hash.
pub async fn append(tx: &mut PgConnection, event: &AuditEvent) -> Result<String, AuditError> {
    // Serialize concurrent appends: the chain has exactly one head. The lock
    // is transaction-scoped and keyed to the audit chain alone.
    sqlx::query!("SELECT pg_advisory_xact_lock(hashtext('audit.audit_events.chain'))")
        .fetch_one(&mut *tx)
        .await?;

    let prev_hash: String =
        sqlx::query_scalar!("SELECT hash FROM audit.audit_events ORDER BY seq DESC LIMIT 1")
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or_default();

    // Hash EXACTLY what will be read back at verification time:
    // - timestamptz stores microseconds, so truncate before hashing/inserting
    //   (a nanosecond-precision hash could never be re-derived from the row);
    // - the payload is hashed from (and inserted as) the parsed Value, so
    //   serde_json's normalization (1e2 → 100.0 etc.) happens before Postgres
    //   ever sees it and applies identically on the verify side.
    let occurred_at = truncate_to_micros(OffsetDateTime::from(event.occurred_at));
    let payload: serde_json::Value = serde_json::from_str(&event.payload_json)?;
    let canonical = canonical_json(event, &occurred_at, &payload)?;

    let mut hasher = Sha256::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(canonical.as_bytes());
    let hash = hex::encode(hasher.finalize());

    sqlx::query!(
        r#"
        INSERT INTO audit.audit_events
            (occurred_at, actor, event_type, target, correlation_id, payload, prev_hash, hash)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
        occurred_at,
        event.actor,
        event.event_type,
        event.target,
        event.correlation_id.as_deref(),
        payload,
        prev_hash,
        hash,
    )
    .execute(&mut *tx)
    .await?;

    Ok(hash)
}

fn truncate_to_micros(dt: OffsetDateTime) -> OffsetDateTime {
    dt.replace_nanosecond((dt.nanosecond() / 1_000) * 1_000)
        .expect("truncated nanosecond is always in range")
}

/// Canonical event serialization: BTreeMap gives deterministic (sorted) key
/// order; the exact string produced here is the only thing ever hashed.
/// Nested payload key order relies on serde_json's default BTreeMap `Map` —
/// enabling the `preserve_order` feature anywhere in the workspace would
/// break every chain (guarded by `serde_json_map_is_sorted` below).
fn canonical_json(
    event: &AuditEvent,
    occurred_at: &OffsetDateTime,
    payload: &serde_json::Value,
) -> Result<String, AuditError> {
    let mut map = std::collections::BTreeMap::new();
    map.insert("actor", serde_json::Value::String(event.actor.clone()));
    map.insert(
        "correlationId",
        match &event.correlation_id {
            Some(c) => serde_json::Value::String(c.clone()),
            None => serde_json::Value::Null,
        },
    );
    map.insert(
        "eventType",
        serde_json::Value::String(event.event_type.clone()),
    );
    map.insert(
        "occurredAt",
        serde_json::Value::String(occurred_at.format(&Rfc3339)?),
    );
    map.insert("payload", payload.clone());
    map.insert("target", serde_json::Value::String(event.target.clone()));
    Ok(serde_json::to_string(&map).expect("BTreeMap of valid JSON values serializes"))
}

/// Verify the whole chain from genesis; returns the number of verified events.
/// Used by tests now, restore verification later (docs/09 §3).
pub async fn verify_chain(conn: &mut PgConnection) -> Result<u64, AuditError> {
    let rows = sqlx::query!(
        r#"
        SELECT occurred_at, actor, event_type, target, correlation_id, payload, prev_hash, hash
        FROM audit.audit_events ORDER BY seq
        "#
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut expected_prev = String::new();
    let mut count = 0u64;
    for row in rows {
        if row.prev_hash != expected_prev {
            return Err(AuditError::Storage(sqlx::Error::Protocol(format!(
                "audit chain broken at event {count}: prev_hash mismatch"
            ))));
        }
        let event = AuditEvent {
            occurred_at: row.occurred_at.into(),
            actor: row.actor,
            event_type: row.event_type,
            target: row.target,
            correlation_id: row.correlation_id,
            // Reconstructed for the struct only — canonical_json below hashes
            // `row.payload` directly; this field is NOT hash input.
            payload_json: row.payload.to_string(),
        };
        let canonical = canonical_json(&event, &row.occurred_at, &row.payload)?;
        let mut hasher = Sha256::new();
        hasher.update(expected_prev.as_bytes());
        hasher.update(canonical.as_bytes());
        let recomputed = hex::encode(hasher.finalize());
        if recomputed != row.hash {
            return Err(AuditError::Storage(sqlx::Error::Protocol(format!(
                "audit chain broken at event {count}: hash mismatch"
            ))));
        }
        expected_prev = row.hash;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    /// Chain determinism depends on serde_json's default sorted Map. If any
    /// workspace crate enables `serde_json/preserve_order` (feature
    /// unification is global), object keys become insertion-ordered and every
    /// stored audit chain silently stops verifying — fail loudly here instead.
    #[test]
    fn serde_json_map_is_sorted() {
        let value = serde_json::json!({"b": 1, "a": 2});
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"{"a":2,"b":1}"#,
            "serde_json/preserve_order got enabled somewhere — audit chains break"
        );
    }
}
