//! Bounded deferrable work scheduling (M4, docs/08 and docs/09 §5).
//!
//! The scheduler is deliberately provider-neutral: background work may run
//! only when the provider is healthy and the caller supplies an open quota
//! window. Persistence and adapter execution remain outer-layer concerns.

use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

use crate::health::HealthState;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub const MAX_DEFERRED_WORK: usize = 100;
const MAX_WORK_ID_BYTES: usize = 128;
const MAX_ATTEMPTS: u16 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredKind {
    Summarization,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredWork {
    pub id: String,
    pub kind: DeferredKind,
    pub not_before: SystemTime,
    pub attempts: u16,
}

impl DeferredWork {
    pub fn new(id: impl Into<String>, kind: DeferredKind, not_before: SystemTime) -> Self {
        let id = id.into();
        Self {
            id: id.chars().take(MAX_WORK_ID_BYTES).collect(),
            kind,
            not_before,
            attempts: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaWindow {
    pub opens_at: SystemTime,
    pub closes_at: SystemTime,
}

impl QuotaWindow {
    pub fn contains(&self, now: SystemTime) -> bool {
        now >= self.opens_at && now < self.closes_at
    }
}

pub struct DeferrableScheduler {
    work: VecDeque<DeferredWork>,
    capacity: usize,
}

/// The provider-neutral callback used by the deferred-work executor.
///
/// The application layer owns the work lifecycle; the runtime supplies the
/// callback that knows how to summarize a particular item. The callback must
/// honour `cancel` while doing provider I/O.
#[async_trait]
pub trait DeferredWorkHandler: Send + Sync {
    async fn handle(
        &self,
        work: DeferredWork,
        cancel: CancellationToken,
    ) -> Result<(), DeferredWorkError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeferredWorkError {
    #[error("deferred work was cancelled")]
    Cancelled,
    #[error("deferred work failed")]
    Failed,
}

/// A cancellable, single-flight executor for [`DeferrableScheduler`].
///
/// `run_once` is intentionally caller-driven: a daemon can call it from its
/// tracked worker loop and decide how to wake between attempts. At most one
/// callback is in flight, so background work cannot create an unbounded burst
/// of provider requests. A work item is removed only after its callback
/// succeeds; failures and cancellation are returned to the bounded queue.
pub struct DeferredWorkExecutor {
    scheduler: DeferrableScheduler,
}

impl DeferredWorkExecutor {
    pub fn new(capacity: usize) -> Self {
        Self {
            scheduler: DeferrableScheduler::new(capacity),
        }
    }

    pub fn enqueue(&mut self, work: DeferredWork) -> bool {
        self.scheduler.enqueue(work)
    }

    pub fn len(&self) -> usize {
        self.scheduler.len()
    }

    pub fn is_empty(&self) -> bool {
        self.scheduler.is_empty()
    }

    /// Consume one ready item and invoke the handler when quota is usable.
    ///
    /// Returns `Ok(true)` when a callback was run successfully, `Ok(false)`
    /// when health/quota/time/cancellation prevented a callback, and `Err`
    /// only when the callback failed after the item was safely requeued.
    pub async fn run_once<H: DeferredWorkHandler + ?Sized>(
        &mut self,
        now: SystemTime,
        health: HealthState,
        window: Option<QuotaWindow>,
        cancel: &CancellationToken,
        handler: &H,
    ) -> Result<bool, DeferredWorkError> {
        if cancel.is_cancelled() {
            return Ok(false);
        }

        let Some(work) = self.scheduler.pop_ready(now, health, window) else {
            return Ok(false);
        };
        let callback_cancel = cancel.child_token();

        if cancel.is_cancelled() {
            let _ = self.scheduler.enqueue(work);
            return Ok(false);
        }

        match handler.handle(work.clone(), callback_cancel).await {
            Ok(()) => Ok(true),
            Err(error) => {
                let mut retry = work;
                retry.attempts = retry.attempts.saturating_add(1).min(MAX_ATTEMPTS);
                retry.not_before = retry_after(now, retry.attempts);
                // The queue was just made smaller by pop_ready, so this cannot
                // normally fail; retain the explicit result for the bound.
                let _ = self.scheduler.enqueue(retry);
                Err(error)
            }
        }
    }
}

impl DeferrableScheduler {
    pub fn new(capacity: usize) -> Self {
        Self {
            work: VecDeque::new(),
            capacity: capacity.min(MAX_DEFERRED_WORK),
        }
    }

    pub fn enqueue(&mut self, work: DeferredWork) -> bool {
        if self.work.len() >= self.capacity {
            return false;
        }
        self.work.push_back(work);
        true
    }

    /// Select one ready item only when provider health and quota permit it.
    /// Items not yet due remain queued in FIFO order.
    pub fn pop_ready(
        &mut self,
        now: SystemTime,
        health: HealthState,
        window: Option<QuotaWindow>,
    ) -> Option<DeferredWork> {
        if health != HealthState::Healthy || !window.is_some_and(|w| w.contains(now)) {
            return None;
        }
        let index = self.work.iter().position(|item| item.not_before <= now)?;
        self.work.remove(index)
    }

    pub fn len(&self) -> usize {
        self.work.len()
    }

    pub fn is_empty(&self) -> bool {
        self.work.is_empty()
    }
}

pub fn retry_after(now: SystemTime, attempts: u16) -> SystemTime {
    let seconds = 2u64.saturating_pow(u32::from(attempts.min(MAX_ATTEMPTS)));
    now.checked_add(Duration::from_secs(seconds)).unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct RecordingHandler {
        seen: Arc<Mutex<Vec<String>>>,
        result: Result<(), DeferredWorkError>,
    }

    #[async_trait]
    impl DeferredWorkHandler for RecordingHandler {
        async fn handle(
            &self,
            work: DeferredWork,
            _cancel: CancellationToken,
        ) -> Result<(), DeferredWorkError> {
            self.seen.lock().unwrap().push(work.id);
            self.result.clone()
        }
    }

    fn t(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn work_waits_for_due_time_healthy_provider_and_open_window() {
        let mut scheduler = DeferrableScheduler::new(10);
        assert!(scheduler.enqueue(DeferredWork::new(
            "summary-1",
            DeferredKind::Summarization,
            t(10),
        )));
        let window = QuotaWindow {
            opens_at: t(5),
            closes_at: t(20),
        };
        assert!(
            scheduler
                .pop_ready(t(9), HealthState::Healthy, Some(window))
                .is_none()
        );
        assert!(
            scheduler
                .pop_ready(t(10), HealthState::Degraded, Some(window))
                .is_none()
        );
        assert_eq!(
            scheduler
                .pop_ready(t(10), HealthState::Healthy, Some(window))
                .unwrap()
                .id,
            "summary-1"
        );
    }

    #[test]
    fn queue_is_bounded_and_window_is_half_open() {
        let mut scheduler = DeferrableScheduler::new(1);
        assert!(scheduler.enqueue(DeferredWork::new("one", DeferredKind::Summarization, t(0),)));
        assert!(!scheduler.enqueue(DeferredWork::new("two", DeferredKind::Summarization, t(0),)));
        let window = QuotaWindow {
            opens_at: t(0),
            closes_at: t(1),
        };
        assert!(
            scheduler
                .pop_ready(t(1), HealthState::Healthy, Some(window))
                .is_none()
        );
    }

    #[tokio::test]
    async fn executor_consumes_ready_work_through_provider_neutral_handler() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handler = RecordingHandler {
            seen: seen.clone(),
            result: Ok(()),
        };
        let mut executor = DeferredWorkExecutor::new(2);
        assert!(executor.enqueue(DeferredWork::new(
            "summary-1",
            DeferredKind::Summarization,
            t(10),
        )));
        let window = QuotaWindow {
            opens_at: t(5),
            closes_at: t(20),
        };

        assert!(
            executor
                .run_once(
                    t(10),
                    HealthState::Healthy,
                    Some(window),
                    &CancellationToken::new(),
                    &handler
                )
                .await
                .unwrap()
        );
        assert_eq!(&*seen.lock().unwrap(), &["summary-1"]);
        assert_eq!(executor.len(), 0);
    }

    #[tokio::test]
    async fn executor_does_not_consume_during_unhealthy_or_cancelled_windows() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handler = RecordingHandler {
            seen: seen.clone(),
            result: Ok(()),
        };
        let mut executor = DeferredWorkExecutor::new(1);
        assert!(executor.enqueue(DeferredWork::new(
            "summary-1",
            DeferredKind::Summarization,
            t(0),
        )));
        let window = QuotaWindow {
            opens_at: t(0),
            closes_at: t(10),
        };
        let cancel = CancellationToken::new();

        assert!(
            !executor
                .run_once(
                    t(1),
                    HealthState::Unavailable,
                    Some(window),
                    &cancel,
                    &handler
                )
                .await
                .unwrap()
        );
        cancel.cancel();
        assert!(
            !executor
                .run_once(t(1), HealthState::Healthy, Some(window), &cancel, &handler)
                .await
                .unwrap()
        );
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(executor.len(), 1);
    }

    #[tokio::test]
    async fn executor_requeues_failed_work_with_backoff_and_attempt_count() {
        let handler = RecordingHandler {
            seen: Arc::new(Mutex::new(Vec::new())),
            result: Err(DeferredWorkError::Failed),
        };
        let mut executor = DeferredWorkExecutor::new(1);
        assert!(executor.enqueue(DeferredWork::new(
            "summary-1",
            DeferredKind::Summarization,
            t(0),
        )));
        let window = QuotaWindow {
            opens_at: t(0),
            closes_at: t(100),
        };

        assert_eq!(
            executor
                .run_once(
                    t(1),
                    HealthState::Healthy,
                    Some(window),
                    &CancellationToken::new(),
                    &handler
                )
                .await,
            Err(DeferredWorkError::Failed)
        );
        assert_eq!(executor.len(), 1);
        let retry = executor.scheduler.work.front().unwrap();
        assert_eq!(retry.attempts, 1);
        assert_eq!(retry.not_before, t(3));
        assert!(
            !executor
                .run_once(
                    t(2),
                    HealthState::Healthy,
                    Some(window),
                    &CancellationToken::new(),
                    &handler
                )
                .await
                .unwrap()
        );
    }
}
