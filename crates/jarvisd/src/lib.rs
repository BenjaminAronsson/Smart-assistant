#![deny(unsafe_code)]
//! axum host: REST routes, WS hub, auth, DI wiring, config, health
//! (docs/02 §3). Library so the binary stays thin and everything is testable.

pub mod api;
pub mod config;
pub mod observability;
