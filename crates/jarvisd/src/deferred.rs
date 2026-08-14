//! The daemon driver for M4's deferrable work (closes D-M4-1, F8.7).
//!
//! `DeferrableScheduler` and `DeferredWorkExecutor` have existed since M4 and
//! **nothing called them** — the deviation carried forward through three
//! milestones. The scheduling logic was never the gap; a loop to turn it was.
//!
//! Deliberately shaped like `timers::run_scheduler` rather than inventing a
//! second pattern: wake, do at most one thing, log honestly, sleep. Two
//! properties matter and both come from the application layer, not from here:
//!
//! * **Single-flight.** `run_once` runs at most one callback, so background
//!   work cannot become a burst of provider requests while the owner is
//!   waiting on a foreground turn.
//! * **Health- and quota-gated.** Deferred work is the first thing that should
//!   stop when the provider is unhealthy or the quota window is shut — it is
//!   *deferrable* by definition, and the owner's live turn is not.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use jarvis_application::health::HealthState;
use jarvis_application::scheduler::{
    DeferredWorkError, DeferredWorkExecutor, DeferredWorkHandler, QuotaWindow,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// How often the worker looks for something to do.
///
/// Minutes, not seconds: this is background work by definition, and a tight
/// loop on an 8 GB laptop is exactly what docs/09 §5 forbids. A quota window
/// opening is not an event worth polling for at higher resolution than the
/// work itself is worth.
const IDLE_INTERVAL: Duration = Duration::from_secs(120);

/// A shorter wait after a successful item, so a backlog drains rather than
/// trickling one item every two minutes.
const BUSY_INTERVAL: Duration = Duration::from_secs(5);

/// What the daemon can tell the worker about right now.
pub trait DeferredContext: Send + Sync {
    fn health(&self) -> HealthState;
    /// The open quota window, if there is one. `None` means "not now".
    fn quota_window(&self, now: SystemTime) -> Option<QuotaWindow>;
}

/// Drives deferred work until shutdown.
pub async fn run_worker<C: DeferredContext, H: DeferredWorkHandler + ?Sized>(
    executor: Arc<Mutex<DeferredWorkExecutor>>,
    context: Arc<C>,
    handler: Arc<H>,
    shutdown: CancellationToken,
) {
    tracing::info!("deferred work worker started");
    while !shutdown.is_cancelled() {
        let now = SystemTime::now();
        let health = context.health();
        let window = context.quota_window(now);

        // The lock is held only across one item, never across the sleep: a
        // caller enqueueing work must not wait two minutes for the mutex.
        let outcome = {
            let mut executor = executor.lock().await;
            executor
                .run_once(now, health, window, &shutdown, handler.as_ref())
                .await
        };

        let interval = match outcome {
            Ok(true) => BUSY_INTERVAL,
            Ok(false) => IDLE_INTERVAL,
            Err(DeferredWorkError::Cancelled) => break,
            Err(error) => {
                // The item was requeued with a longer `not_before` by the
                // executor, so this is a report rather than a loss. Warned
                // rather than debugged: deferred work that silently never
                // succeeds is the failure mode this whole thing exists to
                // avoid being invisible.
                tracing::warn!(%error, "deferred work failed; it will be retried");
                IDLE_INTERVAL
            }
        };

        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(interval) => {}
        }
    }
    tracing::info!("deferred work worker stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_application::scheduler::{DeferredKind, DeferredWork};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Context {
        health: HealthState,
        open: bool,
    }

    impl DeferredContext for Context {
        fn health(&self) -> HealthState {
            self.health
        }
        fn quota_window(&self, now: SystemTime) -> Option<QuotaWindow> {
            self.open.then(|| QuotaWindow {
                opens_at: now - Duration::from_secs(60),
                closes_at: now + Duration::from_secs(3_600),
            })
        }
    }

    #[derive(Default)]
    struct Counting(AtomicUsize);

    #[async_trait::async_trait]
    impl DeferredWorkHandler for Counting {
        async fn handle(
            &self,
            _work: DeferredWork,
            _cancel: CancellationToken,
        ) -> Result<(), DeferredWorkError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn work(id: &str) -> DeferredWork {
        DeferredWork::new(id, DeferredKind::Summarization, SystemTime::UNIX_EPOCH)
    }

    /// The whole point of D-M4-1: enqueued work actually runs, because
    /// something finally turns the handle.
    #[tokio::test]
    async fn enqueued_work_runs_when_the_provider_is_healthy_and_quota_is_open() {
        let mut executor = DeferredWorkExecutor::new(10);
        executor.enqueue(work("a"));
        executor.enqueue(work("b"));
        let executor = Arc::new(Mutex::new(executor));
        let handler = Arc::new(Counting::default());
        let shutdown = CancellationToken::new();

        let task = tokio::spawn(run_worker(
            executor.clone(),
            Arc::new(Context {
                health: HealthState::Healthy,
                open: true,
            }),
            handler.clone(),
            shutdown.clone(),
        ));

        // BUSY_INTERVAL between items, so both drain quickly.
        for _ in 0..100 {
            if executor.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;

        assert_eq!(handler.0.load(Ordering::SeqCst), 2);
        assert!(executor.lock().await.is_empty());
    }

    /// Deferred work is the *first* thing that should stop when the provider is
    /// struggling — it is deferrable by definition, and the owner's live turn
    /// is not.
    #[tokio::test]
    async fn nothing_runs_while_the_provider_is_unavailable() {
        let mut executor = DeferredWorkExecutor::new(10);
        executor.enqueue(work("a"));
        let executor = Arc::new(Mutex::new(executor));
        let handler = Arc::new(Counting::default());
        let shutdown = CancellationToken::new();

        let task = tokio::spawn(run_worker(
            executor.clone(),
            Arc::new(Context {
                health: HealthState::Unavailable,
                open: true,
            }),
            handler.clone(),
            shutdown.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;

        assert_eq!(handler.0.load(Ordering::SeqCst), 0);
        // Retained, not dropped: it runs when the provider recovers.
        assert_eq!(executor.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn nothing_runs_while_the_quota_window_is_shut() {
        let mut executor = DeferredWorkExecutor::new(10);
        executor.enqueue(work("a"));
        let executor = Arc::new(Mutex::new(executor));
        let handler = Arc::new(Counting::default());
        let shutdown = CancellationToken::new();

        let task = tokio::spawn(run_worker(
            executor.clone(),
            Arc::new(Context {
                health: HealthState::Healthy,
                open: false,
            }),
            handler.clone(),
            shutdown.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;

        assert_eq!(handler.0.load(Ordering::SeqCst), 0);
        assert_eq!(executor.lock().await.len(), 1);
    }

    /// Shutdown must be prompt (invariant 4) — not "after the idle interval".
    #[tokio::test]
    async fn shutdown_ends_the_worker_promptly() {
        let executor = Arc::new(Mutex::new(DeferredWorkExecutor::new(10)));
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run_worker(
            executor,
            Arc::new(Context {
                health: HealthState::Healthy,
                open: true,
            }),
            Arc::new(Counting::default()),
            shutdown.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the worker must not wait out its idle interval to stop")
            .expect("joins");
    }
}
