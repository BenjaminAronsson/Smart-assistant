#![deny(unsafe_code)]
//! Jarvis node agent, as a library.
//!
//! The binary is a thin `main` over these modules. They live in a lib target
//! for one reason: F8.1's evidence is "pairs against a **real** TLS listener
//! and refuses a changed fingerprint", and an integration test cannot reach
//! into a bin-only crate. A claim that can only be tested through unit tests of
//! its own parts is not the claim the feature makes.

pub mod audio;
pub mod cli;
pub mod client;
pub mod compositor;
pub mod handler;
pub mod http;
pub mod identity;
pub mod node_voice;
pub mod pairing;
pub mod pinning;
pub mod store;
