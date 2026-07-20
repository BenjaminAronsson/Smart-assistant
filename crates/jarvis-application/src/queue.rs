//! Run queue for degraded mode (F1.7, docs/02 §5). When the provider is
//! unavailable, LLM-needing runs park in this FIFO queue and resume when the
//! provider recovers. Interactive runs are prioritized over background.
//!
//! The queue is transient (memory-only in F1.7); restart re-drives unfinished
//! runs from the checkpoint (NFR-05). Persistence lands in a later feature if
//! warranted by the UX (e.g. queue survives a graceful restart).

use crate::orchestrator::RunInput;
use jarvis_domain::run::Run;
use std::collections::VecDeque;

/// Priority class for queued runs. Interactive runs (from the active session)
/// are dequeued before background work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RunPriority {
    Background = 0,
    Interactive = 1,
}

/// A run waiting for the provider to become available.
#[derive(Debug, Clone)]
pub struct QueuedRun {
    pub run: Run,
    pub input: RunInput,
    pub priority: RunPriority,
}

/// FIFO queue: interactive runs first, then background FIFO. Capacity-bounded
/// to prevent unbounded memory growth when provider is stuck.
pub struct RunQueue {
    interactive: VecDeque<QueuedRun>,
    background: VecDeque<QueuedRun>,
    max_background: usize,
}

impl RunQueue {
    pub fn new(max_background: usize) -> Self {
        Self {
            interactive: VecDeque::new(),
            background: VecDeque::new(),
            max_background,
        }
    }

    /// Enqueue a run. Interactive runs are unlimited; background runs are capped
    /// and oldest background run is evicted if limit exceeded.
    pub fn enqueue(&mut self, run: Run, input: RunInput, priority: RunPriority) {
        let queued = QueuedRun {
            run,
            input,
            priority,
        };
        match priority {
            RunPriority::Interactive => {
                self.interactive.push_back(queued);
            }
            RunPriority::Background => {
                if self.background.len() >= self.max_background {
                    // Evict oldest background run
                    let _ = self.background.pop_front();
                }
                self.background.push_back(queued);
            }
        }
    }

    /// Dequeue the highest-priority run (interactive first, then background).
    pub fn dequeue(&mut self) -> Option<QueuedRun> {
        if let Some(run) = self.interactive.pop_front() {
            Some(run)
        } else {
            self.background.pop_front()
        }
    }

    /// Current queue depth (for observability).
    pub fn len(&self) -> usize {
        self.interactive.len() + self.background.len()
    }

    pub fn is_empty(&self) -> bool {
        self.interactive.is_empty() && self.background.is_empty()
    }
}
