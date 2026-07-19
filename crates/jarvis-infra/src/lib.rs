#![deny(unsafe_code)]
//! sqlx repositories, migrations, outbox, artifact CAS, keyring secret store,
//! OTel wiring (docs/02 §3).

pub mod audit;
pub mod db;
pub mod identity;
pub mod sessions;

/// Embedded migration stream (docs/04 §3); applied by ops (`sqlx migrate run`)
/// or tests, never implicitly by jarvisd (docs/02 §12: migrations run before
/// the daemon starts).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
