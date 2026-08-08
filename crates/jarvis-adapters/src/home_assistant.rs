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
//!    restores each light to *its own* prior state rather than blanket-off.
//!
//! **Known limitation, stated rather than hidden.** `EntityMetadata::area` is
//! populated only where HA exposes `area_id` on state attributes; true registry
//! area membership needs HA's WebSocket registry API, which this adapter does
//! not speak. Lights with no area are counted and surfaced — either as a caveat
//! appended to a successful result, or as part of the refusal when nothing
//! resolved. They are never silently treated as "no match", because a command
//! that quietly does nothing is the dishonest failure mode ADR-018 exists to
//! prevent.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{
    CanonicalValue, ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion, canonical_form,
    sanitize_result_content,
};
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256 as Sha2};
use tokio_util::sync::CancellationToken;

/// Scope for the read tool. Held separately from control so a device may be
/// allowed to look without being allowed to touch.
const READ_SCOPE: &str = "home:read";
/// Scope for every mutating tool.
const CONTROL_SCOPE: &str = "home:control";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// One entity's state document. Generous for attribute-heavy entities, far
/// below anything that could pressure an 8 GB host (docs/09 §5).
const MAX_STATE_BYTES: usize = 128 * 1024;
/// The full `/api/states` document, used only for the metadata index.
const MAX_STATES_BYTES: usize = 4 * 1024 * 1024;
/// Entities kept in the metadata cache. Bounds resident memory against a
/// hostile or simply enormous HA instance.
const MAX_CACHED_ENTITIES: usize = 4096;
/// Entities parsed out of one `/api/states` response.
const MAX_PARSED_ENTITIES: usize = MAX_CACHED_ENTITIES;
/// Metadata staleness bound. HA stays authoritative for *state*; a rename takes
/// at most this long to be noticed.
const METADATA_TTL: Duration = Duration::from_secs(300);
/// Allowlist entries per category.
const MAX_ALLOWLIST_ENTRIES: usize = 512;
const MAX_ENTITY_ID_BYTES: usize = 128;
const MAX_TOKEN_BYTES: usize = 4096;
const MAX_FRIENDLY_NAME_CHARS: usize = 96;
const MAX_STATE_TEXT_CHARS: usize = 64;

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

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Every variant is deliberately generic. No HA response body, URL, status
/// line, or token fragment is ever carried here: these strings reach logs, the
/// model's observation, and the run timeline (invariant 5, docs/06 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HomeAssistantError {
    #[error("home assistant configuration is invalid")]
    InvalidConfiguration,
    #[error("home assistant is unavailable")]
    Unavailable,
    #[error("home assistant rejected the request")]
    Rejected,
    #[error("home assistant does not know that entity")]
    UnknownEntity,
    #[error("home assistant response was invalid")]
    InvalidResponse,
    #[error("home assistant response exceeded the size limit")]
    ResponseTooLarge,
    #[error("home assistant request was cancelled")]
    Cancelled,
}

impl From<HomeAssistantError> for ToolError {
    fn from(error: HomeAssistantError) -> Self {
        match error {
            HomeAssistantError::Cancelled => ToolError::Cancelled,
            other => ToolError::ExecutionFailed(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Transport seam
// ---------------------------------------------------------------------------

/// The closed set of HA services the curated layer can invoke.
///
/// This is the structural form of "never the whole service namespace"
/// (docs/02 §10): there is no code path that turns a caller-supplied string
/// into a `domain/service` pair, so no argument, however hostile, can reach
/// `lock.unlock` or `homeassistant.restart`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CuratedService {
    LightTurnOn,
    LightTurnOff,
    SceneTurnOn,
    ScriptTurnOn,
}

impl CuratedService {
    pub fn domain(self) -> &'static str {
        match self {
            Self::LightTurnOn | Self::LightTurnOff => "light",
            Self::SceneTurnOn => "scene",
            Self::ScriptTurnOn => "script",
        }
    }

    pub fn service(self) -> &'static str {
        match self {
            Self::LightTurnOn | Self::SceneTurnOn | Self::ScriptTurnOn => "turn_on",
            Self::LightTurnOff => "turn_off",
        }
    }
}

/// The only three HA REST operations this adapter performs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeRequest {
    /// `GET /api/states` — metadata index only.
    AllStates,
    /// `GET /api/states/{entity_id}` — live, authoritative state.
    State(EntityId),
    /// `POST /api/services/{domain}/{service}` with `{"entity_id": …}`.
    Service {
        service: CuratedService,
        entity: EntityId,
    },
}

impl HomeRequest {
    fn max_bytes(&self) -> usize {
        match self {
            Self::AllStates => MAX_STATES_BYTES,
            Self::State(_) | Self::Service { .. } => MAX_STATE_BYTES,
        }
    }
}

/// The network boundary, kept behind a trait so every test in this module runs
/// against a fixture and never touches a socket (CLAUDE.md: fixture-driven tests
/// over live-provider calls, always). Implementations must return the generic
/// [`HomeAssistantError`] values — provider detail stops here.
#[async_trait]
pub trait HomeAssistantTransport: Send + Sync {
    async fn send(
        &self,
        request: HomeRequest,
        cancel: CancellationToken,
    ) -> Result<String, HomeAssistantError>;
}

/// Connection settings for the dedicated least-privilege HA token.
///
/// The token is resolved from the keyring by the host and handed over already
/// in plaintext; this type never resolves, logs, or re-exports it. There is
/// **no `Debug` derive** — the same deliberate omission as `SmtpConfig` and
/// `CalDavConfig` — so a `{:?}` on a config cannot spill the credential.
pub struct HomeAssistantConfig {
    base_url: Url,
    token: String,
}

impl HomeAssistantConfig {
    /// `base_url` must be `https` (docs/06 §7: the HA credential is a bearer
    /// token, so the link carries it on every request). A LAN instance served
    /// over plain http is rejected rather than silently downgraded.
    pub fn new(base_url: &str, token: impl Into<String>) -> Result<Self, HomeAssistantError> {
        let mut url: Url = base_url
            .parse()
            .map_err(|_| HomeAssistantError::InvalidConfiguration)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(HomeAssistantError::InvalidConfiguration);
        }
        // Normalize to a directory path so `join` appends rather than replaces
        // the last segment.
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }

        let token = token.into();
        // A token with a control character or whitespace would be a header
        // injection vector; an empty one is a misconfiguration.
        if token.is_empty()
            || token.len() > MAX_TOKEN_BYTES
            || token.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(HomeAssistantError::InvalidConfiguration);
        }
        Ok(Self {
            base_url: url,
            token,
        })
    }
}

/// The production transport: bearer-authenticated HTTPS with a request timeout,
/// no redirect following (a redirect would re-send the token to another origin),
/// a response byte cap, and cancellation honored while both connecting and
/// streaming the body.
pub struct RestTransport {
    client: reqwest::Client,
    config: HomeAssistantConfig,
}

impl RestTransport {
    pub fn new(config: HomeAssistantConfig) -> Result<Self, HomeAssistantError> {
        let client = reqwest::Client::builder()
            .connect_timeout(REQUEST_TIMEOUT)
            // A 302 to an attacker origin would otherwise leak the bearer token.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| HomeAssistantError::InvalidConfiguration)?;
        Ok(Self { client, config })
    }

    fn route(&self, request: &HomeRequest) -> Result<(Url, Option<String>), HomeAssistantError> {
        let (path, body) = match request {
            HomeRequest::AllStates => ("api/states".to_owned(), None),
            HomeRequest::State(entity) => (format!("api/states/{entity}"), None),
            HomeRequest::Service { service, entity } => (
                format!("api/services/{}/{}", service.domain(), service.service()),
                Some(serde_json::json!({ "entity_id": entity.as_str() }).to_string()),
            ),
        };
        let url = self
            .config
            .base_url
            .join(&path)
            .map_err(|_| HomeAssistantError::InvalidConfiguration)?;
        // Defence in depth: `EntityId` cannot contain a path escape, but assert
        // the derived URL never left the configured origin anyway.
        if url.scheme() != self.config.base_url.scheme()
            || url.host_str() != self.config.base_url.host_str()
            || url.port_or_known_default() != self.config.base_url.port_or_known_default()
        {
            return Err(HomeAssistantError::InvalidConfiguration);
        }
        Ok((url, body))
    }
}

#[async_trait]
impl HomeAssistantTransport for RestTransport {
    async fn send(
        &self,
        request: HomeRequest,
        cancel: CancellationToken,
    ) -> Result<String, HomeAssistantError> {
        if cancel.is_cancelled() {
            return Err(HomeAssistantError::Cancelled);
        }
        let max_bytes = request.max_bytes();
        let (url, body) = self.route(&request)?;
        let builder = match body {
            Some(body) => self
                .client
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body),
            None => self.client.get(url),
        }
        .bearer_auth(&self.config.token);

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(HomeAssistantError::Cancelled),
            result = tokio::time::timeout(REQUEST_TIMEOUT, builder.send()) => {
                result
                    .map_err(|_| HomeAssistantError::Unavailable)?
                    .map_err(|_| HomeAssistantError::Unavailable)?
            }
        };
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(HomeAssistantError::UnknownEntity);
        }
        if !status.is_success() {
            // Status code and body are deliberately dropped: an HA error body
            // can echo entity names and integration detail.
            tracing::warn!(target: "jarvis.home", "home assistant rejected a request");
            return Err(HomeAssistantError::Rejected);
        }
        tokio::time::timeout(REQUEST_TIMEOUT, read_bounded(response, max_bytes, &cancel))
            .await
            .map_err(|_| HomeAssistantError::Unavailable)?
    }
}

/// Accumulates a response body under a hard byte cap. Kept as its own type so
/// the bound is unit-testable without a socket.
struct BoundedBody {
    max_bytes: usize,
    bytes: Vec<u8>,
}

impl BoundedBody {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            bytes: Vec::new(),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<(), HomeAssistantError> {
        if self.bytes.len().saturating_add(chunk.len()) > self.max_bytes {
            return Err(HomeAssistantError::ResponseTooLarge);
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    fn into_string(self) -> Result<String, HomeAssistantError> {
        String::from_utf8(self.bytes).map_err(|_| HomeAssistantError::InvalidResponse)
    }
}

async fn read_bounded(
    mut response: reqwest::Response,
    max_bytes: usize,
    cancel: &CancellationToken,
) -> Result<String, HomeAssistantError> {
    // Reject an oversized body from its advertised length before allocating.
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(HomeAssistantError::ResponseTooLarge);
    }
    let mut body = BoundedBody::new(max_bytes);
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(HomeAssistantError::Cancelled),
            result = response.chunk() => result.map_err(|_| HomeAssistantError::Unavailable)?,
        };
        let Some(chunk) = chunk else { break };
        body.push(&chunk)?;
    }
    body.into_string()
}

// ---------------------------------------------------------------------------
// Client: live state + cached metadata
// ---------------------------------------------------------------------------

/// Cacheable, non-volatile facts about an entity. Explicitly **not** state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityMetadata {
    pub entity_id: EntityId,
    /// Sanitized: HA content is data, never markup or control sequences.
    pub friendly_name: String,
    /// `Some` only where HA exposes `area_id` on the state attributes. Full
    /// area membership needs HA's WebSocket registry API, which this adapter
    /// does not speak — so `None` means "unknown", never "no area", and F5.4
    /// reports the difference instead of hiding it.
    pub area: Option<String>,
}

impl EntityMetadata {
    /// The approval-card and result label: friendly name **and** entity id
    /// together, as docs/02 §10 requires.
    pub fn label(&self) -> String {
        format!("{} ({})", self.friendly_name, self.entity_id)
    }
}

/// A live read: state plus the metadata that came with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityState {
    pub metadata: EntityMetadata,
    pub state: String,
}

struct CacheEntry {
    metadata: EntityMetadata,
    stored_at: Instant,
}

#[derive(Default)]
struct MetadataCache {
    entries: BTreeMap<EntityId, CacheEntry>,
}

