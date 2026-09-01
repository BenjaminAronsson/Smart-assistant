//! The WebSocket hub and `/ws/v1` upgrade (docs/05 §1-§3). One
//! token-authenticated fan-out carries the owner's run events. Two producers
//! converge here:
//!
//! * committed **domain events** arrive via [`jarvis_infra::dispatcher::OutboxPublisher`] — the dispatcher
//!   calls us after commit. They are persisted and replayable; `seq` is the
//!   outbox row `id`, the same global cursor the timeline `since` uses.
//! * transient **text deltas** and voice recognition hypotheses arrive through
//!   bounded in-process streams. They are never persisted and never replayed.
//!
//! The hub owns every envelope field (docs/05 §3); payload authors never set
//! `seq`/`occurredAt`/etc. Run **state** changes are deliberately NOT emitted
//! through the sink — they are persisted by the checkpointer and delivered on
//! the outbox path, so the sink drops `StateChanged`/`Finished` to avoid the
//! double-emit the F1.4 review flagged.

mod hub;
mod replay;
mod sinks;
mod socket;
mod voice;

pub use hub::*;
pub use socket::*;

#[cfg(test)]
mod delivery_scope_tests;
#[cfg(test)]
mod speech_sensitivity_tests;
#[cfg(test)]
mod tests;
