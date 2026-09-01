//! Errors, the HTTP transport seam, and `HomeAssistantClient` — live
//! state + cached metadata (F9.5).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use jarvis_domain::tools::{ToolError, sanitize_result_content};
use reqwest::Url;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::*;

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
    pub(crate) fn max_bytes(&self) -> usize {
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
    pub(crate) token: String,
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

    pub(crate) fn route(
        &self,
        request: &HomeRequest,
    ) -> Result<(Url, Option<String>), HomeAssistantError> {
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
pub(crate) struct BoundedBody {
    max_bytes: usize,
    bytes: Vec<u8>,
}

impl BoundedBody {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            bytes: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<(), HomeAssistantError> {
        if self.bytes.len().saturating_add(chunk.len()) > self.max_bytes {
            return Err(HomeAssistantError::ResponseTooLarge);
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    pub(crate) fn into_string(self) -> Result<String, HomeAssistantError> {
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
pub(crate) struct MetadataCache {
    entries: BTreeMap<EntityId, CacheEntry>,
}

impl MetadataCache {
    pub(crate) fn get(&self, entity: &EntityId, now: Instant) -> Option<EntityMetadata> {
        self.entries
            .get(entity)
            .filter(|entry| now.duration_since(entry.stored_at) < METADATA_TTL)
            .map(|entry| entry.metadata.clone())
    }

    pub(crate) fn put(&mut self, metadata: EntityMetadata, now: Instant) {
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

    pub(in crate::home_assistant) async fn call_service(
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

    pub(crate) fn cached(&self, entity: &EntityId) -> Option<EntityMetadata> {
        self.lock().get(entity, Instant::now())
    }

    pub(crate) fn store(&self, metadata: EntityMetadata) {
        self.lock().put(metadata, Instant::now());
    }

    /// Recover from a poisoned lock rather than panicking: a cache is advisory,
    /// and a panic here would take down an unrelated run.
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, MetadataCache> {
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
    pub(crate) fn into_entity_state(
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
pub(crate) fn clean_text(value: &str, max_chars: usize) -> String {
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
