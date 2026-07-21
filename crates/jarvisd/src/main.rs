#![deny(unsafe_code)]
//! jarvisd entry point: config → telemetry → serve → graceful shutdown
//! (docs/02 §12). Cold start to healthy must stay < 2 s (NFR-15).

use std::sync::Arc;

use jarvis_adapters::claude_cli::ClaudeCliModel;
use jarvis_application::orchestrator::RunInput;
use jarvis_application::ports::{MessageStore, RunStore};
use jarvis_domain::conversations::MessageRole;
use jarvis_domain::ids::SessionId;
use jarvis_domain::run::Run;
use jarvis_infra::dispatcher::OutboxDispatcher;
use jarvisd::api::RunWiring;
use jarvisd::runs::{PassthroughAssembler, RunApi, RunEngine, SystemClock};
use jarvisd::ws::{WsHub, WsState};
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;

fn main() -> anyhow::Result<()> {
    let config = jarvisd::config::Config::load()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(config))
}

async fn run(config: jarvisd::config::Config) -> anyhow::Result<()> {
    let telemetry = jarvisd::observability::init(config.observability.otlp_endpoint.as_deref())?;

    // Unresolvable secret reference = config error = fail fast (docs/09 §1).
    // An unREACHABLE database is different: the lazy pool lets jarvisd start
    // degraded and the health probe reports it (docs/02 §12).
    let db_url = jarvisd::config::resolve_secret_ref(&config.database.url_secret)?;
    let pool = jarvis_infra::db::connect_lazy(db_url.expose(), config.database.max_connections)?;

    let identity = Arc::new(jarvis_infra::identity::PgIdentityStore::new(pool.clone()));
    let auth = jarvisd::auth::AuthState::bootstrap(identity).await;

    // Persistence adapters behind the application ports.
    let session_store = Arc::new(jarvis_infra::sessions::PgSessionStore::new(pool.clone()));
    let message_store = Arc::new(jarvis_infra::messages::PgMessageStore::new(pool.clone()));
    let run_store = Arc::new(jarvis_infra::runs::PgRunStore::new(pool.clone()));
    let event_log = Arc::new(jarvis_infra::events::PgEventLog::new(pool.clone()));
    let sessions = jarvisd::sessions::SessionApi::new(session_store.clone());

    // The WS hub is both the outbox publisher (committed domain events) and the
    // orchestrator's run-event sink (transient deltas).
    let hub = WsHub::new();

    // Two shutdown tokens so the outbox dispatcher outlives the runs it must
    // publish for: `serve_shutdown` stops the HTTP server and cancels in-flight
    // runs; the dispatcher only stops once those runs have drained.
    let serve_shutdown = CancellationToken::new();
    let dispatch_shutdown = CancellationToken::new();
    spawn_signal_listener(serve_shutdown.clone());

    // The human-approval seam (F2.5), shared by the REST surface (resolve) and
    // the orchestrator's tool plane (park), so both rendezvous on the same
    // pending-approval map.
    let approval_gate = jarvisd::approvals::JarvisApprovalGate::new(pool.clone());

    // The live tool plane (F2.6): a registry with every executor timeout-wrapped
    // at its single registration site (`jarvisd::tools`), the durable audit sink,
    // and the grant mint/validate ports. `fs.read` is left unregistered — no
    // configured root is the stricter default (no ambient filesystem authority).
    let grant_store = Arc::new(jarvis_infra::grants::PgGrantStore::new(pool.clone()));
    let mut registry = jarvisd::tools::build_registry(None)?;
    // MCP tool servers (F2.7): none configured in M2, so no ambient MCP tool
    // authority — the stricter default. `_mcp_hosts` must live for the process
    // lifetime: each registered MCP executor holds a peer into its child, and
    // dropping a host reaps that child. Held here in `run`'s scope until shutdown.
    let _mcp_hosts =
        jarvisd::tools::register_mcp_servers(&mut registry, Vec::new(), serve_shutdown.clone())
            .await?;
    let tool_plane = jarvisd::runs::ToolPlane {
        registry: Arc::new(registry),
        audit: Arc::new(jarvis_infra::audit_sink::PgAuditSink::new(pool.clone())),
        approval_gate: approval_gate.clone(),
        grant_minter: grant_store.clone(),
        grant_validator: grant_store,
    };

    let engine = RunEngine::new(
        Arc::new(ClaudeCliModel::with_config(
            "claude-cli",
            config.providers.claude_cli.to_adapter(),
        )),
        Arc::new(PassthroughAssembler),
        run_store.clone(),
        message_store.clone(),
        hub.clone(),
        Arc::new(SystemClock),
        serve_shutdown.clone(),
        Some(tool_plane),
    );

    let run_api = RunApi::new(
        session_store,
        message_store.clone(),
        run_store.clone(),
        event_log.clone(),
        engine.clone(),
        approval_gate,
    );
    let ws_state = WsState {
        hub: hub.clone(),
        events: event_log,
        shutdown: serve_shutdown.clone(),
    };

    // Start the event-driven outbox dispatcher (LISTEN/NOTIFY, not polling) and
    // re-drive any runs left unfinished by a previous crash (NFR-05).
    let dispatcher_task = tokio::spawn(run_dispatcher(
        pool.clone(),
        hub.clone(),
        dispatch_shutdown.clone(),
    ));
    recover_unfinished_runs(&engine, run_store.as_ref(), message_store.as_ref()).await;

    // Start the health polling loop (F1.7): periodically try to dequeue and
    // re-spawn runs when the provider recovers (minimal viable: no external checks).
    let polling_engine = engine.clone();
    let polling_shutdown = serve_shutdown.clone();
    let _polling_task = tokio::spawn(async move {
        poll_provider_health(polling_engine, polling_shutdown).await;
    });

    let state = jarvisd::api::AppState::with_database(pool, auth);
    let app = jarvisd::api::router_with(
        state,
        Some(sessions),
        Some(RunWiring {
            runs: run_api,
            ws: ws_state,
        }),
        config.server.web_assets.clone(),
    )
    .layer(
        TraceLayer::new_for_http().make_span_with(|req: &axum::http::Request<_>| {
            tracing::info_span!("http", method = %req.method(), path = %req.uri().path())
        }),
    );

    let listener = tokio::net::TcpListener::bind(config.bind_addr()).await?;
    tracing::info!(bind = %config.bind_addr(), "jarvisd listening");

    let cancel = serve_shutdown.clone();
    let serve =
        axum::serve(listener, app).with_graceful_shutdown(async move { cancel.cancelled().await });
    // Bounded drain (invariant 4): a wedged in-flight request must not block
    // shutdown — after the signal, connections get DRAIN_DEADLINE to finish.
    let deadline = async {
        serve_shutdown.cancelled().await;
        tokio::time::sleep(DRAIN_DEADLINE).await;
    };
    tokio::select! {
        result = serve => result?,
        _ = deadline => tracing::warn!("drain deadline exceeded; forcing exit"),
    }

    // Runs were signalled to cancel with `serve_shutdown`; wait (bounded) for
    // them to checkpoint their terminal state, THEN stop the dispatcher so those
    // final events are still published.
    let _ = tokio::time::timeout(DRAIN_DEADLINE, engine.drain()).await;
    dispatch_shutdown.cancel();
    let _ = tokio::time::timeout(DRAIN_DEADLINE, dispatcher_task).await;

    tracing::info!("jarvisd draining telemetry and exiting");
    telemetry.shutdown();
    Ok(())
}

