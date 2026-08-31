//! Ports (docs/02 §3): traits the outer layers implement. The application
//! layer names capabilities; infra provides them. No sqlx/axum/provider
//! types may appear here (CLAUDE.md invariant 3, enforced by arch-test).
//!
//! Split by port area (F9.10); every item stays reachable at the exact same
//! `jarvis_application::ports::*` path it had as one 1,006-line file — the
//! re-export surface is the crate's public seam and three crates depend on
//! its shape.

pub use crate::calendar::{CalendarReader, CalendarReaderError};

mod artifacts;
mod audit;
mod automations;
mod display;
mod identity;
mod lists;
mod media;
mod memory;
mod runs;
mod sessions;
mod settings;
mod shared;
mod timers;

pub use artifacts::*;
pub use audit::*;
pub use automations::*;
pub use display::*;
pub use identity::*;
pub use lists::*;
pub use media::*;
pub use memory::*;
pub use runs::*;
pub use sessions::*;
pub use settings::*;
pub use shared::*;
pub use timers::*;
