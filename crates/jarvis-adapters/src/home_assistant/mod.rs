//! Home Assistant curated tool layer (M5 F5.3/F5.4, FR-14, FR-28, docs/02 §10,
//! ADR-006, ADR-018).
//!
//! HA is the **home system of record**. This adapter is a client of it, never a
//! replacement: it holds a dedicated least-privilege long-lived token, caches
//! *entity metadata* (friendly name, area) and never live state, and exposes a
//! deliberately **curated** tool surface — `home.get_state`, `home.set_light`,
//! `home.set_area_lights`, `home.execute_scene`, `home.run_script` — rather than
//! a passthrough to HA's whole service namespace. The home keeps working when
//! Jarvis is down because nothing here is on HA's own control path.
//!
//! ## Why the tiers are what they are (docs/06 §3, re-read for M5)
//!
//! * `home.get_state` — **R0**. docs/06 §3 names "query HA state" as the
//!   canonical R0 example: read-only, automatic within scope, audited. It is
//!   still restricted to allowlisted entities, because a read of an arbitrary
//!   entity (a presence sensor, a camera's last-motion timestamp) is a privacy
//!   effect even though it mutates nothing.
//! * `home.set_light` — **R1**. docs/06 §3's own R1 example is literally
//!   "toggle a light": reversible, low impact, local. Reversibility is honest
//!   here and not merely asserted — the executor reads the entity's prior state
//!   from HA *before* mutating and registers the concrete undo with its result.
//!   If the prior state cannot be read, the call fails closed rather than
//!   performing an un-undoable "reversible" action.
//! * `home.execute_scene` / `home.run_script` — **R2**. A scene or script is a
//!   *set* of effects behind one name: its blast radius is not bounded by the
//!   entity the human sees, there is no per-entity undo, and a script can drive
//!   locks, notifications and automations. That is docs/06 §3's "meaningful
//!   mutation / change automation" row: explicit approval with the exact target
//!   and payload. They are not R3 — the targets are owner-authored and
//!   allowlisted, so this is not the destructive/financial/security tier — which
//!   also matches the tiering the M5 feature list fixed for this slice.
//!
//! ## Enforcing the allowlist when policy cannot see arguments
//!
//! `policy::evaluate` classifies a proposal by the *registered tool's*
//! [`ToolPolicy`] and never inspects arguments (the same constraint that forced
//! two tools for the M3a volume cap). An "only these entities" rule is therefore
//! **not** expressible as a policy tier. It is enforced twice inside this
//! module, both times before any HTTP request exists:
//!
//! 1. [`ToolExecutor::validate_args`] — runs on the human's *approved* (possibly
//!    edited) arguments before a grant is minted (CF-9), so a non-allowlisted
//!    entity never reaches an approval card's grant.
//! 2. [`ToolExecutor::execute`] — re-checks before touching the transport, so a
//!    direct invocation cannot skip step 1.
//!
//! The R2 tools additionally re-derive the grant's argument fingerprint (which
//! covers `entity_id`, a required argument) and check the grant's
//! `target_resource` against [`grant_target_resource`], so an approval for one
//! scene cannot execute another.
//!
//! ## Plural area commands (F5.4, FR-28, ADR-018)
//!
//! [`HomeSetAreaLightsTool`] turns "the living room lamps" into a concrete
//! entity **set** and drives it per entity. Three rules make that safe and
//! honest; each is argued at its implementation site:
//!
//! 1. **Resolution is over the allowlist, never over HA.** The candidate set is
//!    [`EntityAllowlist::lights`]; HA metadata only *filters* it by area. An
//!    entity the owner never allowlisted cannot become reachable by sharing a
//!    room with one that is.
//! 2. **The tier stays R1, with an in-executor fan-out cap.** See
//!    [`HomeSetAreaLightsTool::policy`] for the full argument — briefly: every
//!    member of the set is individually R1, the same actor can already reach the
//!    identical effect with N `home.set_light` calls, so an R2 gate here would
//!    buy friction rather than containment. What genuinely differs — fan-out —
//!    is bounded numerically by [`MAX_AREA_ENTITIES`].
//! 3. **The report is honest or it is an error.** A partial result says "2 of 3"
//!    and names the entity that failed; a total failure is an `Err`, never a
//!    partial success; an area that resolves to nothing is an `Err`, never a
//!    silent success. The undo is composed from the per-entity pre-reads, so it
//!    restores each light to *its own* prior state rather than blanket-off. A
//!    fan-out that runs long stops *itself* at [`AREA_FANOUT_BUDGET`] and still
//!    reports what it did, rather than being dropped mid-loop by the host
//!    timeout and audited as if nothing had happened (M5 audit S1).
//!
//! **Known limitation, stated rather than hidden.** `EntityMetadata::area` is
//! populated only where HA exposes `area_id` on state attributes; true registry
//! area membership needs HA's WebSocket registry API, which this adapter does
//! not speak. Lights with no area are counted and surfaced — either as a caveat
//! appended to a successful result, or as part of the refusal when nothing
//! resolved. They are never silently treated as "no match", because a command
//! that quietly does nothing is the dishonest failure mode ADR-018 exists to
//! prevent.

use std::time::Duration;

mod client;
mod tools;
mod wire;

pub use client::*;
pub use tools::*;
pub use wire::*;

/// Scope for the read tool. Held separately from control so a device may be
/// allowed to look without being allowed to touch.
pub(crate) const READ_SCOPE: &str = "home:read";
/// Scope for every mutating tool.
pub(crate) const CONTROL_SCOPE: &str = "home:control";

pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// One entity's state document. Generous for attribute-heavy entities, far
/// below anything that could pressure an 8 GB host (docs/09 §5).
pub(crate) const MAX_STATE_BYTES: usize = 128 * 1024;
/// The full `/api/states` document, used only for the metadata index.
pub(crate) const MAX_STATES_BYTES: usize = 4 * 1024 * 1024;
/// Entities kept in the metadata cache. Bounds resident memory against a
/// hostile or simply enormous HA instance.
pub(crate) const MAX_CACHED_ENTITIES: usize = 4096;
/// Entities parsed out of one `/api/states` response.
pub(crate) const MAX_PARSED_ENTITIES: usize = MAX_CACHED_ENTITIES;
/// Metadata staleness bound. HA stays authoritative for *state*; a rename takes
/// at most this long to be noticed.
pub(crate) const METADATA_TTL: Duration = Duration::from_secs(300);
/// Allowlist entries per category.
pub(crate) const MAX_ALLOWLIST_ENTRIES: usize = 512;
pub(crate) const MAX_ENTITY_ID_BYTES: usize = 128;
pub(crate) const MAX_TOKEN_BYTES: usize = 4096;
pub(crate) const MAX_FRIENDLY_NAME_CHARS: usize = 96;
pub(crate) const MAX_STATE_TEXT_CHARS: usize = 64;

#[cfg(test)]
mod tests;