impl MetadataCache {
    fn get(&self, entity: &EntityId, now: Instant) -> Option<EntityMetadata> {
        self.entries
            .get(entity)
            .filter(|entry| now.duration_since(entry.stored_at) < METADATA_TTL)
            .map(|entry| entry.metadata.clone())
    }

    fn put(&mut self, metadata: EntityMetadata, now: Instant) {
        if self.entries.len() >= MAX_CACHED_ENTITIES
            && !self.entries.contains_key(&metadata.entity_id)
        {
            // Bounded: evict the oldest rather than growing without limit.
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.stored_at)
                .map(|(id, _)| id.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            metadata.entity_id.clone(),
            CacheEntry {
                metadata,
                stored_at: now,
            },
        );
    }
}

/// The HA client the curated tools share.
///
/// The cache/authority split is the whole point of this type: [`Self::state`]
/// **always** performs a live read, while [`Self::metadata`] may answer from
/// cache. There is no method that can return a cached `state` — the cached
/// struct does not even have the field.
pub struct HomeAssistantClient {
    transport: Arc<dyn HomeAssistantTransport>,
    cache: Mutex<MetadataCache>,
}

impl HomeAssistantClient {
    pub fn new(config: HomeAssistantConfig) -> Result<Self, HomeAssistantError> {
        Ok(Self::with_transport(Arc::new(RestTransport::new(config)?)))
    }

    /// Test//fixture seam: swap the network for a scripted transport.
    pub fn with_transport(transport: Arc<dyn HomeAssistantTransport>) -> Self {
        Self {
            transport,
            cache: Mutex::new(MetadataCache::default()),
        }
    }

    /// Live, authoritative state. Never served from the cache — a stale "is the
    /// oven off?" answer is exactly the failure docs/02 §10 forbids. Metadata
    /// learned on the way is cached as a side effect.
    pub async fn state(
        &self,
        entity: &EntityId,
        cancel: &CancellationToken,
    ) -> Result<EntityState, HomeAssistantError> {
        let body = self
            .transport
            .send(HomeRequest::State(entity.clone()), cancel.clone())
            .await?;
        let raw: RawState =
            serde_json::from_str(&body).map_err(|_| HomeAssistantError::InvalidResponse)?;
        let state = raw.into_entity_state(Some(entity))?;
        self.store(state.metadata.clone());
        Ok(state)
    }

    /// Metadata (friendly name, area). Answered from the cache while fresh,
    /// otherwise refreshed from HA. Safe to cache precisely because none of it
    /// is volatile.
    pub async fn metadata(
        &self,
        entity: &EntityId,
        cancel: &CancellationToken,
    ) -> Result<EntityMetadata, HomeAssistantError> {
        if let Some(cached) = self.cached(entity) {
            return Ok(cached);
        }
        Ok(self.state(entity, cancel).await?.metadata)
    }

    /// Build the whole entity/area index in one request (the F5.4 resolution
    /// input). Bounded in both bytes (by the transport) and entity count (here).
    pub async fn refresh_metadata(
        &self,
        cancel: &CancellationToken,
    ) -> Result<usize, HomeAssistantError> {
        let body = self
            .transport
            .send(HomeRequest::AllStates, cancel.clone())
            .await?;
        let raw: Vec<RawState> =
            serde_json::from_str(&body).map_err(|_| HomeAssistantError::InvalidResponse)?;
        if raw.len() > MAX_PARSED_ENTITIES {
            return Err(HomeAssistantError::ResponseTooLarge);
        }
        let mut stored = 0usize;
        for entry in raw {
            // A malformed or unparseable entry is skipped, not fatal: one odd
            // entity must not blind Jarvis to the rest of the house.
            if let Ok(state) = entry.into_entity_state(None) {
                self.store(state.metadata);
                stored += 1;
            }
        }
        tracing::debug!(target: "jarvis.home", entities = stored, "refreshed home metadata");
        Ok(stored)
    }

    async fn call_service(
        &self,
        service: CuratedService,
        entity: &EntityId,
        cancel: &CancellationToken,
    ) -> Result<(), HomeAssistantError> {
        self.transport
            .send(
                HomeRequest::Service {
                    service,
                    entity: entity.clone(),
                },
                cancel.clone(),
            )
            .await
            .map(|_| ())
    }

    fn cached(&self, entity: &EntityId) -> Option<EntityMetadata> {
        self.lock().get(entity, Instant::now())
    }

    fn store(&self, metadata: EntityMetadata) {
        self.lock().put(metadata, Instant::now());
    }

    /// Recover from a poisoned lock rather than panicking: a cache is advisory,
    /// and a panic here would take down an unrelated run.
    fn lock(&self) -> std::sync::MutexGuard<'_, MetadataCache> {
        self.cache
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

#[derive(Deserialize)]
struct RawState {
    entity_id: String,
    state: String,
    #[serde(default)]
    attributes: RawAttributes,
}

#[derive(Deserialize, Default)]
struct RawAttributes {
    #[serde(default)]
    friendly_name: Option<String>,
    #[serde(default)]
    area_id: Option<String>,
}

impl RawState {
    fn into_entity_state(
        self,
        expected: Option<&EntityId>,
    ) -> Result<EntityState, HomeAssistantError> {
        let entity_id: EntityId = self
            .entity_id
            .parse()
            .map_err(|_| HomeAssistantError::InvalidResponse)?;
        // HA answering about a different entity than the one asked for is a
        // confused-deputy signal, not a formatting quirk.
        if expected.is_some_and(|expected| *expected != entity_id) {
            return Err(HomeAssistantError::InvalidResponse);
        }
        let friendly_name = self
            .attributes
            .friendly_name
            .map(|name| clean_text(&name, MAX_FRIENDLY_NAME_CHARS))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| entity_id.as_str().to_owned());
        let area = self
            .attributes
            .area_id
            .map(|area| clean_text(&area, MAX_FRIENDLY_NAME_CHARS))
            .filter(|area| !area.is_empty());
        let state = clean_text(&self.state, MAX_STATE_TEXT_CHARS);
        if state.is_empty() {
            return Err(HomeAssistantError::InvalidResponse);
        }
        Ok(EntityState {
            metadata: EntityMetadata {
                entity_id,
                friendly_name,
                area,
            },
            state,
        })
    }
}

/// HA-supplied text is untrusted content that ends up on an approval card and in
/// the model's context: strip control/bidi/zero-width characters with the domain
/// sanitizer, collapse it to a single line, and cap its length.
fn clean_text(value: &str, max_chars: usize) -> String {
    let sanitized = sanitize_result_content(value, max_chars * 4).text;
    let single_line: String = sanitized
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .collect();
    single_line
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

// ---------------------------------------------------------------------------
// Shared tool plumbing
// ---------------------------------------------------------------------------

fn arguments_fingerprint(arguments: &CanonicalValue) -> jarvis_domain::grants::Sha256 {
    let mut hasher = Sha2::new();
    hasher.update(canonical_form(arguments));
    jarvis_domain::grants::Sha256::from_bytes(hasher.finalize().into())
}

/// Read the exact set of string keys an argument object must carry — extra or
/// missing keys are a schema violation, so an argument the executor would ignore
/// can never ride along inside a grant's hash.
fn exact_string_args<'a>(
    arguments: &'a CanonicalValue,
    keys: &[&str],
) -> Result<Vec<&'a str>, ToolError> {
    let CanonicalValue::Object(map) = arguments else {
        return Err(ToolError::SchemaInvalid(
            "home arguments must be an object".to_owned(),
        ));
    };
    let expected: BTreeSet<&str> = keys.iter().copied().collect();
    let actual: BTreeSet<&str> = map.keys().map(String::as_str).collect();
    if actual != expected {
        return Err(ToolError::SchemaInvalid(format!(
            "home arguments must be exactly {{{}}}",
            keys.join(", ")
        )));
    }
    let mut values = Vec::with_capacity(keys.len());
    for key in keys {
        match map.get(*key) {
            Some(CanonicalValue::Str(value)) => values.push(value.as_str()),
            _ => {
                return Err(ToolError::SchemaInvalid(format!(
                    "home argument `{key}` must be a string"
                )));
            }
        }
    }
    Ok(values)
}

fn parse_entity(value: &str) -> Result<EntityId, ToolError> {
    value
        .parse()
        .map_err(|_| ToolError::SchemaInvalid("invalid home entity id".to_owned()))
}

/// The denial a non-allowlisted target produces. The entity id is echoed on
/// purpose — it is owner-visible configuration, not a secret, and naming it is
/// what makes the denial actionable.
fn not_allowlisted(entity: &EntityId) -> ToolError {
    ToolError::Denied(format!("{entity} is not on the home control allowlist"))
}

/// Re-validate a grant at the executor, immediately before the effect
/// (docs/06 §4, policy-grants skill step 5). The orchestrator's `GrantValidator`
/// is the primary gate; this is the tool's own fail-closed check so a direct
/// invocation of the executor cannot bypass it. It therefore re-checks
/// *everything* that matters, expiry included. Kept symmetric with
/// [`crate::spotify`]'s `check_grant`.
///
/// The target entity is bound through `normalized_args_sha256` rather than
/// through the resource pattern — see [`grant_target_resource`] for why.
fn check_grant(
    grant: Option<&ExecutionGrant>,
    invocation: &ToolInvocation,
    now: SystemTime,
) -> Result<(), ToolError> {
    let Some(grant) = grant else {
        return Err(ToolError::Denied(format!(
            "{} requires an execution grant",
            invocation.tool_id
        )));
    };
    let fingerprint = arguments_fingerprint(&invocation.arguments);
    if grant.tool_id != invocation.tool_id
        || grant.tool_version != invocation.tool_version
        || !grant.single_use
        || grant.normalized_args_sha256 != fingerprint
        || !grant
            .target_resource
            .matches(&grant_target_resource(&invocation.tool_id))
        || grant.expires_at <= now
    {
        return Err(ToolError::Denied(format!(
            "execution grant does not match {}",
            invocation.tool_id
        )));
    }
    Ok(())
}

/// The friendly-name argument the approval card renders is checked against HA's
/// own metadata before the effect happens. Text never grants authority: a
/// proposal that claims `script.disarm_alarm` is "Kitchen timer" is refused,
/// rather than trusted, so the name a human approved is the name HA holds.
async fn verify_label(
    client: &HomeAssistantClient,
    entity: &EntityId,
    claimed: &str,
    cancel: &CancellationToken,
) -> Result<EntityMetadata, ToolError> {
    let metadata = client.metadata(entity, cancel).await?;
    if metadata.friendly_name != clean_text(claimed, MAX_FRIENDLY_NAME_CHARS) {
        return Err(ToolError::Denied(format!(
            "the approved name does not match Home Assistant's name for {entity}"
        )));
    }
    Ok(metadata)
}

// ---------------------------------------------------------------------------
// home.get_state (R0)
// ---------------------------------------------------------------------------

/// `home.get_state` — read one allowlisted entity's **live** state.
pub struct HomeGetStateTool {
    client: Arc<HomeAssistantClient>,
    allowlist: Arc<EntityAllowlist>,
}

impl HomeGetStateTool {
    pub fn new(client: Arc<HomeAssistantClient>, allowlist: Arc<EntityAllowlist>) -> Self {
        Self { client, allowlist }
    }

    pub fn id() -> ToolId {
        "home.get_state".parse().expect("static tool id is valid")
    }

    /// Host-owned policy: **R0** — read-only, automatic within scope, audited
    /// (docs/06 §3). `Local` egress: the request reaches HA on the LAN and
    /// nothing leaves the home network.
    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R0,
            is_reversible: true,
            requires_user_presence: false,
            timeout: REQUEST_TIMEOUT,
            required_scopes: [Scope::new(READ_SCOPE).expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::Local,
        }
    }

    pub fn descriptor(
        client: Arc<HomeAssistantClient>,
        allowlist: Arc<EntityAllowlist>,
    ) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: Arc::new(Self::new(client, allowlist)),
        }
    }

    fn target(&self, arguments: &CanonicalValue) -> Result<EntityId, ToolError> {
        let [entity_id] = exact_string_args(arguments, &["entity_id"])?[..] else {
            return Err(ToolError::SchemaInvalid(
                "home arguments must be exactly {entity_id}".to_owned(),
            ));
        };
        let entity = parse_entity(entity_id)?;
        if !self.allowlist.is_readable(&entity) {
            return Err(not_allowlisted(&entity));
        }
        Ok(entity)
    }
}

