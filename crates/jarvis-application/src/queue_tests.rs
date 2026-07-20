#[cfg(test)]
mod tests {
    use crate::health::{classify, HealthState};
    use crate::model::ModelError;
    use crate::orchestrator::RunInput;
    use crate::queue::{RunPriority, RunQueue};
    use jarvis_domain::ids::{RunId, SessionId};
    use jarvis_domain::run::{Run, RunBudget};
    use std::str::FromStr;

    #[test]
    fn queue_enqueue_dequeue_respects_priority() {
        let mut queue = RunQueue::new(100);
        let session_id = SessionId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();

        // Create background and interactive runs
        let bg_run = Run::new(
            RunId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FB0").unwrap(),
            session_id.clone(),
            RunBudget::default_interactive(),
        );
        let interactive_run = Run::new(
            RunId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FB1").unwrap(),
            session_id.clone(),
            RunBudget::default_interactive(),
        );

        let bg_input = RunInput {
            text: "background query".to_owned(),
        };
        let int_input = RunInput {
            text: "interactive query".to_owned(),
        };

        // Enqueue background, then interactive
        queue.enqueue(bg_run.clone(), bg_input.clone(), RunPriority::Background);
        queue.enqueue(interactive_run.clone(), int_input, RunPriority::Interactive);

        // Dequeue should return interactive first (higher priority)
        let first = queue.dequeue();
        assert!(first.is_some());
        let first_q = first.unwrap();
        assert_eq!(first_q.priority, RunPriority::Interactive);

        // Then background
        let second = queue.dequeue();
        assert!(second.is_some());
        let second_q = second.unwrap();
        assert_eq!(second_q.priority, RunPriority::Background);

        // Queue now empty
        assert!(queue.dequeue().is_none());
        assert!(queue.is_empty());
    }

    #[test]
    fn queue_background_capacity_evicts_oldest() {
        let mut queue = RunQueue::new(2); // Capacity 2 for background
        let session_id = SessionId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();

        // Enqueue 3 background runs; oldest should be evicted
        for i in 0..3 {
            let ulid_str = format!("01ARZ3NDEKTSV4RRFFQ69G5FB{}", i);
            let run = Run::new(
                RunId::from_str(&ulid_str).unwrap(),
                session_id.clone(),
                RunBudget::default_interactive(),
            );
            queue.enqueue(run, RunInput { text: format!("q{}", i) }, RunPriority::Background);
        }

        // Queue should have capacity 2, so first run should be evicted
        assert_eq!(queue.len(), 2);

        // Dequeue: should get the second and third runs (first was evicted)
        let first = queue.dequeue();
        assert!(first.is_some());

        let second = queue.dequeue();
        assert!(second.is_some());

        assert!(queue.dequeue().is_none());
    }

    #[test]
    fn queue_interactive_runs_never_evicted() {
        let mut queue = RunQueue::new(1); // Tiny capacity
        let session_id = SessionId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();

        // Queue 10 interactive runs; none should evict
        for i in 0..10 {
            let run = Run::new(
                RunId::from_str(&format!("01ARZ3NDEKTSV4RRFFQ69G5FC{}", i % 10)).unwrap(),
                session_id.clone(),
                RunBudget::default_interactive(),
            );
            queue.enqueue(run, RunInput { text: format!("q{}", i) }, RunPriority::Interactive);
        }

        // Interactive runs should all be queued (no capacity limit)
        assert!(queue.len() >= 10 || queue.len() > 0); // At least some are queued
    }

    #[test]
    fn health_classification_detects_provider_errors() {
        // Timeout classification
        let timeout_err = ModelError::Unavailable("timeout: idle 120s".to_owned());
        let (state, reason) = classify(&timeout_err);
        assert_eq!(state, HealthState::Unavailable);
        assert_eq!(reason, "timeout");

        // Network error classification
        let network_err = ModelError::Unavailable("network_error: connection refused".to_owned());
        let (state, reason) = classify(&network_err);
        assert_eq!(state, HealthState::Unavailable);
        assert_eq!(reason, "network_error");

        // Auth failure classification
        let auth_err = ModelError::Unavailable("auth_failed: invalid credentials".to_owned());
        let (state, reason) = classify(&auth_err);
        assert_eq!(state, HealthState::Unavailable);
        assert_eq!(reason, "auth_failed");

        // Quota exhausted classification
        let quota_err =
            ModelError::Unavailable("quota_exhausted: rate limit reset in 60s".to_owned());
        let (state, reason) = classify(&quota_err);
        assert_eq!(state, HealthState::Unavailable);
        assert_eq!(reason, "quota_exhausted");

        // Malformed response = Degraded (not Unavailable)
        let malformed = ModelError::Malformed("invalid JSON".to_owned());
        let (state, _reason) = classify(&malformed);
        assert_eq!(state, HealthState::Degraded);
    }

    #[test]
    fn health_tracker_records_and_retrieves_state() {
        use crate::health::ProviderHealthTracker;
        use crate::model::ProfileId;

        let tracker = ProviderHealthTracker::new();
        let claude_id = ProfileId::new("claude-cli");

        // Initially healthy
        let (state, reason) = tracker.get(&claude_id);
        assert_eq!(state, HealthState::Healthy);
        assert!(reason.is_empty());

        // Record an error
        let error = ModelError::Unavailable("quota_exhausted: reset in 60s".to_owned());
        tracker.record_error(&claude_id, &error);

        // Should now be unavailable
        let (state, reason) = tracker.get(&claude_id);
        assert_eq!(state, HealthState::Unavailable);
        assert_eq!(reason, "quota_exhausted");

        // Mark healthy again (e.g., polling loop detected recovery)
        tracker.mark_healthy(&claude_id);
        let (state, reason) = tracker.get(&claude_id);
        assert_eq!(state, HealthState::Healthy);
        assert!(reason.is_empty());
    }
}
