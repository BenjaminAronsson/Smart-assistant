//! F1.5: the outbox read side (`PgEventLog`) that feeds the WS `?since=` replay
//! and the session timeline resync (docs/05 §3). Proves the shared cursor (the
//! outbox `id`), oldest-first ordering, and session scoping across both
//! `message.created` (sessionId in payload) and `run.*` (runId → runs join).

use jarvis_application::orchestrator::Checkpointer;
use jarvis_application::ports::{MessageStore, RunStore};
use jarvis_domain::conversations::{Message, MessageRole};
use jarvis_domain::ids::{MessageId, RunId, SessionId};
use jarvis_domain::run::{Run, RunBudget, RunEvent};
use jarvis_infra::events::PgEventLog;
use jarvis_infra::messages::PgMessageStore;
use jarvis_infra::runs::PgRunStore;
use sqlx::PgPool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SESSION_A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";
const SESSION_B: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
const RUN_A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const MSG_A: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";

fn ts(micros: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_micros(1_800_000_000_000_000 + micros)
}

async fn seed_session(pool: &PgPool, id: &str) {
    sqlx::query(
        "INSERT INTO conversation.sessions (id, title, status, created_at, updated_at) \
         VALUES ($1, NULL, 'active', now(), now())",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn since_returns_the_full_owner_stream_in_id_order(pool: PgPool) {
    seed_session(&pool, SESSION_A).await;
    let runs = PgRunStore::new(pool.clone());
    let messages = PgMessageStore::new(pool.clone());
    let log = PgEventLog::new(pool.clone());

    let mut run = Run::new(
        RUN_A.parse::<RunId>().unwrap(),
        SESSION_A.parse::<SessionId>().unwrap(),
        RunBudget::default_interactive(),
    );
    runs.create(&run).await.unwrap(); // run.started
    run.apply(RunEvent::ContextAssembled).unwrap();
    runs.save(&run).await.unwrap(); // run.state_changed
    messages
        .append(&Message::new(
            MSG_A.parse::<MessageId>().unwrap(),
            SESSION_A.parse::<SessionId>().unwrap(),
            MessageRole::User,
            "hi".into(),
            ts(0),
        ))
        .await
        .unwrap(); // message.created

    let all = log.since(0, 100).await.unwrap();
    assert_eq!(
        all.iter()
            .map(|r| r.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["run.started", "run.state_changed", "message.created"]
    );
    // Ids are strictly increasing — the cursor space.
    assert!(all.windows(2).all(|w| w[0].id < w[1].id));

    // `since` is exclusive: cursor at the first row yields the remaining two.
    let after_first = log.since(all[0].id, 100).await.unwrap();
    assert_eq!(after_first.len(), 2);
    assert_eq!(after_first[0].id, all[1].id);

    // limit caps the page.
    assert_eq!(log.since(0, 2).await.unwrap().len(), 2);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn timeline_scopes_to_one_session_across_message_and_run_events(pool: PgPool) {
    seed_session(&pool, SESSION_A).await;
    seed_session(&pool, SESSION_B).await;
    let runs = PgRunStore::new(pool.clone());
    let messages = PgMessageStore::new(pool.clone());
    let log = PgEventLog::new(pool.clone());

    // Session A: a run (start + state_changed) and a message.
    let mut run_a = Run::new(
        RUN_A.parse::<RunId>().unwrap(),
        SESSION_A.parse::<SessionId>().unwrap(),
        RunBudget::default_interactive(),
    );
    runs.create(&run_a).await.unwrap();
    run_a.apply(RunEvent::ContextAssembled).unwrap();
    runs.save(&run_a).await.unwrap();
    messages
        .append(&Message::new(
            MSG_A.parse::<MessageId>().unwrap(),
            SESSION_A.parse::<SessionId>().unwrap(),
            MessageRole::User,
            "for A".into(),
            ts(0),
        ))
        .await
        .unwrap();

    // Session B: a message that must NOT appear in A's timeline.
    messages
        .append(&Message::new(
            "01BX5ZZKBKACTAV9WEVGEMMVS9".parse::<MessageId>().unwrap(),
            SESSION_B.parse::<SessionId>().unwrap(),
            MessageRole::User,
            "for B".into(),
            ts(1),
        ))
        .await
        .unwrap();

    let a = log.timeline(SESSION_A, 0, 100).await.unwrap();
    assert_eq!(
        a.iter().map(|r| r.event_type.as_str()).collect::<Vec<_>>(),
        vec!["run.started", "run.state_changed", "message.created"],
        "run.* rows resolve their session through the runs join"
    );
    // The run.state_changed row (runId only, no sessionId) is scoped correctly.
    assert!(
        a.iter()
            .any(|r| r.event_type == "run.state_changed" && r.payload.get("sessionId").is_none())
    );

    let b = log.timeline(SESSION_B, 0, 100).await.unwrap();
    assert_eq!(
        b.iter().map(|r| r.event_type.as_str()).collect::<Vec<_>>(),
        vec!["message.created"]
    );
    assert_eq!(b[0].payload["message"]["content"][0]["text"], "for B");
}