#[async_trait]
impl ToolExecutor for HomeGetStateTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R0: auto-authorized, never carries a grant.
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        // Allowlist first: a denied read costs no request.
        let entity = self.target(&invocation.arguments)?;
        // Always live — HA is the system of record (docs/02 §10).
        let state = self.client.state(&entity, &cancel).await?;
        Ok(ToolResult {
            content: format!("{} is {}.", state.metadata.label(), state.state),
            truncated: false,
            compensation: None,
        })
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        self.target(arguments).map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// home.set_light (R1 + allowlist)
// ---------------------------------------------------------------------------

/// The desired light state. Deliberately binary.
///
/// Brightness, colour and transition are **out of scope for F5.3** — every
/// extra parameter is another argument the policy tier cannot see, and the
/// milestone's exit evidence is "safely control one allowlisted entity".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LightState {
    On,
    Off,
}

impl LightState {
    fn parse(value: &str) -> Result<Self, ToolError> {
        match value {
            "on" => Ok(Self::On),
            "off" => Ok(Self::Off),
            _ => Err(ToolError::SchemaInvalid(
                "home argument `state` must be `on` or `off`".to_owned(),
            )),
        }
    }

    fn service(self) -> CuratedService {
        match self {
            Self::On => CuratedService::LightTurnOn,
            Self::Off => CuratedService::LightTurnOff,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// `home.set_light` — turn one allowlisted light on or off.
pub struct HomeSetLightTool {
    client: Arc<HomeAssistantClient>,
    allowlist: Arc<EntityAllowlist>,
}

impl HomeSetLightTool {
    pub fn new(client: Arc<HomeAssistantClient>, allowlist: Arc<EntityAllowlist>) -> Self {
        Self { client, allowlist }
    }

    pub fn id() -> ToolId {
        "home.set_light".parse().expect("static tool id is valid")
    }

    /// Host-owned policy: **R1** — docs/06 §3's own "toggle a light" row.
    /// Reversible is claimed here only because the executor proves it: it reads
    /// the prior state and registers the concrete undo. Local egress; no user
    /// presence required, which is what makes a voice-routed light command
    /// (F5.5) work hands-free.
    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R1,
            is_reversible: true,
            requires_user_presence: false,
            timeout: REQUEST_TIMEOUT,
            required_scopes: [Scope::new(CONTROL_SCOPE).expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::Local,
        }
    }

    pub fn descriptor(
        client: Arc<HomeAssistantClient>,
        allowlist: Arc<EntityAllowlist>,
    ) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: Arc::new(Self::new(client, allowlist)),
        }
    }

    fn target(&self, arguments: &CanonicalValue) -> Result<(EntityId, LightState), ToolError> {
        let values = exact_string_args(arguments, &["entity_id", "state"])?;
        let [entity_id, state] = values[..] else {
            return Err(ToolError::SchemaInvalid(
                "home arguments must be exactly {entity_id, state}".to_owned(),
            ));
        };
        let entity = parse_entity(entity_id)?;
        // The allowlist check *is* the authorization for this tier, because
        // `policy::evaluate` never sees these arguments. It runs here, in the
        // executor's own pure path, so both `validate_args` (pre-grant) and
        // `execute` (pre-transport) enforce the identical rule.
        //
        // `allows_light` also pins the domain to `light.*`: a `switch.*` or
        // `lock.*` entity is refused rather than being quietly routed to
        // `switch.turn_on`. That is the conservative reading — a caller who
        // wants a non-light entity must get a tool built for it, with its own
        // tier, not this one's R1.
        if !self.allowlist.allows_light(&entity) {
            return Err(not_allowlisted(&entity));
        }
        Ok((entity, LightState::parse(state)?))
    }
}

/// The single-entity unit of work, shared by `home.set_light` and the F5.4 area
/// fan-out.
///
/// It is a free function rather than a method precisely so the plural tool calls
/// the *identical* code path per entity — including the fail-closed pre-read —
/// and collects `Result`s. A partial failure is therefore a list of per-entity
/// outcomes, never one collapsed success/failure for the whole area.
///
/// The caller is responsible for the allowlist check; this function performs no
/// authorization of its own and is not reachable from outside the module.
async fn apply_one(
    client: &HomeAssistantClient,
    entity: &EntityId,
    desired: LightState,
    cancel: &CancellationToken,
) -> Result<ToolResult, ToolError> {
    // Read the prior state first. A "reversible" action whose undo cannot be
    // described is not reversible, so a failed pre-read fails the call
    // rather than mutating blind.
    let before = client.state(entity, cancel).await?;
    if cancel.is_cancelled() {
        return Err(ToolError::Cancelled);
    }
    client
        .call_service(desired.service(), entity, cancel)
        .await?;
    let label = before.metadata.label();
    Ok(ToolResult {
        content: format!("{label} is now {}.", desired.as_str()),
        truncated: false,
        compensation: Some(format!("Set {label} back to {}.", before.state)),
    })
}

#[async_trait]
impl ToolExecutor for HomeSetLightTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R1: auto-authorized, never carries a grant.
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let (entity, desired) = self.target(&invocation.arguments)?;
        apply_one(&self.client, &entity, desired, &cancel).await
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        self.target(arguments).map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// home.set_area_lights (R1 + allowlist + fan-out cap) — F5.4, FR-28, ADR-018
// ---------------------------------------------------------------------------

/// The hard fan-out bound for one plural command.
///
/// This is the control that answers "a plural command has a strictly larger
/// blast radius than the single-entity R1 `set_light`": the difference between
/// the two is *exactly* the fan-out, so the fan-out is what gets bounded. A
/// house whose living room somehow resolves to 40 allowlisted lights gets a
/// refusal naming the count, not a 40-entity sweep. Sixteen comfortably covers a
/// real room while keeping the worst case reviewable in one spoken sentence.
const MAX_AREA_ENTITIES: usize = 16;
const MAX_AREA_NAME_CHARS: usize = 48;

/// A normalized area name — HA's `area_id` slug shape.
///
/// Both sides of the comparison are normalized so the spoken form ("Living
/// Room", "the living room") matches HA's stored `area_id` ("living_room")
/// without a lookup table. Non-ASCII letters are kept and lowercased rather than
/// transliterated: HA's own slugifier may fold them differently, and guessing
/// wrong would silently mis-resolve, so an unmatched area is reported as "no
/// lights in that area" rather than quietly matching the wrong room.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AreaKey(String);

fn normalize_area(value: &str) -> Option<AreaKey> {
    let mut key = String::new();
    for ch in value.chars() {
        if ch.is_alphanumeric() {
            key.extend(ch.to_lowercase());
        } else if !key.ends_with('_') {
            key.push('_');
        }
    }
    let key = key.trim_matches('_').to_owned();
    (!key.is_empty()).then_some(AreaKey(key))
}

/// The human-facing area text: sanitized like every other HA-adjacent string,
/// with a leading article dropped so the rendered result reads "in the living
/// room" rather than "in the the living room".
fn area_label(value: &str) -> String {
    // `clean_text` has already collapsed runs of whitespace, so the article is
    // exactly the first space-separated word. Split on the char boundary rather
    // than byte-slicing a lowercased copy: case folding is not length-preserving
    // for every scalar, and an area name is untrusted text.
    let cleaned = clean_text(value, MAX_AREA_NAME_CHARS);
    match cleaned.split_once(' ') {
        Some((article, rest)) if article.eq_ignore_ascii_case("the") && !rest.is_empty() => {
            rest.to_owned()
        }
        _ => cleaned,
    }
}

fn lights_noun(count: usize) -> &'static str {
    if count == 1 { "light" } else { "lights" }
}

/// "A", "A and B", "A, B and C" — the result is spoken aloud (F5.5), so it is
/// written to be heard, not parsed.
fn join_labels(labels: &[String]) -> String {
    match labels {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The honesty clause for the `area_id` gap. Never omitted when it applies:
/// silently dropping lights whose area HA did not report is precisely the
/// failure ADR-018 forbids.
fn unknown_area_caveat(count: usize) -> String {
    if count == 0 {
        return String::new();
    }
    let verb = if count == 1 { "has" } else { "have" };
    format!(
        " {count} allowlisted {} {verb} no known area in Home Assistant and could not be considered.",
        lights_noun(count)
    )
}

fn cancelled_caveat(skipped: &[String]) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    let verb = if skipped.len() == 1 { "was" } else { "were" };
    format!(
        " {} {verb} not attempted because the request was cancelled.",
        join_labels(skipped)
    )
}

/// What an area resolved to, plus what could not be judged.
#[derive(Debug, Default)]
struct AreaResolution {
    /// Allowlisted lights whose HA area matches, in entity-id order so the
    /// spoken result and the audit trail are deterministic.
    matched: Vec<EntityMetadata>,
    /// Allowlisted lights HA reported no area for (or does not know at all).
    /// Counted, never silently discarded.
    unknown_area: usize,
}

/// `home.set_area_lights` — turn every allowlisted light in one area on or off.
///
/// The **device class is fixed to `light.*` by the tool's identity**, exactly as
/// `home.set_light` pins it. That is the conservative reading of FR-28's
/// "area/device-class": a plural command for another class (switches, covers)
/// must be a tool built for that class with its own tier, not a `device_class`
/// argument this one's tier cannot see.
///
/// An area is **required**. There is deliberately no "everywhere" form: a
/// whole-house sweep is a different blast radius from a room, and it is not
/// what FR-28 asks for.
pub struct HomeSetAreaLightsTool {
    client: Arc<HomeAssistantClient>,
    allowlist: Arc<EntityAllowlist>,
}

impl HomeSetAreaLightsTool {
    pub fn new(client: Arc<HomeAssistantClient>, allowlist: Arc<EntityAllowlist>) -> Self {
        Self { client, allowlist }
    }

    pub fn id() -> ToolId {
        "home.set_area_lights"
            .parse()
            .expect("static tool id is valid")
    }

    /// Host-owned policy: **R1**, the same tier as `home.set_light`.
    ///
    /// This is the F5.4 tier decision, argued against docs/06 §3 rather than
    /// assumed from the singular case:
    ///
    /// * **R1's own row is "reversible, low impact"** and its example is
    ///   "toggle a light". Every member of the resolved set is, individually,
    ///   that exact action. The set is bounded by the owner's allowlist, pinned
    ///   to `light.*`, local to the LAN, and — see below — genuinely reversible
    ///   per entity. None of R2's row applies: nothing leaves the home, no
    ///   record is mutated, no automation is changed.
    /// * **R2 would buy friction, not containment.** The same caller that can
    ///   invoke this tool can already invoke `home.set_light` N times, each
    ///   auto-authorized at R1, and reach the identical physical effect. A
    ///   control the same actor can bypass at the same authority, by the same
    ///   code path, is theatre — and it would put an approval card in front of
    ///   the single commonest voice command in a house ("turn off the lights"),
    ///   which is how owners get trained to blanket-approve.
    /// * **What actually differs is fan-out, so fan-out is what is bounded** —
    ///   [`MAX_AREA_ENTITIES`], enforced in-executor because `policy::evaluate`
    ///   does not inspect arguments (the same constraint that forced two tools
    ///   for the M3a volume cap and F5.6's volume boost).
    /// * **If this had been R2, the resolved set would have had to reach the
    ///   card.** Approving the literal argument "living room" tells a human
    ///   nothing about which entities it expanded to, and resolution needs HA
    ///   I/O that `validate_args` (sync) cannot perform — so an honest R2 form
    ///   of this tool would need the resolved set carried in the arguments and
    ///   re-verified against HA at execution. That is a real design, and it is
    ///   the one to build if an owner wants approval here; the correct way to
    ///   get it is a policy rule through the settings flow (docs/06 §3), which
    ///   is a human-only decision, not a tier this adapter invents.
    ///
    /// `is_reversible` is claimed only because the executor proves it: every
    /// entity is pre-read, the undo is composed from those pre-reads, and an
    /// entity whose pre-read fails is not mutated at all.
    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R1,
            is_reversible: true,
            requires_user_presence: false,
            timeout: REQUEST_TIMEOUT,
            required_scopes: [Scope::new(CONTROL_SCOPE).expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::Local,
        }
    }

    pub fn descriptor(
        client: Arc<HomeAssistantClient>,
        allowlist: Arc<EntityAllowlist>,
    ) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: Arc::new(Self::new(client, allowlist)),
        }
    }

    /// Arguments are exactly `{area, state}`. The caller names a *room*, never
    /// an entity: the expansion is Jarvis's own, derived from HA metadata and
    /// bounded by the allowlist, so there is no entity list a model could lie
    /// about. Text proposes the room; it never names the targets.
    fn target(
        &self,
        arguments: &CanonicalValue,
    ) -> Result<(AreaKey, String, LightState), ToolError> {
        let values = exact_string_args(arguments, &["area", "state"])?;
        let [area, state] = values[..] else {
            return Err(ToolError::SchemaInvalid(
                "home arguments must be exactly {area, state}".to_owned(),
            ));
        };
        if area.is_empty() || area.len() > MAX_AREA_NAME_CHARS * 4 {
            return Err(ToolError::SchemaInvalid(
                "home argument `area` is out of range".to_owned(),
            ));
        }
        let label = area_label(area);
        let key = normalize_area(&label).ok_or_else(|| {
            ToolError::SchemaInvalid("home argument `area` is not a usable area name".to_owned())
        })?;
        Ok((key, label, LightState::parse(state)?))
    }

    /// Resolve an area to the concrete allowlisted entity set.
    ///
    /// The candidate set is the *allowlist* — HA metadata only filters it. This
    /// direction is the whole security property: iterating HA and then checking
    /// the allowlist would be equivalent only as long as the check is never
    /// forgotten, while iterating the allowlist makes reaching a non-allowlisted
    /// entity structurally impossible.
    async fn resolve(
        &self,
        area: &AreaKey,
        cancel: &CancellationToken,
    ) -> Result<AreaResolution, ToolError> {
        let lights: Vec<EntityId> = self.allowlist.lights().cloned().collect();
        if lights.is_empty() {
            return Ok(AreaResolution::default());
        }
        // One bounded index read, and only when the cache cannot already answer
        // for every candidate. `cached` returns `None` for stale entries too, so
        // this is also the TTL refresh.
        if lights
            .iter()
            .any(|entity| self.client.cached(entity).is_none())
        {
            self.client.refresh_metadata(cancel).await?;
        }
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let mut resolution = AreaResolution::default();
        for entity in &lights {
            let Some(metadata) = self.client.cached(entity) else {
                // HA does not know this allowlisted entity at all. Unknown, not
                // absent — counted so the caveat can say so.
                resolution.unknown_area += 1;
                continue;
            };
            match metadata.area.as_deref().and_then(normalize_area) {
                Some(found) if found == *area => resolution.matched.push(metadata),
                Some(_) => {}
                None => resolution.unknown_area += 1,
            }
        }
        tracing::debug!(
            target: "jarvis.home",
            area = %area.0,
            matched = resolution.matched.len(),
            unknown_area = resolution.unknown_area,
            "resolved a home area to allowlisted lights",
        );
        Ok(resolution)
    }
}

