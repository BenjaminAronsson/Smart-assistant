#![deny(unsafe_code)]
//! Use cases, orchestrator state machine, context assembler, router, policy
//! engine, and the ports (traits) adapters implement (docs/02 §3).

pub mod model;
pub mod orchestrator;
pub mod ports;

#[cfg(any(test, feature = "fixtures"))]
pub mod testing;

#[cfg(test)]
mod orchestrator_tests;
