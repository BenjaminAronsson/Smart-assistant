//! HTTP surface (docs/05 §1). Unauthenticated loopback health endpoint;
//! sessions + auth arrive in F0.7–F0.8.

use axum::{Json, Router, extract::State, routing::get};
use jarvis_contracts::health::{
    AdapterHealth, AdapterState, HealthResponse, ServiceStatus, UiSettingsDto,
};
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
    ui: Option<UiSettingsDto>,
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
            ui: None,
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

    /// Attach the non-sensitive `[ui]` presentation profile used by the HUD.
    pub fn with_ui_settings(mut self, settings: UiSettingsDto) -> Self {
        self.ui = Some(settings);
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
            ui: self.ui.clone(),
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

/// Which optional surfaces to mount. Each is absent by default: an unwired
/// surface serves no routes at all, which is the stricter default (a surface
/// that was not deliberately wired must not be reachable).
///
/// A struct rather than a positional argument list because the list only grows
/// — with six `Option`s of different types, a mis-ordered call site is a real
/// hazard, and every one of these mounts an authenticated surface.
pub struct Wiring {
    pub sessions: Option<crate::sessions::SessionApi>,
    pub runs: Option<RunWiring>,
    pub artifacts: Option<crate::artifacts::ArtifactApi>,
    /// The generated-app capability bridge (F6.5, docs/06 §6). `None` until the
    /// tool plane exists — without a registry there is nothing an app could be
    /// evaluated against, and a bridge that cannot evaluate must not exist
    /// rather than default to permitting.
    pub appbridge: Option<crate::appbridge::AppBridgeApi>,
    pub display: Option<crate::display::DisplayApi>,
    pub media: Option<crate::media::MediaApi>,
    /// Local PMTiles map serving (F3b.5, ADR-013). `None` when no archive is
    /// configured — the map routes are then absent, not empty (the client reads
    /// a 404 on coverage as "no local map" and takes the docs/12 §3 fallback).
    pub maps: Option<crate::maps::MapApi>,
    pub timers: Option<crate::timers::TimerApi>,
    /// Lists and quick notes (F3b.8, FR-34, ADR-024).
    pub lists: Option<crate::lists::ListApi>,
    /// Deep-dive threads (F3b.6, FR-27, ADR-017). The same handle is given to
    /// [`crate::runs::RunApi`], which routes the turn on the message path; this
    /// mounts the findings and promotion entry points.
    pub deepdive: Option<crate::deepdive::DeepDiveApi>,
    pub memories: Option<crate::memories::MemoryApi>,
    pub web_assets: Option<std::path::PathBuf>,
    /// The node-pairing window + challenge map (F7.2). Defaulted so a test
    /// that does not pair nodes needs no ceremony; `main` passes the same
    /// instance the daemon lives with.
    pub pairing: crate::pairing::PairingState,
    /// The TLS fingerprint a pairing node pins (F7.3); `None` on loopback.
    pub server_fingerprint: Option<String>,
    /// Whether the unauthenticated health endpoint may be served (F7.3).
    ///
    /// docs/05 §6.2 scopes it to "loopback only", which was free to honour
    /// while loopback was the only bind. Off loopback it becomes an
    /// unauthenticated readout of adapter state and — worse — of the bootstrap
    /// pairing code, so on a non-loopback listener health moves **behind
    /// authentication** rather than being served to the network.
    pub public_health: bool,
}

impl Default for Wiring {
    fn default() -> Self {
        Self {
            sessions: None,
            runs: None,
            artifacts: None,
            appbridge: None,
            display: None,
            media: None,
            maps: None,
            timers: None,
            lists: None,
            deepdive: None,
            memories: None,
            web_assets: None,
            pairing: crate::pairing::PairingState::default(),
            server_fingerprint: None,
            // Loopback is the default bind, so the default is the loopback
            // rule: health is public. `main` turns it off for any other bind.
            public_health: true,
        }
    }
}

pub fn router(state: AppState) -> Router {
    router_with(state, Wiring::default())
}

/// Full router: unauthenticated surface (loopback health + pair), the
/// authenticated session/run APIs + WebSocket hub behind the bearer middleware,
/// and optional static web assets (docs/03 §3: Angular built assets served by
/// jarvisd).
pub fn router_with(state: AppState, wiring: Wiring) -> Router {
    let Wiring {
        sessions,
        runs,
        artifacts,
        appbridge,
        display,
        media,
        maps,
        timers,
        lists,
        deepdive,
        memories,
        web_assets,
        pairing,
        server_fingerprint,
        public_health,
    } = wiring;
    // Health and pair are unauthenticated by design but loopback-only:
    // config validation rejects non-loopback binds until M7 (docs/06 §7).
    let mut router = Router::new();
    if public_health {
        router = router.route("/api/v1/diagnostics/health", get(health));
    }
    if let Some(auth) = &state.auth {
        router = router.route(
            "/api/v1/auth/pair",
            axum::routing::post(crate::auth::pair).with_state(auth.clone()),
        );
        // Node pairing (F7.2, ADR-031). The two node-facing steps are
        // UNAUTHENTICATED by necessity — a node has no token until it has
        // paired — and are bounded instead by the owner-opened window, its
        // lockout, the in-flight challenge cap, and a signature over a
        // single-use nonce. The window opener itself is owner-only and lives
        // on the protected router below.
        let pairing_api = crate::pairing::PairingApi {
            auth: auth.clone(),
            pairing: pairing.clone(),
            server_fingerprint: server_fingerprint.clone(),
        };
        router = router
            .route(
                "/api/v1/devices/pair",
                axum::routing::post(crate::pairing::start).with_state(pairing_api.clone()),
            )
            .route(
                "/api/v1/devices/pair/complete",
                axum::routing::post(crate::pairing::complete).with_state(pairing_api.clone()),
            );
        // One protected sub-router merges every authenticated surface (each
        // keeps its own typed state); the bearer middleware wraps them once.
        //
        // `protected` additionally requires the `ui` class scope — the owner's
        // surface, deny-by-default (F7.1). `node_reachable` is the explicit
        // carve-out: routes a paired satellite must reach, each added
        // deliberately by the feature that needs it.
        let mut protected = Router::new();
        let mut node_reachable = Router::new();
        if !public_health {
            // Still reachable, but only by an authenticated device.
            protected = protected.route(
                "/api/v1/diagnostics/health",
                get(health).with_state(state.clone()),
            );
        }
        // Device management (F7.1, FR-19). Authenticated like everything else
        // here, and additionally `ui`-scoped inside the handlers — a paired
        // room satellite must not be able to enumerate or revoke its siblings.
        protected = protected.merge(
            Router::new()
                .route(
                    "/api/v1/devices/pairing-window",
                    axum::routing::post(crate::pairing::open_window),
                )
                .with_state(pairing_api),
        );
        protected = protected.merge(
            Router::new()
                .route("/api/v1/devices", get(crate::devices::list))
                .route(
                    "/api/v1/devices/{id}/revoke",
                    axum::routing::post(crate::devices::revoke),
                )
                .with_state(auth.clone()),
        );
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
            protected = protected.merge(
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
            );
            // A node's whole purpose is this socket, so it is the one
            // authenticated route not gated on `ui`. What a given class may
            // *receive* on it is F7.4's per-connection filter; what it may
            // *send* is already scope-checked per frame.
            node_reachable = node_reachable.merge(
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
                    // F6.4: the one deliberately *renderable* artifact path —
                    // separate from the blob route, which stays attachment-only.
                    .route(
                        "/api/v1/apps/{id}/versions/{version}/document",
                        get(crate::artifacts::get_app_document),
                    )
                    .with_state(api),
            );
        }
        if let Some(api) = appbridge {
            protected = protected.merge(
                Router::new()
                    // F6.5: mint a short-lived, single-use capability token…
                    .route(
                        "/api/v1/apps/{id}/versions/{version}/capability-tokens",
                        axum::routing::post(crate::appbridge::mint_token),
                    )
                    // …and exchange it for exactly one operation, through
                    // `policy::evaluate` and a grant for R2+.
                    .route(
                        "/api/v1/apps/{id}/versions/{version}/invoke",
                        axum::routing::post(crate::appbridge::invoke),
                    )
                    .with_state(api),
            );
        }
        if let Some(api) = display {
            protected = protected.merge(
                Router::new()
                    .route(
                        "/api/v1/artifacts/{id}/open",
                        axum::routing::post(crate::display::open_artifact),
                    )
                    .with_state(api),
            );
        }
        if let Some(api) = timers {
            protected = protected.merge(
                Router::new()
                    .route(
                        "/api/v1/timers",
                        get(crate::timers::list).post(crate::timers::create),
                    )
                    .route(
                        "/api/v1/timers/{id}/action",
                        axum::routing::post(crate::timers::act),
                    )
                    .with_state(api),
            );
        }
        if let Some(api) = lists {
            protected = protected.merge(
                Router::new()
                    .route(
                        "/api/v1/lists",
                        get(crate::lists::index).post(crate::lists::create),
                    )
                    .route("/api/v1/lists/{id}", get(crate::lists::get))
                    .route(
                        "/api/v1/lists/{id}/items",
                        axum::routing::post(crate::lists::add_item),
                    )
                    .route(
                        "/api/v1/lists/{id}/items/{item_id}",
                        axum::routing::patch(crate::lists::check_item)
                            .delete(crate::lists::remove_item),
                    )
                    .route(
                        "/api/v1/lists/command",
                        axum::routing::post(crate::lists::command),
                    )
                    .route(
                        "/api/v1/lists/{id}/promote",
                        axum::routing::post(crate::lists::promote),
                    )
                    .with_state(api),
            );
        }
        if let Some(api) = deepdive {
            protected = protected.merge(
                Router::new()
                    .route(
                        "/api/v1/sessions/{id}/deepdive/findings",
                        axum::routing::post(crate::deepdive::record_findings),
                    )
                    .route(
                        "/api/v1/sessions/{id}/deepdive/promote",
                        axum::routing::post(crate::deepdive::promote),
                    )
                    .with_state(api),
            );
        }
        if let Some(api) = memories {
            protected = protected.merge(
                Router::new()
                    .route("/api/v1/memories", get(crate::memories::list))
                    .route(
                        "/api/v1/memories/{id}",
                        axum::routing::patch(crate::memories::patch)
                            .delete(crate::memories::forget),
                    )
                    .with_state(api),
            );
        }
        if let Some(api) = media {
            protected = protected.merge(
                Router::new()
                    .route("/api/v1/media/state", get(crate::media::get_state))
                    .route(
                        "/api/v1/media/command",
                        axum::routing::post(crate::media::post_command),
                    )
                    .with_state(api),
            );
        }
        if let Some(api) = maps {
            protected = protected.merge(
                Router::new()
                    .route("/api/v1/map/coverage", get(crate::maps::get_coverage))
                    .route("/api/v1/map/tiles/{z}/{x}/{y}", get(crate::maps::get_tile))
                    .with_state(api),
            );
        }
        // Order matters and is load-bearing: `require_device` must be the
        // OUTER layer, because the class gate reads the `DeviceContext` it
        // inserts. Applying the `ui` gate to `protected` before merging the
        // node-reachable routes is what keeps `/ws/v1` out of it.
        let authenticated = protected
            .layer(axum::middleware::from_fn_with_state(
                auth.clone(),
                crate::devices::require_owner_ui,
            ))
            .merge(node_reachable);
        router = router.merge(authenticated.layer(axum::middleware::from_fn_with_state(
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
