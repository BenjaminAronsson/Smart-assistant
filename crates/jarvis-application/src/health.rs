//! Provider health classification and state tracking (F1.7, docs/05 §2).
//! Maps ModelError → health state + reason code for degraded-mode detection.
//!
//! Health state machine:
//! - Healthy: no errors observed, run directly
//! - Degraded: rate-limited; runs may queue (future feature, treat as Healthy for F1.7)
//! - Unavailable: quota exhausted, auth failed, network down; LLM runs queue
//!
//! Pure domain layer: no dependencies on jarvis_contracts (the host converts
//! to wire types).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::model::{ModelError, ProfileId};

/// Health state for a provider (wire types defined in jarvis_contracts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    Healthy,
    Degraded,
    Unavailable,
}

/// Maps a model error to a health state and reason code.
pub fn classify(error: &ModelError) -> (HealthState, String) {
    match error {
        ModelError::Unavailable(msg) => {
            // Classify based on error prefixes set by adapters.
            if msg.starts_with("timeout:") {
                (HealthState::Unavailable, "timeout".to_owned())
            } else if msg.starts_with("network_error:") {
                (HealthState::Unavailable, "network_error".to_owned())
            } else if msg.starts_with("auth_failed:") {
                (HealthState::Unavailable, "auth_failed".to_owned())
            } else if msg.starts_with("quota_exhausted:") {
                (HealthState::Unavailable, "quota_exhausted".to_owned())
            } else {
                (HealthState::Unavailable, "unavailable".to_owned())
            }
        }
        ModelError::Malformed(_) => (HealthState::Degraded, "malformed".to_owned()),
    }
}

/// Provider health state tracked per profile (in-memory, transient in F1.7).
#[derive(Debug, Clone)]
struct HealthRecord {
    state: HealthState,
    reason: String,
}

impl HealthRecord {
    fn new(state: HealthState, reason: String) -> Self {
        Self { state, reason }
    }

    fn healthy() -> Self {
        Self {
            state: HealthState::Healthy,
            reason: String::new(),
        }
    }
}

/// Tracks health state per provider profile.
pub struct ProviderHealthTracker {
    records: Mutex<HashMap<String, HealthRecord>>,
}

impl ProviderHealthTracker {
    /// Create a new tracker, wrapped in Arc for shared ownership.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            records: Mutex::new(HashMap::new()),
        })
    }

    /// Record an error: update the provider's health state to Unavailable.
    pub fn record_error(&self, profile: &ProfileId, error: &ModelError) {
        let (state, reason) = classify(error);
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        records.insert(profile.0.clone(), HealthRecord::new(state, reason));
    }

    /// Mark a provider as healthy (called by polling loop when provider recovers).
    pub fn mark_healthy(&self, profile: &ProfileId) {
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        records.insert(profile.0.clone(), HealthRecord::healthy());
    }

    /// Get current health for a profile.
    pub fn get(&self, profile: &ProfileId) -> (HealthState, String) {
        let records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        records
            .get(&profile.0)
            .map(|r| (r.state, r.reason.clone()))
            .unwrap_or_else(|| (HealthState::Healthy, String::new()))
    }

    /// Get health for all known profiles.
    pub fn all(&self) -> Vec<(String, HealthState, Option<String>)> {
        let records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        records
            .iter()
            .map(|(id, r)| {
                let reason = if r.reason.is_empty() {
                    None
                } else {
                    Some(r.reason.clone())
                };
                (id.clone(), r.state, reason)
            })
            .collect()
    }
}

impl Default for ProviderHealthTracker {
    fn default() -> Self {
        // Default() should return the struct, not Arc; callers use new() for Arc
        panic!("use ProviderHealthTracker::new() for Arc<Self>")
    }
}
