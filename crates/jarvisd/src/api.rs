//! HTTP surface (docs/05 §1). Unauthenticated loopback health endpoint;
//! sessions + auth arrive in F0.7–F0.8.

use axum::{Json, Router, extract::State, routing::get};
use jarvis_contracts::health::{AdapterHealth, AdapterState, HealthResponse, ServiceStatus};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// Shared per-request state. Adapter readiness is registered by whoever owns
/// the adapter (docs/02 §12: adapters register asynchronously and update
/// their state as it changes). The database is probed live per health
/// request — on-demand, never a background polling loop (docs/09 §5).
#[derive(Clone, Default)]
pub struct AppState {
    adapters: Arc<RwLock<BTreeMap<String, AdapterHealth>>>,
    db: Option<sqlx::PgPool>,
    auth: Option<crate::auth::AuthState>,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_database(pool: sqlx::PgPool, auth: crate::auth::AuthState) -> Self {
        Self {
            adapters: Arc::default(),
            db: Some(pool),
            auth: Some(auth),
        }
    }

    pub fn database(&self) -> Option<&sqlx::PgPool> {
        self.db.as_ref()
    }

    /// Attach auth without a database (tests use a fake IdentityStore).
    pub fn with_auth(mut self, auth: crate::auth::AuthState) -> Self {
        self.auth = Some(auth);
        self
    }

    pub fn set_adapter(&self, name: &str, state: AdapterState, detail: Option<String>) {
        // Poison recovery: the map is plain data — a panic elsewhere must not
        // wedge health reporting forever.
        self.adapters
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.to_owned(), AdapterHealth { state, detail });
    }

    async fn health(&self) -> HealthResponse {
        if let Some(pool) = &self.db {
            // Detail carries a STABLE reason code only — never raw driver
            // errors; this response is unauthenticated (docs/06 §5).
            match jarvis_infra::db::ping(pool).await {
                Ok(()) => self.set_adapter("database", AdapterState::Up, None),
                Err(reason) => {
                    self.set_adapter("database", AdapterState::Down, Some(reason.to_owned()))
                }
            }
        }
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
            // Deliberate (docs/05 §6): the bootstrap code is shown on the
            // loopback-only health page while the pairing window is open.
            pairing_code: self.auth.as_ref().and_then(|a| a.current_pairing_code()),
        }
    }
}

/// The authenticated run surface (docs/05 §1): the run REST endpoints and the
/// WebSocket hub, wired together (both share the engine + hub). Passed as a unit
/// so a caller cannot mount the REST routes without the matching WS route.
pub struct RunWiring {
    pub runs: crate::runs::RunApi,
    pub ws: crate::ws::WsState,
}

pub fn router(state: AppState) -> Router {
    router_with(state, None, None, None, None)
}

/// Full router: unauthenticated surface (loopback health + pair), the
/// authenticated session/run APIs + WebSocket hub behind the bearer middleware,
/// and optional static web assets (docs/03 §3: Angular built assets served by
/// jarvisd).
pub fn router_with(
    state: AppState,
    sessions: Option<crate::sessions::SessionApi>,
    runs: Option<RunWiring>,
    artifacts: Option<crate::artifacts::ArtifactApi>,
    web_assets: Option<std::path::PathBuf>,
) -> Router {
    // Health and pair are unauthenticated by design but loopback-only:
    // config validation rejects non-loopback binds until M7 (docs/06 §7).
    let mut router = Router::new().route("/api/v1/diagnostics/health", get(health));
    if let Some(auth) = &state.auth {
        router = router.route(
            "/api/v1/auth/pair",
            axum::routing::post(crate::auth::pair).with_state(auth.clone()),
        );
        // One protected sub-router merges every authenticated surface (each
        // keeps its own typed state); the bearer middleware wraps them once.
        let mut protected = Router::new();
        if let Some(api) = sessions {
            protected = protected.merge(
                Router::new()
                    .route(
                        "/api/v1/sessions",
                        axum::routing::post(crate::sessions::create).get(crate::sessions::list),
                    )
                    .route("/api/v1/sessions/{id}", get(crate::sessions::get))
                    .with_state(api),
            );
        }
        if let Some(RunWiring { runs, ws }) = runs {
            protected = protected
                .merge(
                    Router::new()
                        .route(
                            "/api/v1/sessions/{id}/messages",
                            axum::routing::post(crate::runs::submit_message),
                        )
                        .route(
                            "/api/v1/sessions/{id}/timeline",
                            get(crate::runs::get_timeline),
                        )
                        .route("/api/v1/runs/{id}", get(crate::runs::get_run))
                        .route(
                            "/api/v1/runs/{id}/cancel",
                            axum::routing::post(crate::runs::cancel_run),
                        )
                        .route(
                            "/api/v1/runs/{id}/approvals/{approval_id}",
                            axum::routing::post(crate::runs::resolve_approval),
                        )
                        .route("/api/v1/providers", get(crate::runs::get_providers))
                        .with_state(runs),
                )
                .merge(
                    Router::new()
                        .route("/ws/v1", get(crate::ws::ws_upgrade))
                        .with_state(ws),
                );
        }
        if let Some(api) = artifacts {
            protected = protected.merge(
                Router::new()
                    .route(
                        "/api/v1/artifacts/{id}/versions",
                        get(crate::artifacts::list_versions),
                    )
                    .route(
                        "/api/v1/artifacts/{id}/versions/{version}/blob",
                        get(crate::artifacts::get_blob),
                    )
                    .with_state(api),
            );
        }
        router = router.merge(protected.layer(axum::middleware::from_fn_with_state(
            auth.clone(),
            crate::auth::require_device,
        )));
    }
    if let Some(assets) = web_assets {
        // Unknown API paths must stay problem-body 404s — only non-API paths
        // fall through to the SPA (rust-reviewer F0.8 NIT-3).
        router = router.route("/api/{*rest}", axum::routing::any(api_not_found));
        let index = assets.join("index.html");
        router = router.fallback_service(
            tower_http::services::ServeDir::new(assets)
                .fallback(tower_http::services::ServeFile::new(index)),
        );
    }
    router.with_state(state)
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(state.health().await)
}

async fn api_not_found() -> axum::response::Response {
    crate::problem::problem(
        axum::http::StatusCode::NOT_FOUND,
        jarvis_contracts::errors::ErrorCode::ResourceNotFound,
        "unknown API route",
        None,
    )
}
