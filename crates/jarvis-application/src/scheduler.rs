//! Bounded deferrable work scheduling (M4, docs/08 and docs/09 §5).
//!
//! The scheduler is deliberately provider-neutral: background work may run
//! only when the provider is healthy and the caller supplies an open quota
//! window. Persistence and adapter execution remain outer-layer concerns.

use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

use crate::health::HealthState;

pub const MAX_DEFERRED_WORK: usize = 100;
const MAX_WORK_ID_BYTES: usize = 128;

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
    let seconds = 2u64.saturating_pow(u32::from(attempts.min(10)));
    now.checked_add(Duration::from_secs(seconds)).unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
