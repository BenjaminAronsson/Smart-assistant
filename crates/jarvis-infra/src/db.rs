//! Connection pool construction. The URL arrives as an exposed secret from
//! `jarvisd` (resolved at the adapter boundary, invariant 5) and is consumed
//! here without logging.

use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;

/// Lazy pool: jarvisd starts degraded when Postgres is down (docs/02 §12)
/// instead of failing; readiness is observed via [`ping`].
pub fn connect_lazy(database_url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(2))
        .connect_lazy(database_url)
}

/// Health probe. Returns a STABLE reason code on failure ("unreachable"),
/// never raw driver error text — health details reach an unauthenticated
/// endpoint and must not leak connection internals (docs/06 §5).
pub async fn ping(pool: &PgPool) -> Result<(), &'static str> {
    match tokio::time::timeout(
        Duration::from_millis(800),
        sqlx::query("SELECT 1").execute(pool),
    )
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) | Err(_) => Err("unreachable"),
    }
}
