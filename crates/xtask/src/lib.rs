#![deny(unsafe_code)]
//! Dev-automation logic reachable from integration tests.
//!
//! `main.rs` stays the CLI surface; anything worth asserting on lives here.

pub mod dist;