/// Restart backoff so a persistent dispatcher failure cannot hot-loop (CPU +
/// log flood); short enough that recovery from a transient blip stays prompt.
const DISPATCH_RESTART_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// Run the outbox dispatcher, restarting it if a transient database/publish
/// error ends the loop; a cancelled `shutdown` ends it for good.
async fn run_dispatcher(pool: sqlx::PgPool, hub: Arc<WsHub>, shutdown: CancellationToken) {
    while !shutdown.is_cancelled() {
        let dispatcher = OutboxDispatcher::new(pool.clone());
        match dispatcher.run(&*hub, shutdown.clone()).await {
            Ok(()) => return, // cancelled
            Err(error) => {
                tracing::error!(%error, "outbox dispatcher stopped; restarting");
                // Back off before reconnecting, but wake immediately on shutdown.
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(DISPATCH_RESTART_BACKOFF) => {}
                }
            }
        }
    }
}

/// Health polling loop (F1.7): periodically attempt to dequeue and re-spawn runs.
/// For F1.7 minimal viable, we do not check external provider status; instead,
/// we assume recovery has happened if we successfully dequeue and re-spawn a run.
/// If the run succeeds, the provider is healthy; if it fails again, it re-queues
/// and we try again next interval.
async fn poll_provider_health(engine: Arc<RunEngine>, shutdown: CancellationToken) {
    while !shutdown.is_cancelled() {
        // Try to dequeue and re-spawn one run per interval
        if let Some((run, input)) = engine.try_dequeue() {
            tracing::debug!("dequeued and re-spawning run after provider recovery");
            // A requeued run carries no device identity → no tool authority
            // (invariant #1); it re-runs the model turn that failed on quota.
            engine.spawn(run, input, None);
        }
        // Wait for the next poll interval or shutdown
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(HEALTH_POLL_INTERVAL) => {}
        }
    }
}

