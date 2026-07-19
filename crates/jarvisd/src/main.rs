#![deny(unsafe_code)]
//! jarvisd entry point: config → telemetry → serve → graceful shutdown
//! (docs/02 §12). Cold start to healthy must stay < 2 s (NFR-15).

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
    let identity = std::sync::Arc::new(jarvis_infra::identity::PgIdentityStore::new(pool.clone()));
    let auth = jarvisd::auth::AuthState::bootstrap(identity).await;
    let sessions = jarvisd::sessions::SessionApi::new(std::sync::Arc::new(
        jarvis_infra::sessions::PgSessionStore::new(pool.clone()),
    ));
    let state = jarvisd::api::AppState::with_database(pool, auth);

    let app = jarvisd::api::router_with(
        state,
        Some(sessions),
        config.server.web_assets.clone(),
    )
    .layer(
        TraceLayer::new_for_http().make_span_with(|req: &axum::http::Request<_>| {
            tracing::info_span!("http", method = %req.method(), path = %req.uri().path())
        }),
    );

    let shutdown = CancellationToken::new();
    spawn_signal_listener(shutdown.clone());

    let listener = tokio::net::TcpListener::bind(config.bind_addr()).await?;
    tracing::info!(bind = %config.bind_addr(), "jarvisd listening");

    let cancel = shutdown.clone();
    let serve =
        axum::serve(listener, app).with_graceful_shutdown(async move { cancel.cancelled().await });
    // Bounded drain (invariant 4): a wedged in-flight request must not block
    // shutdown — after the signal, connections get DRAIN_DEADLINE to finish.
    let deadline = async {
        shutdown.cancelled().await;
        tokio::time::sleep(DRAIN_DEADLINE).await;
    };
    tokio::select! {
        result = serve => result?,
        _ = deadline => tracing::warn!("drain deadline exceeded; forcing exit"),
    }

    tracing::info!("jarvisd draining telemetry and exiting");
    telemetry.shutdown();
    Ok(())
}

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
