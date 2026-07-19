//! F1.4: the event-driven outbox dispatcher (docs/02 §2, perf-warden). Proves
//! it drains a pre-existing backlog on start AND reacts to a live `NOTIFY`,
//! publishes in `id` order, and marks rows dispatched. Uses unchecked queries
//! for test-only inserts so no offline data is needed.

use jarvis_infra::dispatcher::{OutboxDispatcher, OutboxPublisher, OutboxRecord, PublishError};
use serde_json::json;
use sqlx::PgPool;
use std::sync::Mutex;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct RecordingPublisher {
    seen: Mutex<Vec<OutboxRecord>>,
}

#[async_trait::async_trait]
impl OutboxPublisher for RecordingPublisher {
    async fn publish(&self, record: &OutboxRecord) -> Result<(), PublishError> {
        self.seen.lock().unwrap().push(record.clone());
        Ok(())
    }
}

impl RecordingPublisher {
    fn event_types(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.event_type.clone())
            .collect()
    }
    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

async fn insert_event(pool: &PgPool, event_type: &str) {
    sqlx::query("INSERT INTO outbox.outbox_events (event_type, payload) VALUES ($1, $2)")
        .bind(event_type)
        .bind(json!({}))
        .execute(pool)
        .await
        .unwrap();
}

/// Yield until `cond` holds or a generous deadline passes (keeps the test from
/// hanging if delivery breaks, without a fixed sleep on the happy path).
async fn wait_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..2000 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("condition not met before deadline");
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn drains_backlog_then_reacts_to_notify_in_order(pool: PgPool) {
    // Backlog: an event inserted BEFORE the dispatcher starts.
    insert_event(&pool, "backlog.event").await;

    let publisher = RecordingPublisher::default();
    let dispatcher = OutboxDispatcher::new(pool.clone());
    let cancel = CancellationToken::new();

    let dispatch = dispatcher.run(&publisher, cancel.clone());
    let driver = async {
        // Backlog is drained on start.
        wait_until(|| publisher.count() >= 1).await;
        // Live path: a new insert fires NOTIFY and wakes the dispatcher.
        insert_event(&pool, "live.event").await;
        wait_until(|| publisher.count() >= 2).await;
        cancel.cancel();
    };

    let (result, ()) = tokio::join!(dispatch, driver);
    result.unwrap();

    // Published both, backlog first (id order).
    assert_eq!(publisher.event_types(), vec!["backlog.event", "live.event"]);

    // Both rows are marked dispatched — nothing left undelivered.
    let undispatched: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox.outbox_events WHERE dispatched_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(undispatched, 0);
}

struct FailingPublisher;

#[async_trait::async_trait]
impl OutboxPublisher for FailingPublisher {
    async fn publish(&self, _record: &OutboxRecord) -> Result<(), PublishError> {
        Err(PublishError("hub unavailable".into()))
    }
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn publisher_failure_leaves_events_undispatched(pool: PgPool) {
    insert_event(&pool, "will.fail").await;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    // The backlog drain hits the failing publisher and the loop ends with Err;
    // nothing is marked dispatched, so the event re-delivers on the next run.
    let err = dispatcher
        .run(&FailingPublisher, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        jarvis_infra::dispatcher::DispatchError::Publish(_)
    ));

    let undispatched: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox.outbox_events WHERE dispatched_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(undispatched, 1);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn cancellation_stops_the_loop(pool: PgPool) {
    let publisher = RecordingPublisher::default();
    let dispatcher = OutboxDispatcher::new(pool.clone());
    let cancel = CancellationToken::new();
    cancel.cancel(); // already cancelled

    // Returns promptly with Ok despite no events.
    dispatcher.run(&publisher, cancel).await.unwrap();
    assert_eq!(publisher.count(), 0);
}
