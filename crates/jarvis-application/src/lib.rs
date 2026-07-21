#![deny(unsafe_code)]
//! Use cases, orchestrator state machine, context assembler, router, policy
//! engine, and the ports (traits) adapters implement (docs/02 §3).

pub mod health;
pub mod location;
pub mod model;
pub mod orchestrator;
pub mod policy;
pub mod ports;
pub mod queue;

#[cfg(any(test, feature = "fixtures"))]
pub mod testing;

#[cfg(test)]
mod approval_tests;

#[cfg(test)]
mod orchestrator_tests;

#[cfg(test)]
mod policy_tests;

#[cfg(test)]
mod queue_tests;
