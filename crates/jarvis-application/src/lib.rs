#![deny(unsafe_code)]
//! Use cases, orchestrator state machine, context assembler, router, policy
//! engine, and the ports (traits) adapters implement (docs/02 §3).

pub mod appbridge;
pub mod automations;
pub mod calendar;
pub mod deepdive;
pub mod deterministic;
pub mod evaluation;
pub mod health;
pub mod home;
pub mod lists;
pub mod location;
pub mod memory;
pub mod model;
pub mod nowplaying;
pub mod orchestrator;
pub mod policy;
pub mod ports;
pub mod queue;
pub mod scheduler;
pub mod timers;
pub mod transport;
pub mod voice;

#[cfg(any(test, feature = "fixtures"))]
pub mod testing;

#[cfg(test)]
mod adversarial_tests;

#[cfg(test)]
mod appbridge_tests;

#[cfg(test)]
mod approval_tests;

#[cfg(test)]
mod deepdive_tests;

#[cfg(test)]
mod orchestrator_tests;

#[cfg(test)]
mod policy_tests;

#[cfg(test)]
mod queue_tests;