#[async_trait]
impl ToolExecutor for HomeSetAreaLightsTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R1: auto-authorized, never carries a grant.
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let (key, label, desired) = self.target(&invocation.arguments)?;
        let resolution = self.resolve(&key, &cancel).await?;
        let total = resolution.matched.len();

        // Both refusals happen before any mutation, and both are errors: an
        // area that drove nothing must never be reported as a success.
        if total > MAX_AREA_ENTITIES {
            return Err(ToolError::Denied(format!(
                "{total} allowlisted lights are in the {label}; a plural command may drive at most \
                 {MAX_AREA_ENTITIES}. Name the lights individually."
            )));
        }
        if total == 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "No allowlisted lights are in the {label}.{}",
                unknown_area_caveat(resolution.unknown_area)
            )));
        }

        // Per-entity execution. One failure must neither abort the rest nor be
        // swallowed, so every outcome lands in exactly one bucket and every
        // bucket reaches the text below.
        let mut succeeded: Vec<String> = Vec::new();
        let mut undos: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for metadata in &resolution.matched {
            let entity_label = metadata.label();
            if cancel.is_cancelled() {
                skipped.push(entity_label);
                continue;
            }
            match apply_one(&self.client, &metadata.entity_id, desired, &cancel).await {
                Ok(result) => {
                    succeeded.push(entity_label);
                    // Per-entity undo, derived from that entity's own pre-read.
                    undos.extend(result.compensation);
                }
                Err(ToolError::Cancelled) => skipped.push(entity_label),
                Err(error) => {
                    // The generic error text is logged, not spoken: the caller
                    // needs to know *which* light failed, and the adapter's
                    // error strings carry no HA detail worth repeating.
                    tracing::warn!(
                        target: "jarvis.home",
                        entity = %metadata.entity_id,
                        error = %error,
                        "home area command: an entity did not respond",
                    );
                    failed.push(entity_label);
                }
            }
        }

        if succeeded.is_empty() {
            // Total failure is reported as total failure. Rounding it up to a
            // partial success would be the same lie in the other direction.
            if failed.is_empty() {
                return Err(ToolError::Cancelled);
            }
            return Err(ToolError::ExecutionFailed(format!(
                "None of the {total} {} in the {label} responded: {}.{}{}",
                lights_noun(total),
                join_labels(&failed),
                cancelled_caveat(&skipped),
                unknown_area_caveat(resolution.unknown_area),
            )));
        }

        let verb = desired.as_str();
        let mut content = if succeeded.len() == total {
            if total == 1 {
                format!("Turned {verb} {} in the {label}.", succeeded[0])
            } else {
                format!(
                    "Turned {verb} all {total} lights in the {label}: {}.",
                    join_labels(&succeeded)
                )
            }
        } else {
            // Never rounded up: the count leads, and the survivors are named.
            format!(
                "Turned {verb} {} of {total} {} in the {label}: {}.",
                succeeded.len(),
                lights_noun(total),
                join_labels(&succeeded)
            )
        };
        if !failed.is_empty() {
            content.push_str(&format!(" {} did not respond.", join_labels(&failed)));
        }
        content.push_str(&cancelled_caveat(&skipped));
        content.push_str(&unknown_area_caveat(resolution.unknown_area));

        Ok(ToolResult {
            content,
            truncated: false,
            // The undo restores each light to *its own* prior state. A blanket
            // "turn them all off" would be wrong for any light that was already
            // on, and only the entities actually mutated appear here.
            compensation: (!undos.is_empty()).then(|| undos.join(" ")),
        })
    }

    /// Shape only. Resolution needs HA I/O and this hook is synchronous, so
    /// there is no entity to check here — and nothing is lost by that: the
    /// arguments name no entity, and the executor can only ever enumerate the
    /// allowlist. The bound is structural rather than validated.
    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        self.target(arguments).map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// home.execute_scene / home.run_script (R2 + allowlist + grant)
// ---------------------------------------------------------------------------

/// Which broad-effect tool a [`HomeBroadTool`] instance is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BroadKind {
    Scene,
    Script,
}

impl BroadKind {
    fn tool_id(self) -> ToolId {
        match self {
            Self::Scene => "home.execute_scene",
            Self::Script => "home.run_script",
        }
        .parse()
        .expect("static tool id is valid")
    }

    fn service(self) -> CuratedService {
        match self {
            Self::Scene => CuratedService::SceneTurnOn,
            Self::Script => CuratedService::ScriptTurnOn,
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Scene => "Activated",
            Self::Script => "Ran",
        }
    }
}

/// `home.execute_scene` and `home.run_script` — the two broad-blast-radius home
/// tools. One implementation, two registered tools: they differ only in the
/// curated service they call and the allowlist they consult, and keeping them as
/// separate `ToolId`s means an approval for a scene can never execute a script.
pub struct HomeBroadTool {
    kind: BroadKind,
    client: Arc<HomeAssistantClient>,
    allowlist: Arc<EntityAllowlist>,
}

impl HomeBroadTool {
    fn new(
        kind: BroadKind,
        client: Arc<HomeAssistantClient>,
        allowlist: Arc<EntityAllowlist>,
    ) -> Self {
        Self {
            kind,
            client,
            allowlist,
        }
    }

    pub fn scene(client: Arc<HomeAssistantClient>, allowlist: Arc<EntityAllowlist>) -> Self {
        Self::new(BroadKind::Scene, client, allowlist)
    }

    pub fn script(client: Arc<HomeAssistantClient>, allowlist: Arc<EntityAllowlist>) -> Self {
        Self::new(BroadKind::Script, client, allowlist)
    }

    pub fn scene_id() -> ToolId {
        BroadKind::Scene.tool_id()
    }

    pub fn script_id() -> ToolId {
        BroadKind::Script.tool_id()
    }

    /// Host-owned policy: **R2** — a scene/script is a set of effects behind one
    /// name (docs/06 §3 "meaningful mutation / change automation"). Not
    /// reversible: there is no single undo for "whatever that script did", so
    /// claiming reversibility would be a lie the approval card repeats. User
    /// presence is required — a broad physical change should not fire while
    /// nobody is at a device to see it. Egress is `Local`: the payload reaches
    /// HA only.
    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R2,
            is_reversible: false,
            requires_user_presence: true,
            timeout: REQUEST_TIMEOUT,
            required_scopes: [Scope::new(CONTROL_SCOPE).expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::Local,
        }
    }

    pub fn scene_descriptor(
        client: Arc<HomeAssistantClient>,
        allowlist: Arc<EntityAllowlist>,
    ) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::scene_id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: Arc::new(Self::scene(client, allowlist)),
        }
    }

    pub fn script_descriptor(
        client: Arc<HomeAssistantClient>,
        allowlist: Arc<EntityAllowlist>,
    ) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::script_id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: Arc::new(Self::script(client, allowlist)),
        }
    }

    /// Arguments are `{entity_id, friendly_name}`. The friendly name is present
    /// because `policy::exact_effect` renders the *arguments* onto the approval
    /// card: carrying it is what makes docs/02 §10's "approvals show friendly
    /// name + entity ID" true of the text a human actually reads. It is checked
    /// against HA before execution (see [`verify_label`]), so it is a claim the
    /// system verifies, never a label the model gets to choose.
    fn target(&self, arguments: &CanonicalValue) -> Result<(EntityId, String), ToolError> {
        let values = exact_string_args(arguments, &["entity_id", "friendly_name"])?;
        let [entity_id, friendly_name] = values[..] else {
            return Err(ToolError::SchemaInvalid(
                "home arguments must be exactly {entity_id, friendly_name}".to_owned(),
            ));
        };
        let entity = parse_entity(entity_id)?;
        let allowed = match self.kind {
            BroadKind::Scene => self.allowlist.allows_scene(&entity),
            BroadKind::Script => self.allowlist.allows_script(&entity),
        };
        if !allowed {
            return Err(not_allowlisted(&entity));
        }
        if friendly_name.is_empty() || friendly_name.len() > MAX_FRIENDLY_NAME_CHARS * 4 {
            return Err(ToolError::SchemaInvalid(
                "home argument `friendly_name` is out of range".to_owned(),
            ));
        }
        Ok((entity, friendly_name.to_owned()))
    }
}

