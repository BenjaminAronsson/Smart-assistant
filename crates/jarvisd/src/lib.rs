#![deny(unsafe_code)]
//! axum host: REST routes, WS hub, auth, DI wiring, config, health
//! (docs/02 §3). Library so the binary stays thin and everything is testable.

pub mod api;
pub mod appbridge;
pub mod approvals;
pub mod apptool;
pub mod artifacts;
pub mod auth;
pub mod automations;
pub mod cards;
pub mod config;
pub mod deepdive;
pub mod deferred;
pub mod devices;
pub mod diagnostics;
pub mod display;
pub mod light_targets;
pub mod lists;
pub mod location;
pub mod maps;
pub mod media;
pub mod memories;
pub mod observability;
pub mod pairing;
pub mod pmtiles;
pub mod policy_view;
pub mod problem;
pub mod runs;
pub mod sessions;
pub mod settings;
pub mod timers;
pub mod tls;
pub mod tools;
pub mod ws;