/// Re-drive runs the previous process left mid-flight (NFR-05, docs/02 §12).
///
/// M1 has no external tool effects (invariant 1), so re-running the model
/// interaction is idempotent from the outside — the reconciliation is to restart
/// each run from its input rather than resume from a half-state with a lost model
/// stream. The input is the session's latest user message (M1 runs one exchange
/// at a time); a run whose input cannot be found re-drives with empty input and
/// completes trivially rather than hanging. Precise run→message linkage arrives
/// when runs reference their originating message (a later schema addition).
async fn recover_unfinished_runs(
    engine: &Arc<RunEngine>,
    runs: &dyn RunStore,
    messages: &dyn MessageStore,
) {
    let unfinished = match runs.load_unfinished().await {
        Ok(unfinished) => unfinished,
        Err(error) => {
            // A degraded start (DB unreachable) simply recovers nothing now; a
            // later restart re-runs this sweep (docs/02 §12).
            tracing::warn!(%error, "restart recovery skipped — runs unreadable");
            return;
        }
    };
    for run in unfinished {
        let text = latest_user_text(messages, &run.session_id).await;
        tracing::info!(run_id = %run.id, "re-driving unfinished run after restart");
        // Restart from the top: same id/session/budget, fresh Received state; the
        // durable row re-converges as the orchestrator re-checkpoints.
        let fresh = Run::new(run.id, run.session_id, run.budget);
        // A crash-recovered run has no device identity → no tool authority
        // (invariant #1); M1/M2 runs re-drive the model turn idempotently.
        engine.spawn(fresh, RunInput { text }, None);
    }
}

async fn latest_user_text(messages: &dyn MessageStore, session: &SessionId) -> String {
    messages
        .list_by_session(session, 100)
        .await
        .ok()
        .and_then(|msgs| {
            msgs.into_iter()
                .rev()
                .find(|m| m.role == MessageRole::User)
                .map(|m| m.text)
        })
        .unwrap_or_default()
}

/// Health polling interval (F1.7): check if queued runs can resume. For F1.7
/// minimal viable, this simply attempts to dequeue and re-spawn; the actual
/// provider health signal comes from whether the run succeeds or fails.
const HEALTH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

const DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

fn spawn_signal_listener(shutdown: CancellationToken) {
    // Deliberately untracked spawn: this listener's only effect is flipping
    // the cancellation token and its lifetime IS the process lifetime — there
    // is nothing to drain or join at shutdown (invariant 4 exemption).
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler install");
            tokio::select! {
                _ = ctrl_c => {},
                _ = term.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
        tracing::info!("shutdown signal received");
        shutdown.cancel();
    });
}
