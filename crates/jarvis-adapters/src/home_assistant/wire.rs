//! Entity identity and the allowlist (F9.5).

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use jarvis_domain::tools::ToolId;

use super::*;

// ---------------------------------------------------------------------------
// Entity identity
// ---------------------------------------------------------------------------

/// A Home Assistant entity id (`light.kitchen_lamp`).
///
/// Validation is a security control, not tidiness: an entity id is interpolated
/// into a REST path, so the accepted alphabet excludes `/`, `.` beyond the
/// single domain separator, `?`, `#`, `%` and every control character. A
/// traversal or query-smuggling attempt cannot be represented by this type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId(String);

/// Deliberately echoes nothing: the rejected text is untrusted and must not be
/// reflected into a log line or a model-visible error (invariant 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid home entity id")]
pub struct EntityIdParseError;

impl EntityId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The HA domain segment (`light`, `scene`, `script`, `sensor`, …).
    pub fn domain(&self) -> &str {
        self.0.split('.').next().unwrap_or_default()
    }
}

impl FromStr for EntityId {
    type Err = EntityIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || s.len() > MAX_ENTITY_ID_BYTES {
            return Err(EntityIdParseError);
        }
        let mut parts = s.split('.');
        let (Some(domain), Some(object_id), None) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(EntityIdParseError);
        };
        let valid = |seg: &str| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        };
        if valid(domain) && valid(object_id) {
            Ok(Self(s.to_owned()))
        } else {
            Err(EntityIdParseError)
        }
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The resource string a grant for a home mutation must cover. Exported so a
/// minting site and this executor's validation use one function rather than two
/// string literals that can drift apart (docs/06 §4).
///
/// It is the **tool id**, not an entity-scoped string. That is what a real grant
/// covers: the orchestrator mints `GrantBinding::target_resource` from the
/// proposal's tool id (`jarvis-application/src/orchestrator.rs`, the
/// `WaitingApproval` arm), and `ResourcePattern::matches` is exact string
/// equality for a pattern with no trailing `*`. An earlier version of this
/// function returned `home:{entity}`, which no minting site ever produces — so
/// **every approved `home.execute_scene` / `home.run_script` was denied at the
/// executor**, after `PgGrantStore::check_and_consume` had already burned the
/// single-use grant. The owner approved, the grant was spent, nothing happened,
/// and retrying needed a fresh approval. Silently breaking an approved action is
/// the worse failure, so the executor checks what is actually minted.
///
/// The **entity is still bound**: `entity_id` is a required argument of both R2
/// tools, so it is inside `normalized_args_sha256`, and a grant approved for one
/// scene fails the fingerprint check in [`check_grant`] when presented for
/// another.
pub fn grant_target_resource(tool_id: &ToolId) -> String {
    tool_id.as_str().to_owned()
}

// ---------------------------------------------------------------------------
// Allowlist
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AllowlistError {
    #[error("invalid home entity id")]
    InvalidEntityId,
    #[error("home allowlist entry is not in the `{0}` domain")]
    WrongDomain(&'static str),
    #[error("home allowlist is too large")]
    TooLarge,
}

/// The closed set of entities the curated tools may touch, per capability.
///
/// Empty means empty: an unconfigured Jarvis controls nothing. That is the
/// fail-closed reading — a missing allowlist is not "allow all".
///
/// F5.4: area resolution maps a spoken area onto a subset of
/// [`EntityAllowlist::lights`]; the allowlist stays the outer bound, so
/// expansion can never reach an entity that single-entity control could not.
#[derive(Debug, Clone, Default)]
pub struct EntityAllowlist {
    readable: BTreeSet<EntityId>,
    lights: BTreeSet<EntityId>,
    scenes: BTreeSet<EntityId>,
    scripts: BTreeSet<EntityId>,
}

impl EntityAllowlist {
    /// Parse and validate the four configured lists. A light entry must be in
    /// the `light` domain, a scene in `scene`, a script in `script` — so a
    /// misconfiguration that would let `home.set_light` drive a lock is a
    /// startup error, not a runtime surprise.
    pub fn new(
        readable: &[String],
        lights: &[String],
        scenes: &[String],
        scripts: &[String],
    ) -> Result<Self, AllowlistError> {
        Ok(Self {
            readable: parse_set(readable, None)?,
            lights: parse_set(lights, Some("light"))?,
            scenes: parse_set(scenes, Some("scene"))?,
            scripts: parse_set(scripts, Some("script"))?,
        })
    }

    /// Anything controllable is implicitly readable — approving a light you
    /// cannot then query would be a pointless asymmetry.
    pub fn is_readable(&self, entity: &EntityId) -> bool {
        self.readable.contains(entity)
            || self.lights.contains(entity)
            || self.scenes.contains(entity)
            || self.scripts.contains(entity)
    }

    pub fn allows_light(&self, entity: &EntityId) -> bool {
        entity.domain() == "light" && self.lights.contains(entity)
    }

    pub fn allows_scene(&self, entity: &EntityId) -> bool {
        entity.domain() == "scene" && self.scenes.contains(entity)
    }

    pub fn allows_script(&self, entity: &EntityId) -> bool {
        entity.domain() == "script" && self.scripts.contains(entity)
    }

    /// The resolvable light set for area expansion (F5.4). This is deliberately
    /// the *only* enumeration the area tool has: it can filter this set, never
    /// extend it.
    pub fn lights(&self) -> impl Iterator<Item = &EntityId> {
        self.lights.iter()
    }
}

fn parse_set(
    values: &[String],
    require_domain: Option<&'static str>,
) -> Result<BTreeSet<EntityId>, AllowlistError> {
    if values.len() > MAX_ALLOWLIST_ENTRIES {
        return Err(AllowlistError::TooLarge);
    }
    let mut set = BTreeSet::new();
    for value in values {
        let entity: EntityId = value.parse().map_err(|_| AllowlistError::InvalidEntityId)?;
        if let Some(domain) = require_domain
            && entity.domain() != domain
        {
            return Err(AllowlistError::WrongDomain(domain));
        }
        set.insert(entity);
    }
    Ok(set)
}