#[async_trait]
impl ToolExecutor for HomeBroadTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        grant: Option<ExecutionGrant>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        // Order matters and is security-first: shape, then allowlist, then
        // grant — all before the transport is touched at all.
        let (entity, claimed_name) = self.target(&invocation.arguments)?;
        check_grant(grant.as_ref(), &invocation, SystemTime::now())?;

        let metadata = verify_label(&self.client, &entity, &claimed_name, &cancel).await?;
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        self.client
            .call_service(self.kind.service(), &entity, &cancel)
            .await?;
        Ok(ToolResult {
            content: format!("{} {}.", self.kind.verb(), metadata.label()),
            truncated: false,
            // Honest: R2 here is not reversible, so no undo is registered.
            compensation: None,
        })
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        self.target(arguments).map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Tests — fixture-driven; no test performs network I/O.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::grants::GrantId;
    use jarvis_domain::ids::{DeviceId, RunId, UserId};
    use jarvis_domain::policy::ResourcePattern;

    /// A scripted transport that records every request. Its very existence is
    /// the assertion that no test reaches the network.
    #[derive(Default)]
    struct FakeTransport {
        calls: Mutex<Vec<HomeRequest>>,
        state_body: Mutex<String>,
        all_states_body: Mutex<String>,
        fail: Mutex<Option<HomeAssistantError>>,
        block_until_cancelled: bool,
        /// Signalled once the transport has actually been entered, so the
        /// cancellation test observes in-flight cancellation rather than racing
        /// the executor's entry guard.
        entered: tokio::sync::Notify,
    }

    impl FakeTransport {
        fn with_state(entity: &str, state: &str, name: &str) -> Self {
            let this = Self::default();
            this.set_state(entity, state, name);
            *this.all_states_body.lock().unwrap() = format!(
                r#"[{{"entity_id":"{entity}","state":"{state}","attributes":{{"friendly_name":"{name}"}}}}]"#
            );
            this
        }

        fn set_state(&self, entity: &str, state: &str, name: &str) {
            *self.state_body.lock().unwrap() = format!(
                r#"{{"entity_id":"{entity}","state":"{state}","attributes":{{"friendly_name":"{name}","area_id":"kitchen"}}}}"#
            );
        }

        fn calls(&self) -> Vec<HomeRequest> {
            self.calls.lock().unwrap().clone()
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl HomeAssistantTransport for FakeTransport {
        async fn send(
            &self,
            request: HomeRequest,
            cancel: CancellationToken,
        ) -> Result<String, HomeAssistantError> {
            self.calls.lock().unwrap().push(request.clone());
            self.entered.notify_one();
            if self.block_until_cancelled {
                cancel.cancelled().await;
                return Err(HomeAssistantError::Cancelled);
            }
            if let Some(error) = *self.fail.lock().unwrap() {
                return Err(error);
            }
            Ok(match request {
                HomeRequest::AllStates => self.all_states_body.lock().unwrap().clone(),
                HomeRequest::State(_) => self.state_body.lock().unwrap().clone(),
                HomeRequest::Service { .. } => "[]".to_owned(),
            })
        }
    }

    fn allowlist() -> Arc<EntityAllowlist> {
        Arc::new(
            EntityAllowlist::new(
                &["sensor.kitchen_temperature".to_owned()],
                &["light.kitchen_lamp".to_owned()],
                &["scene.movie_night".to_owned()],
                &["script.goodnight".to_owned()],
            )
            .unwrap(),
        )
    }

    fn client(transport: Arc<FakeTransport>) -> Arc<HomeAssistantClient> {
        Arc::new(HomeAssistantClient::with_transport(transport))
    }

    fn invocation(id: ToolId, arguments: CanonicalValue) -> ToolInvocation {
        ToolInvocation {
            tool_id: id,
            tool_version: ToolVersion::new(1, 0, 0),
            arguments,
        }
    }

    fn scene_args(entity: &str, name: &str) -> CanonicalValue {
        CanonicalValue::obj([
            ("entity_id", CanonicalValue::str(entity)),
            ("friendly_name", CanonicalValue::str(name)),
        ])
    }

    fn light_args(entity: &str, state: &str) -> CanonicalValue {
        CanonicalValue::obj([
            ("entity_id", CanonicalValue::str(entity)),
            ("state", CanonicalValue::str(state)),
        ])
    }

    /// The grant the orchestrator really mints: `target_resource` is derived
    /// from the proposal's tool id (`orchestrator.rs`, the `WaitingApproval`
    /// arm), so the fixture builds it the same way instead of hand-writing an
    /// entity-scoped string that no minting site produces. Building it wrong is
    /// what let the executor deny every approved R2 home action in production
    /// while the tests stayed green.
    fn grant_for(id: ToolId, args: &CanonicalValue) -> ExecutionGrant {
        let resource = grant_target_resource(&id);
        grant_with(id, args, &resource)
    }

    fn grant_with(id: ToolId, args: &CanonicalValue, resource: &str) -> ExecutionGrant {
        ExecutionGrant {
            grant_id: GrantId::from_bytes([9; 32]),
            user_id: "00000000000000000000000001".parse::<UserId>().unwrap(),
            device_id: "00000000000000000000000002".parse::<DeviceId>().unwrap(),
            run_id: "00000000000000000000000003".parse::<RunId>().unwrap(),
            tool_id: id,
            tool_version: ToolVersion::new(1, 0, 0),
            normalized_args_sha256: arguments_fingerprint(args),
            target_resource: resource.parse::<ResourcePattern>().unwrap(),
            expires_at: SystemTime::now() + Duration::from_secs(300),
            single_use: true,
        }
    }

    // ---- policy assertions -------------------------------------------------

    #[test]
    fn get_state_policy_is_r0_read_only_and_local() {
        let policy = HomeGetStateTool::policy();
        assert_eq!(policy.risk, RiskLevel::R0);
        assert!(!policy.requires_grant());
        assert!(policy.is_reversible);
        assert!(!policy.requires_user_presence);
        assert_eq!(policy.egress, DataEgress::Local);
        assert!(
            policy
                .required_scopes
                .contains(&Scope::new(READ_SCOPE).unwrap())
        );
    }

    #[test]
    fn set_light_policy_is_r1_reversible_local_and_control_scoped() {
        let policy = HomeSetLightTool::policy();
        assert_eq!(policy.risk, RiskLevel::R1);
        assert!(policy.is_reversible);
        assert!(!policy.requires_grant());
        assert!(!policy.requires_user_presence);
        assert_eq!(policy.egress, DataEgress::Local);
        assert!(
            policy
                .required_scopes
                .contains(&Scope::new(CONTROL_SCOPE).unwrap())
        );
    }

    #[test]
    fn scene_and_script_policies_are_r2_irreversible_and_require_a_grant() {
        let policy = HomeBroadTool::policy();
        assert_eq!(policy.risk, RiskLevel::R2);
        assert!(policy.requires_grant());
        assert!(!policy.is_reversible);
        assert!(policy.requires_user_presence);
        assert_eq!(policy.egress, DataEgress::Local);
        // Distinct ids: a scene approval can never execute a script.
        assert_ne!(HomeBroadTool::scene_id(), HomeBroadTool::script_id());
    }

    // ---- allowlist enforcement (policy cannot see arguments) ---------------

    #[tokio::test]
    async fn set_light_on_a_non_allowlisted_entity_is_denied_before_any_request() {
        let transport = Arc::new(FakeTransport::with_state(
            "light.bedroom_lamp",
            "off",
            "Bedroom lamp",
        ));
        let tool = HomeSetLightTool::new(client(transport.clone()), allowlist());
        let error = tool
            .execute(
                invocation(
                    HomeSetLightTool::id(),
                    light_args("light.bedroom_lamp", "on"),
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
        assert_eq!(
            transport.call_count(),
            0,
            "denied before any transport call"
        );
    }

    #[tokio::test]
    async fn set_light_refuses_a_non_light_entity_even_if_otherwise_allowlisted() {
        // `switch.kitchen_kettle` is on no list, and even a mis-typed config
        // could not put it on the light list (`EntityAllowlist::new` rejects it).
        let transport = Arc::new(FakeTransport::default());
        let tool = HomeSetLightTool::new(client(transport.clone()), allowlist());
        let error = tool
            .execute(
                invocation(
                    HomeSetLightTool::id(),
                    light_args("switch.kitchen_kettle", "on"),
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
        assert_eq!(transport.call_count(), 0);
        assert_eq!(
            EntityAllowlist::new(&[], &["switch.kitchen_kettle".to_owned()], &[], &[]).err(),
            Some(AllowlistError::WrongDomain("light"))
        );
    }

    #[tokio::test]
    async fn validate_args_rejects_a_non_allowlisted_entity_before_a_grant_is_minted() {
        // CF-9: the orchestrator calls this on the human's approved arguments,
        // so an edited entity id never reaches a minted grant.
        let transport = Arc::new(FakeTransport::default());
        let tool = HomeBroadTool::scene(client(transport.clone()), allowlist());
        let error = tool
            .validate_args(&scene_args("scene.away_mode", "Away mode"))
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
        assert_eq!(transport.call_count(), 0);
    }

    #[tokio::test]
    async fn get_state_on_a_non_allowlisted_entity_is_denied_before_any_request() {
        let transport = Arc::new(FakeTransport::default());
        let tool = HomeGetStateTool::new(client(transport.clone()), allowlist());
        let error = tool
            .execute(
                invocation(
                    HomeGetStateTool::id(),
                    CanonicalValue::obj([(
                        "entity_id",
                        CanonicalValue::str("binary_sensor.front_door"),
                    )]),
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
        assert_eq!(transport.call_count(), 0);
    }

    #[tokio::test]
    async fn run_script_on_a_non_allowlisted_entity_is_denied_before_any_request() {
        let transport = Arc::new(FakeTransport::default());
        let tool = HomeBroadTool::script(client(transport.clone()), allowlist());
        let args = scene_args("script.open_garage", "Open garage");
        let grant = grant_for(HomeBroadTool::script_id(), &args);
        let error = tool
            .execute(
                invocation(HomeBroadTool::script_id(), args),
                Some(grant),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
        assert_eq!(
            transport.call_count(),
            0,
            "allowlist precedes the grant path"
        );
    }

    // ---- grant enforcement -------------------------------------------------

    #[tokio::test]
    async fn r2_tool_without_a_grant_is_denied_before_any_request() {
        let transport = Arc::new(FakeTransport::with_state(
            "scene.movie_night",
            "unknown",
            "Movie night",
        ));
        let tool = HomeBroadTool::scene(client(transport.clone()), allowlist());
        let error = tool
            .execute(
                invocation(
                    HomeBroadTool::scene_id(),
                    scene_args("scene.movie_night", "Movie night"),
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
        assert_eq!(transport.call_count(), 0);
    }

    #[tokio::test]
    async fn a_grant_bound_to_different_arguments_is_denied_before_any_request() {
        let transport = Arc::new(FakeTransport::with_state(
            "scene.movie_night",
            "unknown",
            "Movie night",
        ));
        let tool = HomeBroadTool::scene(client(transport.clone()), allowlist());
        let approved = scene_args("scene.movie_night", "Movie night");
        let executed = scene_args("scene.movie_night", "Movie Night");
        let error = tool
            .execute(
                invocation(HomeBroadTool::scene_id(), executed),
                Some(grant_for(HomeBroadTool::scene_id(), &approved)),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
        assert_eq!(transport.call_count(), 0);
    }

    #[tokio::test]
    async fn a_grant_for_another_resource_is_denied_before_any_request() {
        let transport = Arc::new(FakeTransport::with_state(
            "scene.movie_night",
            "unknown",
            "Movie night",
        ));
        let tool = HomeBroadTool::scene(client(transport.clone()), allowlist());
        let args = scene_args("scene.movie_night", "Movie night");
        let error = tool
            .execute(
                invocation(HomeBroadTool::scene_id(), args.clone()),
                // Right args, right tool — a resource pattern that covers a
                // different tool's grant.
                Some(grant_with(
                    HomeBroadTool::scene_id(),
                    &args,
                    &grant_target_resource(&HomeBroadTool::script_id()),
                )),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
        assert_eq!(transport.call_count(), 0);
    }

    #[tokio::test]
    async fn an_expired_grant_is_denied_before_any_request() {
        let transport = Arc::new(FakeTransport::with_state(
            "scene.movie_night",
            "unknown",
            "Movie night",
        ));
        let tool = HomeBroadTool::scene(client(transport.clone()), allowlist());
        let args = scene_args("scene.movie_night", "Movie night");
        let mut grant = grant_for(HomeBroadTool::scene_id(), &args);
        grant.expires_at = SystemTime::now() - Duration::from_secs(1);
        let error = tool
            .execute(
                invocation(HomeBroadTool::scene_id(), args),
                Some(grant),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
        assert_eq!(transport.call_count(), 0);
    }

    #[tokio::test]
    async fn an_approved_scene_executes_exactly_the_curated_scene_service() {
        let transport = Arc::new(FakeTransport::with_state(
            "scene.movie_night",
            "unknown",
            "Movie night",
        ));
        let tool = HomeBroadTool::scene(client(transport.clone()), allowlist());
        let args = scene_args("scene.movie_night", "Movie night");
        let result = tool
            .execute(
                invocation(HomeBroadTool::scene_id(), args.clone()),
                Some(grant_for(HomeBroadTool::scene_id(), &args)),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        // Friendly name AND entity id in the human-visible result (docs/02 §10).
        assert_eq!(result.content, "Activated Movie night (scene.movie_night).");
        assert_eq!(
            result.compensation, None,
            "R2 here is honestly irreversible"
        );
        assert_eq!(
            transport
                .calls()
                .iter()
                .filter(|call| matches!(call, HomeRequest::Service { .. }))
                .count(),
            1,
            "exactly one service call"
        );
        assert!(transport.calls().iter().any(|call| matches!(
            call,
            HomeRequest::Service {
                service: CuratedService::SceneTurnOn,
                entity,
            } if entity.as_str() == "scene.movie_night"
        )));
    }

    #[tokio::test]
    async fn a_claimed_friendly_name_that_home_assistant_disagrees_with_is_denied() {
        // Text never grants authority: the model cannot relabel a script as
        // something benign on the approval card.
        let transport = Arc::new(FakeTransport::with_state(
            "script.goodnight",
            "off",
            "Goodnight routine",
        ));
        let tool = HomeBroadTool::script(client(transport.clone()), allowlist());
        let args = scene_args("script.goodnight", "Kitchen timer");
        let error = tool
            .execute(
                invocation(HomeBroadTool::script_id(), args.clone()),
                Some(grant_for(HomeBroadTool::script_id(), &args)),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
        assert!(
            !transport
                .calls()
                .iter()
                .any(|call| matches!(call, HomeRequest::Service { .. })),
            "no service call after a name mismatch"
        );
    }

    // ---- HA stays authoritative -------------------------------------------

    #[tokio::test]
    async fn get_state_always_reads_live_and_never_serves_a_cached_value() {
        let transport = Arc::new(FakeTransport::with_state(
            "light.kitchen_lamp",
            "off",
            "Kitchen lamp",
        ));
        let tool = HomeGetStateTool::new(client(transport.clone()), allowlist());
        let args = CanonicalValue::obj([("entity_id", CanonicalValue::str("light.kitchen_lamp"))]);

        let first = tool
            .execute(
                invocation(HomeGetStateTool::id(), args.clone()),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(first.content, "Kitchen lamp (light.kitchen_lamp) is off.");

        // Somebody flips the switch on the wall. HA is the system of record.
        transport.set_state("light.kitchen_lamp", "on", "Kitchen lamp");
        let second = tool
            .execute(
                invocation(HomeGetStateTool::id(), args),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(second.content, "Kitchen lamp (light.kitchen_lamp) is on.");
        assert_eq!(transport.call_count(), 2, "one live read per get_state");
    }

    #[tokio::test]
    async fn metadata_is_cached_while_state_is_not() {
        let transport = Arc::new(FakeTransport::with_state(
            "scene.movie_night",
            "unknown",
            "Movie night",
        ));
        let client = client(transport.clone());
        let entity: EntityId = "scene.movie_night".parse().unwrap();
        let cancel = CancellationToken::new();

        let first = client.metadata(&entity, &cancel).await.unwrap();
        let second = client.metadata(&entity, &cancel).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(first.friendly_name, "Movie night");
        assert_eq!(first.area.as_deref(), Some("kitchen"));
        assert_eq!(transport.call_count(), 1, "second lookup hit the cache");

        // …but a state read still goes to HA every time.
        client.state(&entity, &cancel).await.unwrap();
        assert_eq!(transport.call_count(), 2);
    }

    #[tokio::test]
    async fn set_light_registers_an_undo_derived_from_the_live_prior_state() {
        let transport = Arc::new(FakeTransport::with_state(
            "light.kitchen_lamp",
            "off",
            "Kitchen lamp",
        ));
        let tool = HomeSetLightTool::new(client(transport.clone()), allowlist());
        let result = tool
            .execute(
                invocation(
                    HomeSetLightTool::id(),
                    light_args("light.kitchen_lamp", "on"),
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.content,
            "Kitchen lamp (light.kitchen_lamp) is now on."
        );
        assert_eq!(
            result.compensation.as_deref(),
            Some("Set Kitchen lamp (light.kitchen_lamp) back to off.")
        );
        assert!(transport.calls().iter().any(|call| matches!(
            call,
            HomeRequest::Service {
                service: CuratedService::LightTurnOn,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn a_failed_prior_state_read_does_not_mutate_the_light() {
        let transport = Arc::new(FakeTransport::with_state(
            "light.kitchen_lamp",
            "off",
            "Kitchen lamp",
        ));
        *transport.fail.lock().unwrap() = Some(HomeAssistantError::Unavailable);
        let tool = HomeSetLightTool::new(client(transport.clone()), allowlist());
        let error = tool
            .execute(
                invocation(
                    HomeSetLightTool::id(),
                    light_args("light.kitchen_lamp", "on"),
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::ExecutionFailed(_)),
            "got {error:?}"
        );
        assert!(
            !transport
                .calls()
                .iter()
                .any(|call| matches!(call, HomeRequest::Service { .. })),
            "no mutation when the undo cannot be described"
        );
    }

    // ---- hostile input -----------------------------------------------------

    #[test]
    fn entity_ids_cannot_carry_path_traversal_or_query_smuggling() {
        for hostile in [
            "../../api/services/homeassistant/restart",
            "light.kitchen_lamp/../../config",
            "light.kitchen_lamp?token=x",
            "light.kitchen_lamp#frag",
            "light.kitchen lamp",
            "light.KITCHEN",
            "light.kitchen.lamp",
            "light.",
            ".lamp",
            "lamp",
            "light.kitchen%2flamp",
            "light.kitchen\nlamp",
            &"light.".to_owned().repeat(64),
        ] {
            assert!(
                hostile.parse::<EntityId>().is_err(),
                "accepted hostile entity id: {hostile}"
            );
        }
        assert!("light.kitchen_lamp2".parse::<EntityId>().is_ok());
    }

    #[test]
    fn a_hostile_friendly_name_is_stripped_of_control_and_bidi_characters() {
        let raw = "Kitchen\u{202E}lamp\n\u{200B}<script>";
        let cleaned = clean_text(raw, MAX_FRIENDLY_NAME_CHARS);
        assert!(!cleaned.contains('\u{202E}'));
        assert!(!cleaned.contains('\u{200B}'));
        assert!(!cleaned.contains('\n'));
        assert_eq!(cleaned, "Kitchenlamp <script>");
    }

    #[test]
    fn an_oversized_response_body_is_bounded_rather_than_accumulated() {
        let mut body = BoundedBody::new(1024);
        assert!(body.push(&vec![b'a'; 1024]).is_ok());
        assert_eq!(
            body.push(b"one byte too many"),
            Err(HomeAssistantError::ResponseTooLarge)
        );
    }

    #[tokio::test]
    async fn an_enormous_states_document_is_refused_by_entity_count() {
        let transport = Arc::new(FakeTransport::default());
        let entities: Vec<String> = (0..MAX_PARSED_ENTITIES + 1)
            .map(|i| format!(r#"{{"entity_id":"light.l{i}","state":"off","attributes":{{}}}}"#))
            .collect();
        *transport.all_states_body.lock().unwrap() = format!("[{}]", entities.join(","));
        let client = HomeAssistantClient::with_transport(transport);
        assert_eq!(
            client.refresh_metadata(&CancellationToken::new()).await,
            Err(HomeAssistantError::ResponseTooLarge)
        );
    }

    #[tokio::test]
    async fn a_response_about_a_different_entity_is_rejected() {
        let transport = Arc::new(FakeTransport::with_state(
            "light.bedroom_lamp",
            "on",
            "Bedroom lamp",
        ));
        let client = HomeAssistantClient::with_transport(transport);
        let entity: EntityId = "light.kitchen_lamp".parse().unwrap();
        assert_eq!(
            client.state(&entity, &CancellationToken::new()).await,
            Err(HomeAssistantError::InvalidResponse)
        );
    }

    // ---- secrets -----------------------------------------------------------

    #[test]
    fn no_error_string_can_carry_the_token_or_the_base_url() {
        const TOKEN: &str = "ha-super-secret-token";
        let config = HomeAssistantConfig::new("https://home.example.test:8123", TOKEN).unwrap();
        // The config holds the token but exposes no Debug/Display and no getter.
        assert_eq!(config.token, TOKEN);

        for error in [
            HomeAssistantError::InvalidConfiguration,
            HomeAssistantError::Unavailable,
            HomeAssistantError::Rejected,
            HomeAssistantError::UnknownEntity,
            HomeAssistantError::InvalidResponse,
            HomeAssistantError::ResponseTooLarge,
            HomeAssistantError::Cancelled,
        ] {
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains(TOKEN), "leaked token: {rendered}");
            assert!(!rendered.contains("home.example.test"), "leaked host");
            let tool_error: ToolError = error.into();
            let rendered = format!("{tool_error} {tool_error:?}");
            assert!(!rendered.contains(TOKEN), "leaked token: {rendered}");
        }
    }

    #[test]
    fn configuration_refuses_plaintext_http_and_credential_shaped_urls() {
        for bad in [
            "http://home.example.test:8123",
            "https://user:pass@home.example.test",
            "https://home.example.test/?token=abc",
            "not a url",
        ] {
            assert_eq!(
                HomeAssistantConfig::new(bad, "token").err(),
                Some(HomeAssistantError::InvalidConfiguration),
                "accepted {bad}"
            );
        }
        for bad_token in ["", "tok en", "tok\nen"] {
            assert_eq!(
                HomeAssistantConfig::new("https://home.example.test", bad_token).err(),
                Some(HomeAssistantError::InvalidConfiguration),
            );
        }
    }

    #[test]
    fn routing_only_ever_targets_the_configured_origin_and_curated_services() {
        let transport = RestTransport::new(
            HomeAssistantConfig::new("https://home.example.test:8123/base", "token").unwrap(),
        )
        .unwrap();
        let entity: EntityId = "light.kitchen_lamp".parse().unwrap();
        let (url, body) = transport
            .route(&HomeRequest::State(entity.clone()))
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://home.example.test:8123/base/api/states/light.kitchen_lamp"
        );
        assert!(body.is_none());

        let (url, body) = transport
            .route(&HomeRequest::Service {
                service: CuratedService::LightTurnOff,
                entity,
            })
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://home.example.test:8123/base/api/services/light/turn_off"
        );
        assert_eq!(
            body.as_deref(),
            Some(r#"{"entity_id":"light.kitchen_lamp"}"#)
        );
    }

    // ---- cancellation ------------------------------------------------------

    #[tokio::test]
    async fn cancellation_before_execution_performs_no_request() {
        let transport = Arc::new(FakeTransport::with_state(
            "light.kitchen_lamp",
            "off",
            "Kitchen lamp",
        ));
        let cancel = CancellationToken::new();
        cancel.cancel();
        for (tool, args) in [(
            HomeSetLightTool::new(client(transport.clone()), allowlist()),
            light_args("light.kitchen_lamp", "on"),
        )] {
            let error = tool
                .execute(
                    invocation(HomeSetLightTool::id(), args),
                    None,
                    cancel.clone(),
                )
                .await
                .unwrap_err();
            assert_eq!(error, ToolError::Cancelled);
        }
        assert_eq!(transport.call_count(), 0);
    }

    #[tokio::test]
    async fn cancellation_during_a_request_returns_promptly() {
        let transport = Arc::new(FakeTransport {
            block_until_cancelled: true,
            ..FakeTransport::with_state("light.kitchen_lamp", "off", "Kitchen lamp")
        });
        let tool = HomeGetStateTool::new(client(transport.clone()), allowlist());
        let cancel = CancellationToken::new();
        let cancel_handle = cancel.clone();
        let task = tokio::spawn(async move {
            tool.execute(
                invocation(
                    HomeGetStateTool::id(),
                    CanonicalValue::obj([("entity_id", CanonicalValue::str("light.kitchen_lamp"))]),
                ),
                None,
                cancel,
            )
            .await
        });
        // Cancel only once the request is genuinely in flight.
        tokio::time::timeout(Duration::from_secs(5), transport.entered.notified())
            .await
            .expect("transport was never entered");
        cancel_handle.cancel();
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("executor did not observe cancellation promptly")
            .unwrap()
            .unwrap_err();
        assert_eq!(error, ToolError::Cancelled);
        assert_eq!(
            transport.call_count(),
            1,
            "one in-flight call, then cancelled"
        );
    }

    // ---- argument schema ---------------------------------------------------

    #[test]
    fn argument_shapes_are_exact() {
        let tool = HomeSetLightTool::new(client(Arc::new(FakeTransport::default())), allowlist());
        assert!(
            tool.validate_args(&light_args("light.kitchen_lamp", "on"))
                .is_ok()
        );
        for bad in [
            CanonicalValue::obj([("entity_id", CanonicalValue::str("light.kitchen_lamp"))]),
            CanonicalValue::obj([
                ("entity_id", CanonicalValue::str("light.kitchen_lamp")),
                ("state", CanonicalValue::str("on")),
                ("brightness", CanonicalValue::int(255)),
            ]),
            CanonicalValue::obj([
                ("entity_id", CanonicalValue::str("light.kitchen_lamp")),
                ("state", CanonicalValue::int(1)),
            ]),
            CanonicalValue::str("light.kitchen_lamp"),
        ] {
            assert!(
                matches!(tool.validate_args(&bad), Err(ToolError::SchemaInvalid(_))),
                "accepted {bad:?}"
            );
        }
        assert!(matches!(
            tool.validate_args(&light_args("light.kitchen_lamp", "dim")),
            Err(ToolError::SchemaInvalid(_))
        ));
    }

    #[test]
    fn the_grant_resource_helper_matches_what_the_orchestrator_actually_mints() {
        // Regression: the executor used to check `home:{entity}` while the only
        // minting site parses the *tool id*, and `ResourcePattern::matches` is
        // exact equality without a trailing `*`. Every approved R2 home action
        // was therefore denied after its single-use grant had been consumed.
        for tool in [HomeBroadTool::scene_id(), HomeBroadTool::script_id()] {
            // Exactly `orchestrator.rs`: `proposal.tool_id.as_str().parse()`.
            let minted: ResourcePattern = tool.as_str().parse().unwrap();
            assert_eq!(minted.as_str(), grant_target_resource(&tool));
            assert!(minted.matches(&grant_target_resource(&tool)));
            // The old, entity-scoped string is matched by nothing minted.
            assert!(!minted.matches(&format!("home:{}", "scene.movie_night")));
        }
        // …and the two tools' resources do not cover each other.
        assert!(
            !HomeBroadTool::scene_id()
                .as_str()
                .parse::<ResourcePattern>()
                .unwrap()
                .matches(&grant_target_resource(&HomeBroadTool::script_id()))
        );
    }

    #[tokio::test]
    async fn a_grant_minted_exactly_as_the_orchestrator_mints_it_is_accepted() {
        // The test that was missing: build the grant the way the *validator*
        // builds it — resource parsed from the proposal's tool id — and assert
        // the executor honours it end to end.
        let transport = Arc::new(FakeTransport::with_state(
            "script.goodnight",
            "off",
            "Goodnight routine",
        ));
        let tool = HomeBroadTool::script(client(transport.clone()), allowlist());
        let args = scene_args("script.goodnight", "Goodnight routine");
        // Literally `proposal.tool_id.as_str().parse()`, as orchestrator.rs does.
        let grant = grant_with(
            HomeBroadTool::script_id(),
            &args,
            HomeBroadTool::script_id().as_str(),
        );

        let result = tool
            .execute(
                invocation(HomeBroadTool::script_id(), args),
                Some(grant),
                CancellationToken::new(),
            )
            .await
            .expect("an orchestrator-minted grant must be accepted");
        assert_eq!(result.content, "Ran Goodnight routine (script.goodnight).");
        assert!(transport.calls().iter().any(|call| matches!(
            call,
            HomeRequest::Service {
                service: CuratedService::ScriptTurnOn,
                entity,
            } if entity.as_str() == "script.goodnight"
        )));
    }

    #[tokio::test]
    async fn a_grant_approved_for_another_entity_is_still_denied_by_the_fingerprint() {
        // The entity binding survives the resource-pattern change: `entity_id`
        // is a required argument, so it is inside `normalized_args_sha256`.
        let transport = Arc::new(FakeTransport::with_state(
            "scene.movie_night",
            "unknown",
            "Movie night",
        ));
        let tool = HomeBroadTool::scene(client(transport.clone()), allowlist());
        let approved = scene_args("scene.away_mode", "Away mode");
        let executed = scene_args("scene.movie_night", "Movie night");
        // Same tool, same (orchestrator-shaped) resource — different entity.
        let grant = grant_for(HomeBroadTool::scene_id(), &approved);
        let error = tool
            .execute(
                invocation(HomeBroadTool::scene_id(), executed),
                Some(grant),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
        assert_eq!(transport.call_count(), 0);
    }

    // ---- F5.4: area resolution and partial-failure honesty -----------------

    struct FakeEntity {
        state: String,
        name: String,
        area: Option<String>,
    }

    /// A multi-entity fixture house. Separate from [`FakeTransport`] on purpose:
    /// the F5.3 tests keep their fixture untouched.
    #[derive(Default)]
    struct FakeHome {
        entities: Mutex<BTreeMap<String, FakeEntity>>,
        calls: Mutex<Vec<HomeRequest>>,
        state_failures: Mutex<BTreeSet<String>>,
        service_failures: Mutex<BTreeSet<String>>,
    }

    impl FakeHome {
        fn add(&self, entity: &str, state: &str, name: &str, area: Option<&str>) {
            self.entities.lock().unwrap().insert(
                entity.to_owned(),
                FakeEntity {
                    state: state.to_owned(),
                    name: name.to_owned(),
                    area: area.map(str::to_owned),
                },
            );
        }

        fn fail_service(&self, entity: &str) {
            self.service_failures
                .lock()
                .unwrap()
                .insert(entity.to_owned());
        }

        fn fail_state(&self, entity: &str) {
            self.state_failures
                .lock()
                .unwrap()
                .insert(entity.to_owned());
        }

        fn render(id: &str, entity: &FakeEntity) -> String {
            match &entity.area {
                Some(area) => format!(
                    r#"{{"entity_id":"{id}","state":"{}","attributes":{{"friendly_name":"{}","area_id":"{area}"}}}}"#,
                    entity.state, entity.name
                ),
                None => format!(
                    r#"{{"entity_id":"{id}","state":"{}","attributes":{{"friendly_name":"{}"}}}}"#,
                    entity.state, entity.name
                ),
            }
        }

        fn service_calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter_map(|call| match call {
                    HomeRequest::Service { entity, .. } => Some(entity.as_str().to_owned()),
                    _ => None,
                })
                .collect()
        }

        /// True if the transport ever saw a request *naming* this entity. The
        /// `/api/states` index names nobody, so this is exactly "was this entity
        /// read or driven".
        fn touched(&self, entity: &str) -> bool {
            self.calls.lock().unwrap().iter().any(|call| match call {
                HomeRequest::State(id) => id.as_str() == entity,
                HomeRequest::Service { entity: id, .. } => id.as_str() == entity,
                HomeRequest::AllStates => false,
            })
        }
    }

    #[async_trait]
    impl HomeAssistantTransport for FakeHome {
        async fn send(
            &self,
            request: HomeRequest,
            cancel: CancellationToken,
        ) -> Result<String, HomeAssistantError> {
            self.calls.lock().unwrap().push(request.clone());
            if cancel.is_cancelled() {
                return Err(HomeAssistantError::Cancelled);
            }
            let entities = self.entities.lock().unwrap();
            match request {
                HomeRequest::AllStates => Ok(format!(
                    "[{}]",
                    entities
                        .iter()
                        .map(|(id, entity)| Self::render(id, entity))
                        .collect::<Vec<_>>()
                        .join(",")
                )),
                HomeRequest::State(id) => {
                    if self.state_failures.lock().unwrap().contains(id.as_str()) {
                        return Err(HomeAssistantError::Unavailable);
                    }
                    entities
                        .get(id.as_str())
                        .map(|entity| Self::render(id.as_str(), entity))
                        .ok_or(HomeAssistantError::UnknownEntity)
                }
                HomeRequest::Service { entity, .. } => {
                    if self
                        .service_failures
                        .lock()
                        .unwrap()
                        .contains(entity.as_str())
                    {
                        return Err(HomeAssistantError::Rejected);
                    }
                    Ok("[]".to_owned())
                }
            }
        }
    }

    fn area_args(area: &str, state: &str) -> CanonicalValue {
        CanonicalValue::obj([
            ("area", CanonicalValue::str(area)),
            ("state", CanonicalValue::str(state)),
        ])
    }

    fn lights_allowlist(lights: &[&str]) -> Arc<EntityAllowlist> {
        Arc::new(
            EntityAllowlist::new(
                &[],
                &lights.iter().map(|l| (*l).to_owned()).collect::<Vec<_>>(),
                &[],
                &[],
            )
            .unwrap(),
        )
    }

    /// Three allowlisted living-room lamps plus a fourth, non-allowlisted one in
    /// the same room and a lamp in another room.
    fn living_room() -> (Arc<FakeHome>, Arc<EntityAllowlist>) {
        let home = Arc::new(FakeHome::default());
        home.add("light.sofa_lamp", "off", "Sofa lamp", Some("living_room"));
        home.add(
            "light.corner_lamp",
            "off",
            "Corner lamp",
            Some("living_room"),
        );
        home.add(
            "light.reading_lamp",
            "off",
            "Reading lamp",
            Some("living_room"),
        );
        // In the room, never allowlisted: must remain untouchable.
        home.add(
            "light.tv_backlight",
            "off",
            "TV backlight",
            Some("living_room"),
        );
        home.add("light.kitchen_lamp", "off", "Kitchen lamp", Some("kitchen"));
        let allowlist = lights_allowlist(&[
            "light.sofa_lamp",
            "light.corner_lamp",
            "light.reading_lamp",
            "light.kitchen_lamp",
        ]);
        (home, allowlist)
    }

    fn area_tool(home: Arc<FakeHome>, allowlist: Arc<EntityAllowlist>) -> HomeSetAreaLightsTool {
        HomeSetAreaLightsTool::new(
            Arc::new(HomeAssistantClient::with_transport(home)),
            allowlist,
        )
    }

    async fn run_area(
        tool: &HomeSetAreaLightsTool,
        area: &str,
        state: &str,
    ) -> Result<ToolResult, ToolError> {
        tool.execute(
            invocation(HomeSetAreaLightsTool::id(), area_args(area, state)),
            None,
            CancellationToken::new(),
        )
        .await
    }

    #[test]
    fn set_area_lights_policy_is_r1_reversible_local_and_needs_no_grant() {
        // The F5.4 tier decision, pinned: plural stays R1 (see `policy` for the
        // argument), with the fan-out bounded in-executor instead.
        let policy = HomeSetAreaLightsTool::policy();
        assert_eq!(policy.risk, RiskLevel::R1);
        assert_eq!(policy.risk, HomeSetLightTool::policy().risk);
        assert!(policy.is_reversible);
        assert!(!policy.requires_grant());
        assert!(!policy.requires_user_presence);
        assert_eq!(policy.egress, DataEgress::Local);
        assert!(
            policy
                .required_scopes
                .contains(&Scope::new(CONTROL_SCOPE).unwrap())
        );
    }

    #[tokio::test]
    async fn an_area_command_resolves_to_every_allowlisted_light_in_that_area() {
        let (home, allowlist) = living_room();
        let tool = area_tool(home.clone(), allowlist);
        let result = run_area(&tool, "the living room", "on").await.unwrap();

        assert_eq!(
            result.content,
            "Turned on all 3 lights in the living room: Corner lamp (light.corner_lamp), \
             Reading lamp (light.reading_lamp) and Sofa lamp (light.sofa_lamp)."
        );
        let mut driven = home.service_calls();
        driven.sort();
        assert_eq!(
            driven,
            vec![
                "light.corner_lamp".to_owned(),
                "light.reading_lamp".to_owned(),
                "light.sofa_lamp".to_owned()
            ]
        );
        // The kitchen lamp is allowlisted but in another area.
        assert!(!home.touched("light.kitchen_lamp"));
    }

    #[tokio::test]
    async fn a_non_allowlisted_light_in_the_same_area_is_never_touched() {
        let (home, allowlist) = living_room();
        let tool = area_tool(home.clone(), allowlist);
        run_area(&tool, "living room", "on").await.unwrap();

        assert!(
            !home.touched("light.tv_backlight"),
            "sharing an area must not grant reachability"
        );
        assert!(
            !home
                .service_calls()
                .contains(&"light.tv_backlight".to_owned())
        );
    }

    #[tokio::test]
    async fn a_partial_failure_reports_two_of_three_and_names_the_light_that_failed() {
        // M5 exit evidence #8. The assertions below are deliberately exact: the
        // point of this test is that the wording cannot drift into a blanket
        // "done" without failing here.
        let (home, allowlist) = living_room();
        home.fail_service("light.reading_lamp");
        let tool = area_tool(home.clone(), allowlist);
        let result = run_area(&tool, "living room", "on").await.unwrap();

        assert_eq!(
            result.content,
            "Turned on 2 of 3 lights in the living room: Corner lamp (light.corner_lamp) and \
             Sofa lamp (light.sofa_lamp). Reading lamp (light.reading_lamp) did not respond."
        );
        assert!(result.content.contains("2 of 3"), "the count leads");
        assert!(
            result.content.contains("Reading lamp (light.reading_lamp)"),
            "the failure is named"
        );
        assert!(
            !result.content.contains("all 3"),
            "a partial result is never rounded up to a success"
        );
        // The undo covers only what actually changed.
        let compensation = result.compensation.unwrap();
        assert!(!compensation.contains("light.reading_lamp"));
        assert_eq!(compensation.matches("Set ").count(), 2);
        // One failure did not abort the rest: all three were attempted.
        assert_eq!(home.service_calls().len(), 3);
    }

    #[tokio::test]
    async fn a_light_whose_prior_state_cannot_be_read_fails_without_being_mutated() {
        let (home, allowlist) = living_room();
        home.fail_state("light.reading_lamp");
        let tool = area_tool(home.clone(), allowlist);
        let result = run_area(&tool, "living room", "on").await.unwrap();

        assert!(result.content.contains("2 of 3"));
        assert!(result.content.contains("Reading lamp (light.reading_lamp)"));
        assert!(
            !home
                .service_calls()
                .contains(&"light.reading_lamp".to_owned()),
            "no mutation when that entity's undo cannot be described"
        );
    }

    #[tokio::test]
    async fn a_total_failure_is_an_error_not_a_partial_success() {
        let (home, allowlist) = living_room();
        for entity in ["light.sofa_lamp", "light.corner_lamp", "light.reading_lamp"] {
            home.fail_service(entity);
        }
        let tool = area_tool(home.clone(), allowlist);
        let error = run_area(&tool, "living room", "on").await.unwrap_err();

        let ToolError::ExecutionFailed(message) = &error else {
            panic!("total failure must be an error, got {error:?}");
        };
        assert_eq!(
            message,
            "None of the 3 lights in the living room responded: Corner lamp (light.corner_lamp), \
             Reading lamp (light.reading_lamp) and Sofa lamp (light.sofa_lamp)."
        );
        assert!(!message.contains("Turned on"), "no success wording");
        assert!(!message.contains(" of 3"), "not reported as partial");
    }

    #[tokio::test]
    async fn an_area_with_no_allowlisted_lights_says_so_rather_than_succeeding() {
        let (home, allowlist) = living_room();
        let tool = area_tool(home.clone(), allowlist);
        let error = run_area(&tool, "the garage", "on").await.unwrap_err();

        let ToolError::ExecutionFailed(message) = &error else {
            panic!("got {error:?}");
        };
        assert_eq!(message, "No allowlisted lights are in the garage.");
        assert!(home.service_calls().is_empty(), "nothing was driven");
    }

    #[tokio::test]
    async fn an_area_that_home_assistant_reports_no_area_for_refuses_and_names_the_gap() {
        // The `area_id` limitation: rather than quietly resolving to nothing,
        // the refusal says how many lights could not be judged.
        let home = Arc::new(FakeHome::default());
        home.add("light.sofa_lamp", "off", "Sofa lamp", None);
        home.add("light.corner_lamp", "off", "Corner lamp", None);
        let allowlist = lights_allowlist(&["light.sofa_lamp", "light.corner_lamp"]);
        let tool = area_tool(home.clone(), allowlist);
        let error = run_area(&tool, "living room", "on").await.unwrap_err();

        let ToolError::ExecutionFailed(message) = &error else {
            panic!("got {error:?}");
        };
        assert_eq!(
            message,
            "No allowlisted lights are in the living room. 2 allowlisted lights have no known \
             area in Home Assistant and could not be considered."
        );
        assert!(home.service_calls().is_empty());
    }

    #[tokio::test]
    async fn lights_with_no_known_area_are_surfaced_as_a_caveat_on_a_successful_result() {
        let home = Arc::new(FakeHome::default());
        home.add("light.sofa_lamp", "off", "Sofa lamp", Some("living_room"));
        home.add("light.corner_lamp", "off", "Corner lamp", None);
        let allowlist = lights_allowlist(&["light.sofa_lamp", "light.corner_lamp"]);
        let tool = area_tool(home.clone(), allowlist);
        let result = run_area(&tool, "living room", "on").await.unwrap();

        assert_eq!(
            result.content,
            "Turned on Sofa lamp (light.sofa_lamp) in the living room. 1 allowlisted light has \
             no known area in Home Assistant and could not be considered."
        );
        assert_eq!(home.service_calls(), vec!["light.sofa_lamp".to_owned()]);
    }

    #[tokio::test]
    async fn an_allowlisted_light_home_assistant_does_not_know_counts_as_unknown_not_absent() {
        let home = Arc::new(FakeHome::default());
        home.add("light.sofa_lamp", "off", "Sofa lamp", Some("living_room"));
        // `light.ghost_lamp` is allowlisted but absent from HA entirely.
        let allowlist = lights_allowlist(&["light.sofa_lamp", "light.ghost_lamp"]);
        let tool = area_tool(home.clone(), allowlist);
        let result = run_area(&tool, "living room", "on").await.unwrap();

        assert!(
            result
                .content
                .contains("1 allowlisted light has no known area"),
            "got {}",
            result.content
        );
        assert!(!home.touched("light.ghost_lamp"));
    }

    #[tokio::test]
    async fn the_undo_restores_each_light_to_its_own_prior_state() {
        // A blanket "turn them all off" would be wrong for the lamp that was
        // already on before the command.
        let home = Arc::new(FakeHome::default());
        home.add("light.sofa_lamp", "on", "Sofa lamp", Some("living_room"));
        home.add(
            "light.corner_lamp",
            "off",
            "Corner lamp",
            Some("living_room"),
        );
        let allowlist = lights_allowlist(&["light.sofa_lamp", "light.corner_lamp"]);
        let tool = area_tool(home.clone(), allowlist);
        let result = run_area(&tool, "living room", "on").await.unwrap();

        assert_eq!(
            result.compensation.as_deref(),
            Some(
                "Set Corner lamp (light.corner_lamp) back to off. \
                 Set Sofa lamp (light.sofa_lamp) back to on."
            )
        );
    }

    #[tokio::test]
    async fn an_oversized_area_is_refused_rather_than_swept() {
        let home = Arc::new(FakeHome::default());
        let ids: Vec<String> = (0..=MAX_AREA_ENTITIES)
            .map(|index| format!("light.lamp_{index}"))
            .collect();
        for id in &ids {
            home.add(id, "off", "Lamp", Some("living_room"));
        }
        let allowlist = lights_allowlist(&ids.iter().map(String::as_str).collect::<Vec<_>>()[..]);
        let tool = area_tool(home.clone(), allowlist);
        let error = run_area(&tool, "living room", "on").await.unwrap_err();

        let ToolError::Denied(message) = &error else {
            panic!("got {error:?}");
        };
        assert!(message.contains("at most 16"), "got {message}");
        assert!(home.service_calls().is_empty(), "refused before any effect");
    }

    #[tokio::test]
    async fn cancellation_before_an_area_command_performs_no_request() {
        let (home, allowlist) = living_room();
        let tool = area_tool(home.clone(), allowlist);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = tool
            .execute(
                invocation(HomeSetAreaLightsTool::id(), area_args("living room", "off")),
                None,
                cancel,
            )
            .await
            .unwrap_err();
        assert_eq!(error, ToolError::Cancelled);
        assert!(home.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn area_names_normalize_across_spoken_and_slug_forms() {
        let slug = normalize_area("living_room").unwrap();
        for spoken in ["Living Room", "living room", "living-room", "LIVING  ROOM"] {
            assert_eq!(normalize_area(spoken).as_ref(), Some(&slug), "{spoken}");
        }
        // Normalization alone keeps the article — dropping it is the label
        // pass's job, so the two stages stay independently checkable.
        assert_eq!(
            normalize_area("the living room"),
            Some(AreaKey("the_living_room".to_owned()))
        );
        assert_eq!(
            normalize_area(&area_label("The Living Room")).as_ref(),
            Some(&slug)
        );
        assert_eq!(area_label("the living room"), "living room");
        for empty in ["", "   ", "!!!"] {
            assert!(normalize_area(empty).is_none(), "{empty}");
        }
    }

    #[test]
    fn a_hostile_area_argument_cannot_carry_markup_or_control_characters() {
        let tool = area_tool(Arc::new(FakeHome::default()), lights_allowlist(&[]));
        let label = area_label("Living\u{202E}Room\n\u{200B}<script>");
        assert!(!label.contains('\u{202E}'));
        assert!(!label.contains('\u{200B}'));
        assert!(!label.contains('\n'));
        // …and it still parses to a usable key rather than being silently empty.
        assert!(tool.validate_args(&area_args("Living Room", "on")).is_ok());
    }

    #[test]
    fn the_area_argument_shape_is_exact() {
        let tool = area_tool(Arc::new(FakeHome::default()), lights_allowlist(&[]));
        assert!(tool.validate_args(&area_args("living room", "off")).is_ok());
        for bad in [
            CanonicalValue::obj([("area", CanonicalValue::str("living room"))]),
            CanonicalValue::obj([
                ("area", CanonicalValue::str("living room")),
                ("state", CanonicalValue::str("on")),
                ("brightness", CanonicalValue::int(255)),
            ]),
            CanonicalValue::obj([
                ("entity_id", CanonicalValue::str("light.sofa_lamp")),
                ("state", CanonicalValue::str("on")),
            ]),
            CanonicalValue::str("living room"),
            area_args("living room", "dim"),
            // Not a usable area name.
            area_args("!!!", "on"),
        ] {
            assert!(tool.validate_args(&bad).is_err(), "accepted {bad:?}");
        }
    }
}
