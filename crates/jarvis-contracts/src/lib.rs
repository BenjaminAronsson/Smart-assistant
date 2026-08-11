#![deny(unsafe_code)]
//! Versioned wire DTOs, WS event envelope, JSON Schemas (docs/05).
//!
//! Everything here is the wire contract: serde + schemars derives, camelCase
//! field names, additive evolution only within a contract version (docs/05 §5).
//! TypeScript client types are generated from these schemas by
//! `cargo xtask codegen` — never hand-written twice.

pub mod appbridge;
pub mod approvals;
pub mod appspec;
pub mod artifacts;
pub mod auth;
pub mod cards;
pub mod content;
pub mod deepdive;
pub mod display;
pub mod envelope;
pub mod errors;
pub mod events;
pub mod health;
pub mod lists;
pub mod maps;
pub mod media;
pub mod memories;
pub mod messages;
pub mod providers;
pub mod runs;
pub mod schema;
pub mod sessions;
pub mod timeline;
pub mod timers;
pub mod voice;

/// Wire contract major version, carried as `v` on every WS envelope.
/// Breaking changes bump this with a dual-emit shim window (docs/05 §5).
pub const CONTRACT_VERSION: u16 = 1;
