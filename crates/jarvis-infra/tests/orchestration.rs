//! F1.4: run + message persistence and the transactional outbox, against real
//! Postgres (docs/04 §3, docs/02 §2-§4, NFR-05). Each test runs in an isolated
//! throwaway database created by `#[sqlx::test]` with the migration stream
//! applied. Test-only seeding uses unchecked queries so it needs no offline
//! data of its own.

use jarvis_application::orchestrator::Checkpointer;
use jarvis_application::ports::{MessageStore, RunStore};
use jarvis_domain::conversations::{Message, MessageRole};
use jarvis_domain::ids::{MessageId, RunId, SessionId};
use jarvis_domain::run::{Run, RunBudget, RunEvent, RunOutcomeKind, RunState};
use jarvis_infra::messages::PgMessageStore;
use jarvis_infra::runs::PgRunStore;
use serde_json::json;
use sqlx::PgPool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SESSION_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";
const RUN_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const MSG_A: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
const MSG_B: &str = "01BX5ZZKBKACTAV9WEVGEMMVS0";

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

async fn undispatched_event_types(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT event_type FROM outbox.outbox_events ORDER BY id ASC")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn latest_payload(pool: &PgPool, event_type: &str) -> serde_json::Value {
    sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT payload FROM outbox.outbox_events WHERE event_type = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(event_type)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn a_run() -> Run {
    Run::new(
        RUN_ID.parse::<RunId>().unwrap(),
        SESSION_ID.parse::<SessionId>().unwrap(),
        RunBudget::default_interactive(),
    )
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn run_create_then_load_round_trips(pool: PgPool) {
    seed_session(&pool, SESSION_ID).await;
    let store = PgRunStore::new(pool.clone());

    let run = a_run();
    store.create(&run).await.unwrap();

    let loaded = store.load(&RUN_ID.parse().unwrap()).await.unwrap();
    assert_eq!(loaded, Some(run));

    // create emitted exactly one run.started event.
    assert_eq!(undispatched_event_types(&pool).await, vec!["run.started"]);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn load_unknown_run_is_none(pool: PgPool) {
    let store = PgRunStore::new(pool);
    assert_eq!(store.load(&RUN_ID.parse().unwrap()).await.unwrap(), None);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn checkpoint_persists_state_and_survives_reload(pool: PgPool) {
    seed_session(&pool, SESSION_ID).await;
    let store = PgRunStore::new(pool.clone());

    let mut run = a_run();
    store.create(&run).await.unwrap();

    // Advance to ContextReady and checkpoint — the run "survives restart".
    run.apply(RunEvent::ContextAssembled).unwrap();
    store.save(&run).await.unwrap();

    let reloaded = store.load(&RUN_ID.parse().unwrap()).await.unwrap().unwrap();
    assert_eq!(reloaded.state, RunState::ContextReady);

    // A checkpoint row was recorded for the boundary.
    let checkpoints: i64 =
        sqlx::query_scalar("SELECT count(*) FROM orchestration.checkpoints WHERE run_id = $1")
            .bind(RUN_ID)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(checkpoints, 1);

    // Outbox now holds run.started then run.state_changed, in order.
    assert_eq!(
        undispatched_event_types(&pool).await,
        vec!["run.started", "run.state_changed"]
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn terminal_checkpoint_records_outcome_and_completed_event(pool: PgPool) {
    seed_session(&pool, SESSION_ID).await;
    let store = PgRunStore::new(pool.clone());

    let mut run = a_run();
    store.create(&run).await.unwrap();
    // Drive the no-tool happy path to Completed, checkpointing the terminal.
    run.apply(RunEvent::ContextAssembled).unwrap();
    run.apply(RunEvent::ModelInvoked).unwrap();
    run.apply(RunEvent::FinalResponseReceived).unwrap();
    run.apply(RunEvent::ResponseCommitted).unwrap();
    store.save(&run).await.unwrap();

    let reloaded = store.load(&RUN_ID.parse().unwrap()).await.unwrap().unwrap();
    assert_eq!(reloaded.state, RunState::Completed);
    assert_eq!(
        reloaded.outcome.map(|o| o.kind),
        Some(RunOutcomeKind::Completed)
    );

    let types = undispatched_event_types(&pool).await;
    assert_eq!(types.first().map(String::as_str), Some("run.started"));
    assert_eq!(types.last().map(String::as_str), Some("run.completed"));
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn message_append_then_list_round_trips(pool: PgPool) {
    seed_session(&pool, SESSION_ID).await;
    let store = PgMessageStore::new(pool.clone());

    let user = Message::new(
        MSG_A.parse::<MessageId>().unwrap(),
        SESSION_ID.parse::<SessionId>().unwrap(),
        MessageRole::User,
        "what's the weather".into(),
        ts(0),
    );
    let assistant = Message::new(
        MSG_B.parse::<MessageId>().unwrap(),
        SESSION_ID.parse::<SessionId>().unwrap(),
        MessageRole::Assistant,
        "clear skies".into(),
        ts(1),
    );
    store.append(&user).await.unwrap();
    store.append(&assistant).await.unwrap();

    let listed = store
        .list_by_session(&SESSION_ID.parse().unwrap(), 10)
        .await
        .unwrap();
    assert_eq!(listed, vec![user, assistant]);

    // Each append emitted a message.created event.
    assert_eq!(
        undispatched_event_types(&pool).await,
        vec!["message.created", "message.created"]
    );
}

/// Pins the hand-built outbox payloads to the wire `DomainEvent`/`MessageDto`
/// shapes (docs/05 §2-§3). infra cannot depend on jarvis-contracts, so nothing
/// mechanical stops these from drifting once the host forwards them verbatim
/// (F1.5) — this literal-fixture test is the guard (contract-keeper F1.4).
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn outbox_payloads_match_the_wire_shapes(pool: PgPool) {
    seed_session(&pool, SESSION_ID).await;
    let runs = PgRunStore::new(pool.clone());
    let messages = PgMessageStore::new(pool.clone());

    let mut run = a_run();
    runs.create(&run).await.unwrap();
    assert_eq!(
        latest_payload(&pool, "run.started").await,
        json!({ "runId": RUN_ID, "sessionId": SESSION_ID })
    );

    run.apply(RunEvent::ContextAssembled).unwrap();
    runs.save(&run).await.unwrap();
    assert_eq!(
        latest_payload(&pool, "run.state_changed").await,
        json!({ "runId": RUN_ID, "state": "context_ready" })
    );

    run.apply(RunEvent::ModelInvoked).unwrap();
    run.apply(RunEvent::FinalResponseReceived).unwrap();
    run.apply(RunEvent::ResponseCommitted).unwrap();
    runs.save(&run).await.unwrap();
    // detail is None here, so it is omitted (skip_serializing_if on the wire).
    assert_eq!(
        latest_payload(&pool, "run.completed").await,
        json!({ "runId": RUN_ID, "outcome": { "kind": "completed" } })
    );

    let message = Message::new(
        MSG_A.parse::<MessageId>().unwrap(),
        SESSION_ID.parse::<SessionId>().unwrap(),
        MessageRole::User,
        "hi".into(),
        ts(0),
    );
    messages.append(&message).await.unwrap();
    let payload = latest_payload(&pool, "message.created").await;
    assert_eq!(payload["message"]["id"], json!(MSG_A));
    assert_eq!(payload["message"]["sessionId"], json!(SESSION_ID));
    assert_eq!(payload["message"]["role"], json!("user"));
    assert_eq!(
        payload["message"]["content"],
        json!([{ "type": "text", "text": "hi" }])
    );
    // createdAt is RFC 3339 text.
    assert!(
        payload["message"]["createdAt"]
            .as_str()
            .unwrap()
            .contains('T')
    );
}
