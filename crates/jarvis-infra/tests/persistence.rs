//! F0.6: session repository + audit chain against real Postgres
//! (docs/04 §2-§3, invariant 6, sqlx-data skill §6). Each test runs in an
//! isolated throwaway database created by `#[sqlx::test]` from DATABASE_URL,
//! with the workspace migration stream applied.

use jarvis_application::ports::{RepositoryError, SessionStore};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::conversations::{Session, SessionStatus};
use jarvis_domain::ids::SessionId;
use jarvis_infra::sessions::PgSessionStore;
use sqlx::PgPool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Microsecond-exact timestamp (timestamptz precision) so round-trips
/// compare with `==` instead of tolerances.
fn ts(micros: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_micros(1_800_000_000_000_000 + micros)
}

fn session(id: &str, title: Option<&str>, micros: u64) -> Session {
    Session::new(
        id.parse::<SessionId>().unwrap(),
        title.map(str::to_owned),
        ts(micros),
    )
}

fn audit_for(session: &Session) -> AuditEvent {
    AuditEvent {
        occurred_at: session.created_at,
        actor: "device:01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        event_type: "session.created".into(),
        target: format!("session:{}", session.id),
        correlation_id: Some("trace-test".into()),
        payload_json: format!(r#"{{"sessionId":"{}"}}"#, session.id),
    }
}

const ID_A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const ID_B: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
const ID_C: &str = "01BX5ZZKBKACTAV9WEVGEMMVS0";

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn create_then_get_round_trips(pool: PgPool) {
    let store = PgSessionStore::new(pool);
    let created = session(ID_A, Some("morning plans"), 0);
    store.create(&created, &audit_for(&created)).await.unwrap();

    let fetched = store.get(&ID_A.parse().unwrap()).await.unwrap();
    assert_eq!(fetched, Some(created));
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn get_unknown_id_is_none(pool: PgPool) {
    let store = PgSessionStore::new(pool);
    assert_eq!(store.get(&ID_B.parse().unwrap()).await.unwrap(), None);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn list_is_newest_first_with_limit(pool: PgPool) {
    let store = PgSessionStore::new(pool);
    for (id, micros) in [(ID_A, 0), (ID_B, 1_000), (ID_C, 2_000)] {
        let s = session(id, None, micros);
        store.create(&s, &audit_for(&s)).await.unwrap();
    }
    let listed = store.list(2).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id.as_str(), ID_C);
    assert_eq!(listed[1].id.as_str(), ID_B);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn duplicate_create_is_conflict_and_leaves_no_partial_writes(pool: PgPool) {
    let store = PgSessionStore::new(pool.clone());
    let first = session(ID_A, Some("one"), 0);
    store.create(&first, &audit_for(&first)).await.unwrap();

    let dup = session(ID_A, Some("two"), 1_000);
    let err = store.create(&dup, &audit_for(&dup)).await.unwrap_err();
    assert!(matches!(err, RepositoryError::Conflict(_)), "got {err:?}");

    // Atomicity (invariant 6): the failed transaction left NOTHING behind.
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit.audit_events WHERE event_type = 'session.created'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let outbox: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox.outbox_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((audits, outbox), (1, 1));
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn create_writes_audit_and_outbox_in_same_transaction(pool: PgPool) {
    let store = PgSessionStore::new(pool.clone());
    let s = session(ID_A, None, 0);
    store.create(&s, &audit_for(&s)).await.unwrap();

    let (event_type, target): (String, String) = sqlx::query_as(
        "SELECT event_type, target FROM audit.audit_events ORDER BY seq DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(event_type, "session.created");
    assert_eq!(target, format!("session:{ID_A}"));

    let outbox_type: String =
        sqlx::query_scalar("SELECT event_type FROM outbox.outbox_events ORDER BY id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(outbox_type, "session.created");

    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        jarvis_infra::audit::verify_chain(&mut conn).await.unwrap(),
        1
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn audit_chain_links_and_verifies_across_transactions(pool: PgPool) {
    for i in 0..3u64 {
        let mut tx = pool.begin().await.unwrap();
        let event = AuditEvent {
            occurred_at: ts(i * 1_000),
            actor: "system".into(),
            event_type: format!("test.event{i}"),
            target: "test:chain".into(),
            correlation_id: None,
            payload_json: format!(r#"{{"i":{i}}}"#),
        };
        jarvis_infra::audit::append(&mut tx, &event).await.unwrap();
        tx.commit().await.unwrap();
    }

    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        jarvis_infra::audit::verify_chain(&mut conn).await.unwrap(),
        3
    );

    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT prev_hash, hash FROM audit.audit_events ORDER BY seq")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows[0].0, "", "genesis prev_hash is empty");
    assert_eq!(rows[1].0, rows[0].1);
    assert_eq!(rows[2].0, rows[1].1);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn audit_rows_reject_update_and_delete(pool: PgPool) {
    let store = PgSessionStore::new(pool.clone());
    let s = session(ID_A, None, 0);
    store.create(&s, &audit_for(&s)).await.unwrap();

    let update = sqlx::query("UPDATE audit.audit_events SET actor = 'attacker'")
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(update.to_string().contains("append-only"), "{update}");

    let delete = sqlx::query("DELETE FROM audit.audit_events")
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(delete.to_string().contains("append-only"), "{delete}");
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn archived_status_round_trips(pool: PgPool) {
    let store = PgSessionStore::new(pool);
    let mut s = session(ID_A, Some("done"), 0);
    s.status = SessionStatus::Archived;
    store.create(&s, &audit_for(&s)).await.unwrap();
    let fetched = store.get(&s.id).await.unwrap().unwrap();
    assert_eq!(fetched.status, SessionStatus::Archived);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn audit_chain_survives_jsonb_key_normalization(pool: PgPool) {
    // JSONB re-orders object keys (length, then bytewise) and normalizes
    // whitespace/numbers; the chain must verify from what Postgres returns,
    // not from the original text. Multi-key, nested, non-sorted input.
    let mut tx = pool.begin().await.unwrap();
    let event = AuditEvent {
        occurred_at: ts(0),
        actor: "system".into(),
        event_type: "test.jsonb".into(),
        target: "test:jsonb".into(),
        correlation_id: None,
        payload_json:
            r#"{"zebra": 1, "aa": {"y": 2.50, "x": "ü"}, "b": [3, {"k2": null, "k1": true}]}"#
                .into(),
    };
    jarvis_infra::audit::append(&mut tx, &event).await.unwrap();
    tx.commit().await.unwrap();

    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        jarvis_infra::audit::verify_chain(&mut conn).await.unwrap(),
        1
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn audit_chain_verifies_with_nanosecond_timestamps(pool: PgPool) {
    // Regression: timestamptz stores microseconds; a hash computed over
    // nanosecond precision could never be re-derived from the stored row.
    let mut tx = pool.begin().await.unwrap();
    let event = AuditEvent {
        occurred_at: UNIX_EPOCH + Duration::from_nanos(1_800_000_000_123_456_789),
        actor: "system".into(),
        event_type: "test.nanos".into(),
        target: "test:nanos".into(),
        correlation_id: None,
        payload_json: r#"{"ok":true}"#.into(),
    };
    jarvis_infra::audit::append(&mut tx, &event).await.unwrap();
    tx.commit().await.unwrap();

    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        jarvis_infra::audit::verify_chain(&mut conn).await.unwrap(),
        1
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn audit_chain_verifies_with_numeric_edge_payloads(pool: PgPool) {
    // Exponents, floats, and large integers must normalize identically on
    // the append side (serde_json parse) and the verify side (JSONB round
    // trip) — the parsed Value is what gets stored, so both agree.
    let mut tx = pool.begin().await.unwrap();
    let event = AuditEvent {
        occurred_at: ts(0),
        actor: "system".into(),
        event_type: "test.numbers".into(),
        target: "test:numbers".into(),
        correlation_id: None,
        payload_json: r#"{"exp": 1e2, "float": 0.30000000000000004, "big": 18446744073709551615, "neg": -12.5}"#.into(),
    };
    jarvis_infra::audit::append(&mut tx, &event).await.unwrap();
    tx.commit().await.unwrap();

    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        jarvis_infra::audit::verify_chain(&mut conn).await.unwrap(),
        1
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn audit_rows_reject_truncate(pool: PgPool) {
    let store = PgSessionStore::new(pool.clone());
    let s = session(ID_A, None, 0);
    store.create(&s, &audit_for(&s)).await.unwrap();

    let truncate = sqlx::query("TRUNCATE audit.audit_events")
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(truncate.to_string().contains("append-only"), "{truncate}");
}
