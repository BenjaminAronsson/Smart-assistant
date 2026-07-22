#![deny(unsafe_code)]
//! Entities, value types, `RunState`, risk tiers, grant types, budget types.
//! Pure logic, no I/O (docs/02 §3).

pub mod audit;
pub mod conversations;
pub mod grants;
pub mod identity;
pub mod ids;
pub mod location;
pub mod policy;
pub mod run;
pub mod secrecy;
pub mod synthesis;
pub mod tools;
