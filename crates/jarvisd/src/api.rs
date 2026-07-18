//! HTTP surface (docs/05 §1). At F0.5: the unauthenticated loopback health
//! endpoint only; sessions + auth arrive in F0.6–F0.8.

use axum::{Json, Router, extract::State, routing::get};
use jarvis_contracts::health::{AdapterHealth, AdapterState, HealthResponse, ServiceStatus};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// Shared per-request state. Adapter readiness is registered by whoever owns
/// the adapter (docs/02 §12: adapters register asynchronously and update
/// their state as it changes).
#[derive(Clone, Default)]
pub struct AppState {
    adapters: Arc<RwLock<BTreeMap<String, AdapterHealth>>>,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_adapter(&self, name: &str, state: AdapterState, detail: Option<String>) {
        // Poison recovery: the map is plain data — a panic elsewhere must not
        // wedge health reporting forever.
        self.adapters
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.to_owned(), AdapterHealth { state, detail });
    }

    fn health(&self) -> HealthResponse {
        let adapters = self
            .adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // Core is up if we can answer at all; any enabled adapter down =>
        // degraded mode, which keeps working deterministically (FR-12).
        // Exhaustive on purpose: a new AdapterState variant must force an
        // explicit decision here rather than silently reading as healthy.
        let status = if adapters.values().any(|a| match a.state {
            AdapterState::Down => true,
            AdapterState::Up | AdapterState::Disabled => false,
        }) {
            ServiceStatus::Degraded
        } else {
            ServiceStatus::Ok
        };
        HealthResponse {
            status,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            adapters,
        }
    }
}

pub fn router(state: AppState) -> Router {
    // Health is unauthenticated by design but loopback-only: config validation
    // rejects non-loopback binds until M7 (docs/06 §7), so no per-route guard
    // is needed yet. Revisit when remote binds become legal.
    Router::new()
        .route("/api/v1/diagnostics/health", get(health))
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(state.health())
}
