//! Configuration, errors, the HTTP transport seam, and `SpotifyClient` — the
//! authenticated, rate-limit-aware core every tool executes through.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jarvis_domain::media::VolumePct;
use jarvis_domain::tools::ToolError;
use tokio_util::sync::CancellationToken;

use super::*;

// ---------------------------------------------------------------------------
// Secrets and configuration
// ---------------------------------------------------------------------------

/// A bearer access token. Cloneable so the client can hand it to the transport,
/// but its `Debug` is redacted and it has no `Display` — the only way to read it
/// is [`AccessToken::expose`], which exists solely for the HTTP header
/// (invariant #5).
#[derive(Clone)]
pub struct AccessToken(String);

impl AccessToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// Read the raw token. Call sites are limited to setting the
    /// `Authorization` header; never log, format, or return this value.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccessToken(<redacted>)")
    }
}

/// Spotify connection settings. `refresh_token` is an **already-resolved**
/// keyring secret supplied by the host, never model- or user-supplied text.
/// Deliberately implements no `Debug` (the [`crate::smtp::SmtpConfig`] rule) and
/// exposes no getter for the token.
pub struct SpotifyConfig {
    client_id: String,
    refresh_token: String,
    max_volume: VolumePct,
    market: Option<String>,
    /// Room name → Connect device id (docs/02 §11 catalog B5: "in the kitchen").
    device_aliases: BTreeMap<String, String>,
}

impl SpotifyConfig {
    /// `client_id` is the owner's own Spotify developer app (public PKCE client
    /// — there is no client secret to hold). `refresh_token` comes from the
    /// keyring, already resolved.
    pub fn new(
        client_id: impl Into<String>,
        refresh_token: impl Into<String>,
        max_volume: VolumePct,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            refresh_token: refresh_token.into(),
            max_volume,
            market: None,
            device_aliases: BTreeMap::new(),
        }
    }

    /// ISO-3166-1 alpha-2 market for catalogue relevance. Rejected silently (not
    /// applied) if malformed — a bad market must not break playback.
    #[must_use]
    pub fn with_market(mut self, market: impl Into<String>) -> Self {
        let market = market.into();
        if market.len() == 2 && market.chars().all(|c| c.is_ascii_alphabetic()) {
            self.market = Some(market.to_ascii_uppercase());
        }
        self
    }

    /// Room aliases for Connect targeting. Ids that are not well-formed are
    /// dropped rather than forwarded to Spotify.
    #[must_use]
    pub fn with_device_aliases(
        mut self,
        aliases: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        self.device_aliases = aliases
            .into_iter()
            .filter(|(_, id)| is_valid_device_id(id))
            .map(|(alias, id)| (normalize(&alias), id))
            .collect();
        self
    }

    pub fn max_volume(&self) -> VolumePct {
        self.max_volume
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a Spotify operation failed. No variant carries a token, a refresh token,
/// or a raw provider error (which embeds the request URL): every message is a
/// short, static-shaped diagnostic (invariant #5).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpotifyError {
    #[error("Spotify request was cancelled")]
    Cancelled,
    #[error("Spotify request timed out")]
    Timeout,
    #[error("Spotify request failed")]
    Transport,
    /// The refresh token was rejected (revoked, or consent withdrawn). The fix
    /// is re-enrollment, so say that instead of retrying forever.
    #[error("Spotify authorization is no longer valid; re-link the Spotify account")]
    AuthExpired,
    /// Playback control needs Premium (docs/02 §11a: detected, not assumed).
    #[error("Spotify playback control requires a Premium account")]
    PremiumRequired,
    #[error("no Spotify device is active; start playback on a device or name one")]
    NoActiveDevice,
    #[error("no Spotify device matches that name (available: {available})")]
    DeviceNotFound { available: String },
    #[error("Spotify is rate limiting; retry in {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("nothing on Spotify matched that")]
    NoMatch,
    /// A genuine multi-match (ADR-022 / ADR-016): the payload IS the one fluent
    /// spoken question, so `Display` is the bare question — never a picker, and
    /// never wrapped in provider jargon.
    #[error("{0}")]
    Ambiguity(String),
    #[error("Spotify returned an unexpected response")]
    InvalidResponse,
    #[error("Spotify returned HTTP {status}")]
    Api { status: u16 },
}

impl SpotifyError {
    pub(crate) fn into_tool_error(self) -> ToolError {
        match self {
            Self::Cancelled => ToolError::Cancelled,
            Self::Timeout => ToolError::Timeout(REQUEST_TIMEOUT),
            other => ToolError::ExecutionFailed(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Transport seam
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
}

impl HttpMethod {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
        }
    }
}

/// One Web API call. `path` is always a static literal chosen by this adapter;
/// caller-influenced text only ever appears in `query` values or the JSON
/// `body`, both of which the transport encodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    pub method: HttpMethod,
    pub path: &'static str,
    pub query: Vec<(String, String)>,
    pub body: Option<String>,
}

impl ApiRequest {
    pub(crate) fn new(method: HttpMethod, path: &'static str) -> Self {
        Self {
            method,
            path,
            query: Vec::new(),
            body: None,
        }
    }

    #[must_use]
    pub(crate) fn query(mut self, key: &str, value: impl Into<String>) -> Self {
        self.query.push((key.to_owned(), value.into()));
        self
    }

    #[must_use]
    pub(crate) fn maybe_device(self, device: Option<&str>) -> Self {
        match device {
            Some(id) => self.query("device_id", id),
            None => self,
        }
    }

    #[must_use]
    pub(crate) fn json(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body.to_string());
        self
    }
}

/// A raw Web API response. `body` is already byte-capped by the transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiResponse {
    pub status: u16,
    pub body: String,
    pub retry_after_secs: Option<u64>,
}

impl ApiResponse {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            retry_after_secs: None,
        }
    }

    #[must_use]
    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    pub(crate) fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The OAuth refresh-grant result.
#[derive(Debug, Clone)]
pub struct TokenResponse {
    pub access_token: AccessToken,
    pub expires_in_secs: u64,
    /// Spotify may rotate the refresh token on use; when present it replaces the
    /// in-memory one for the process lifetime.
    pub rotated_refresh_token: Option<String>,
}

/// The network boundary. Fakeable so no test touches Spotify (CLAUDE.md:
/// fixture-driven tests, always) without weakening the production client.
/// Implementations must honour `cancel` promptly (invariant #4) and must never
/// surface a provider error object that embeds credentials or the request URL.
#[async_trait]
pub trait SpotifyTransport: Send + Sync {
    /// OAuth refresh grant against [`TOKEN_ENDPOINT`].
    async fn refresh_access_token(
        &self,
        client_id: &str,
        refresh_token: &str,
        cancel: CancellationToken,
    ) -> Result<TokenResponse, SpotifyError>;

    /// One authenticated Web API call against [`API_BASE`].
    async fn call(
        &self,
        token: &AccessToken,
        request: ApiRequest,
        cancel: CancellationToken,
    ) -> Result<ApiResponse, SpotifyError>;
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

struct CachedToken {
    token: AccessToken,
    expires_at: Instant,
}

struct AuthState {
    refresh_token: String,
    cached: Option<CachedToken>,
}

/// The Spotify Web API client. Owns token lifecycle, error classification, and
/// the ADR-022 resolution rules; the tools below are thin argument shells over
/// it.
pub struct SpotifyClient {
    config: SpotifyConfig,
    transport: Arc<dyn SpotifyTransport>,
    auth: tokio::sync::Mutex<AuthState>,
}

impl SpotifyClient {
    /// Production client over HTTPS.
    pub fn new(config: SpotifyConfig) -> Self {
        Self::with_transport(config, Arc::new(HttpSpotifyTransport::new()))
    }

    /// Injectable-transport constructor (the [`crate::smtp::SmtpTool`] shape).
    pub fn with_transport(config: SpotifyConfig, transport: Arc<dyn SpotifyTransport>) -> Self {
        let refresh_token = config.refresh_token.clone();
        Self {
            config,
            transport,
            auth: tokio::sync::Mutex::new(AuthState {
                refresh_token,
                cached: None,
            }),
        }
    }

    pub fn max_volume(&self) -> VolumePct {
        self.config.max_volume
    }

    /// A valid access token, refreshing when absent, near expiry, or `force`d
    /// (after a 401). The lock is held across the refresh so concurrent tool
    /// calls make one refresh, not N.
    pub(crate) async fn access_token(
        &self,
        cancel: &CancellationToken,
        force: bool,
    ) -> Result<AccessToken, SpotifyError> {
        let mut auth = self.auth.lock().await;
        if !force
            && let Some(cached) = &auth.cached
            && cached.expires_at > Instant::now() + TOKEN_REFRESH_SKEW
        {
            return Ok(cached.token.clone());
        }

        let refreshed = self
            .transport
            .refresh_access_token(&self.config.client_id, &auth.refresh_token, cancel.clone())
            .await?;
        if let Some(rotated) = refreshed.rotated_refresh_token {
            // Spotify rotated the refresh token. Keeping it for the process
            // lifetime is correct but not durable — persisting it back to the
            // keyring is host wiring (see the module docs).
            tracing::info!("spotify refresh token rotated");
            auth.refresh_token = rotated;
        }
        let token = refreshed.access_token;
        auth.cached = Some(CachedToken {
            token: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(refreshed.expires_in_secs.max(1)),
        });
        Ok(token)
    }

    /// Perform a call: refresh-on-401 once, honour a short `Retry-After` once,
    /// then classify. Cancellation is checked before every await point.
    pub(crate) async fn request(
        &self,
        request: ApiRequest,
        cancel: &CancellationToken,
    ) -> Result<ApiResponse, SpotifyError> {
        if cancel.is_cancelled() {
            return Err(SpotifyError::Cancelled);
        }
        // Method + static path only: query values and bodies can carry the
        // owner's search text, which does not belong in a log line.
        tracing::debug!(
            method = request.method.as_str(),
            path = request.path,
            "spotify web api call"
        );
        let token = self.access_token(cancel, false).await?;
        let mut response = self
            .transport
            .call(&token, request.clone(), cancel.clone())
            .await?;

        if response.status == 401 {
            // The cached token died early (revocation, clock skew). One forced
            // refresh, one retry — never a loop.
            let token = self.access_token(cancel, true).await?;
            response = self
                .transport
                .call(&token, request.clone(), cancel.clone())
                .await?;
        }

        if response.status == 429 {
            let wait = Duration::from_secs(response.retry_after_secs.unwrap_or(1));
            if wait > MAX_RETRY_AFTER {
                return Err(SpotifyError::RateLimited {
                    retry_after_secs: wait.as_secs(),
                });
            }
            tracing::debug!(
                wait_secs = wait.as_secs(),
                "spotify rate limited; honouring Retry-After once"
            );
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(SpotifyError::Cancelled),
                () = tokio::time::sleep(wait) => {}
            }
            let token = self.access_token(cancel, false).await?;
            response = self.transport.call(&token, request, cancel.clone()).await?;
        }

        classify(response)
    }

    // -- catalogue reads ---------------------------------------------------

    pub(crate) async fn search(
        &self,
        query: &str,
        types: &str,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Result<SearchHits, SpotifyError> {
        let mut request = ApiRequest::new(HttpMethod::Get, "/search")
            .query("q", query)
            .query("type", types)
            .query("limit", limit.to_string());
        if let Some(market) = &self.config.market {
            request = request.query("market", market);
        }
        let response = self.request(request, cancel).await?;
        parse_search(&response.body)
    }

    /// The owner's own saved playlists, bounded to [`MAX_PLAYLIST_PAGES`] pages.
    pub(crate) async fn own_playlists(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<PlaylistRef>, SpotifyError> {
        let mut all = Vec::new();
        for page in 0..MAX_PLAYLIST_PAGES {
            let request = ApiRequest::new(HttpMethod::Get, "/me/playlists")
                .query("limit", PLAYLIST_PAGE_LIMIT.to_string())
                .query("offset", (page * PLAYLIST_PAGE_LIMIT).to_string());
            let response = self.request(request, cancel).await?;
            let items = parse_playlist_page(&response.body)?;
            let exhausted = items.len() < PLAYLIST_PAGE_LIMIT;
            all.extend(items);
            if exhausted {
                break;
            }
        }
        Ok(all)
    }

    pub(crate) async fn devices(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<DeviceRef>, SpotifyError> {
        let response = self
            .request(
                ApiRequest::new(HttpMethod::Get, "/me/player/devices"),
                cancel,
            )
            .await?;
        parse_devices(&response.body)
    }

    /// The volume of **one named Connect device**, for an honest undo on the R2
    /// boost. Deliberately not `GET /me/player`: that reports whatever is
    /// *playing*, which need not be the device the grant targets, and an undo
    /// derived from the wrong device would restore a level that device never
    /// had. Absent state is not an error — it just means no undo (docs/06 §4:
    /// a compensating action is only worth recording when it is true).
    pub(crate) async fn device_volume(
        &self,
        device_id: &str,
        cancel: &CancellationToken,
    ) -> Option<VolumePct> {
        self.devices(cancel)
            .await
            .ok()?
            .into_iter()
            .find(|d| d.id.as_deref() == Some(device_id))?
            .volume_pct
    }

    /// Resolve a caller-supplied device name/alias/id to a Connect device id.
    /// `None` in means "wherever playback already is" — Spotify's own default.
    pub(crate) async fn resolve_device(
        &self,
        requested: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, SpotifyError> {
        let Some(raw) = requested else {
            return Ok(None);
        };
        let key = normalize(raw);
        if let Some(id) = self.config.device_aliases.get(&key) {
            return Ok(Some(id.clone()));
        }
        let devices = self.devices(cancel).await?;
        if let Some(found) = devices
            .iter()
            .find(|d| normalize(&d.name) == key || d.id.as_deref() == Some(raw))
        {
            let id = found.id.clone().ok_or(SpotifyError::NoActiveDevice)?;
            if !is_valid_device_id(&id) {
                return Err(SpotifyError::InvalidResponse);
            }
            return Ok(Some(id));
        }
        let available = devices
            .iter()
            .map(|d| short(&d.name))
            .collect::<Vec<_>>()
            .join(", ");
        Err(SpotifyError::DeviceNotFound {
            available: if available.is_empty() {
                "none".to_owned()
            } else {
                short(&available)
            },
        })
    }

    // -- playback writes ---------------------------------------------------

    pub(crate) async fn set_volume(
        &self,
        volume: VolumePct,
        device: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<(), SpotifyError> {
        self.request(
            ApiRequest::new(HttpMethod::Put, "/me/player/volume")
                .query("volume_percent", volume.get().to_string())
                .maybe_device(device),
            cancel,
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn set_shuffle(
        &self,
        state: bool,
        device: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<(), SpotifyError> {
        self.request(
            ApiRequest::new(HttpMethod::Put, "/me/player/shuffle")
                .query("state", state.to_string())
                .maybe_device(device),
            cancel,
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn play_context(
        &self,
        context_uri: &str,
        device: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<(), SpotifyError> {
        self.request(
            ApiRequest::new(HttpMethod::Put, "/me/player/play")
                .maybe_device(device)
                .json(serde_json::json!({ "context_uri": context_uri })),
            cancel,
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn play_uris(
        &self,
        uris: &[String],
        device: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<(), SpotifyError> {
        self.request(
            ApiRequest::new(HttpMethod::Put, "/me/player/play")
                .maybe_device(device)
                .json(serde_json::json!({ "uris": uris })),
            cancel,
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn queue(
        &self,
        uri: &str,
        device: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<(), SpotifyError> {
        self.request(
            ApiRequest::new(HttpMethod::Post, "/me/player/queue")
                .query("uri", uri)
                .maybe_device(device),
            cancel,
        )
        .await
        .map(|_| ())
    }

    // -- ADR-022 resolution ------------------------------------------------

    /// Resolve a free-text play request. **ADR-022 (1):** an artist-only match
    /// resolves to that artist's context — shuffled top tracks — with no
    /// clarifying question. Two *distinct* artists sharing the name is the only
    /// case that asks, and it asks once, fluently (ADR-016).
    pub(crate) async fn resolve_play_query(
        &self,
        query: &str,
        cancel: &CancellationToken,
    ) -> Result<PlayTarget, SpotifyError> {
        let hits = self
            .search(query, "artist,track", DEFAULT_SEARCH_LIMIT, cancel)
            .await?;
        resolve_play_target(query, &hits)
    }

    /// **ADR-022 (2):** the owner's own saved playlists are searched first;
    /// public catalogue search is a fallback, and the result says so.
    pub(crate) async fn resolve_playlist(
        &self,
        name: &str,
        cancel: &CancellationToken,
    ) -> Result<PlaylistMatch, SpotifyError> {
        let own = self.own_playlists(cancel).await?;
        match match_playlist(name, &own) {
            PlaylistLookup::One(found) => {
                return Ok(PlaylistMatch {
                    playlist: found,
                    from_library: true,
                });
            }
            PlaylistLookup::Ambiguous(question) => return Err(SpotifyError::Ambiguity(question)),
            PlaylistLookup::None => {}
        }

        let hits = self
            .search(name, "playlist", DEFAULT_SEARCH_LIMIT, cancel)
            .await?;
        let found = hits
            .playlists
            .into_iter()
            .next()
            .ok_or(SpotifyError::NoMatch)?;
        Ok(PlaylistMatch {
            playlist: found,
            from_library: false,
        })
    }
}

// ---------------------------------------------------------------------------
// Live HTTPS transport
// ---------------------------------------------------------------------------

/// The production transport. Reuses the workspace rustls stack (no new TLS
/// dependency), follows no redirects (both endpoints answer directly; a 3xx
/// would be anomalous), and streams every body under a hard byte cap.
pub struct HttpSpotifyTransport {
    client: reqwest::Client,
}

impl Default for HttpSpotifyTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpSpotifyTransport {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("static reqwest spotify-client config is valid"),
        }
    }

    /// Send a prepared request under cancellation, returning status, capped
    /// body, and `Retry-After`. Provider errors are collapsed to
    /// [`SpotifyError::Transport`] — a raw reqwest error embeds the request URL
    /// and can embed header context (invariant #5).
    pub(crate) async fn send(
        &self,
        builder: reqwest::RequestBuilder,
        cancel: CancellationToken,
    ) -> Result<ApiResponse, SpotifyError> {
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(SpotifyError::Cancelled),
            sent = builder.send() => match sent {
                Ok(response) => response,
                Err(error) if error.is_timeout() => return Err(SpotifyError::Timeout),
                Err(_) => return Err(SpotifyError::Transport),
            },
        };
        let status = response.status().as_u16();
        let retry_after_secs = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok());
        let body = read_body_capped(response, &cancel).await?;
        Ok(ApiResponse {
            status,
            body,
            retry_after_secs,
        })
    }
}

/// Stream a body under a hard cap, cancellable per chunk, bounded even if the
/// server lies about `Content-Length`.
async fn read_body_capped(
    mut response: reqwest::Response,
    cancel: &CancellationToken,
) -> Result<String, SpotifyError> {
    let mut body: Vec<u8> = Vec::new();
    loop {
        let chunk = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(SpotifyError::Cancelled),
            next = response.chunk() => next.map_err(|_| SpotifyError::Transport)?,
        };
        match chunk {
            Some(bytes) => {
                let remaining = MAX_BODY_BYTES.saturating_sub(body.len());
                body.extend_from_slice(&bytes[..remaining.min(bytes.len())]);
                if body.len() >= MAX_BODY_BYTES {
                    break;
                }
            }
            None => break,
        }
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

#[derive(serde::Deserialize)]
struct TokenEnvelope {
    access_token: String,
    #[serde(default)]
    expires_in: u64,
    refresh_token: Option<String>,
}

#[async_trait]
impl SpotifyTransport for HttpSpotifyTransport {
    async fn refresh_access_token(
        &self,
        client_id: &str,
        refresh_token: &str,
        cancel: CancellationToken,
    ) -> Result<TokenResponse, SpotifyError> {
        // PKCE public client: the refresh grant carries the client id and the
        // refresh token in the *body*, never in the URL — a query string can
        // reach access logs and process listings (invariant #5).
        let builder = self.client.post(TOKEN_ENDPOINT).form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ]);
        let response = self.send(builder, cancel).await?;
        if !response.is_success() {
            // 400 `invalid_grant` is the revoked/expired case; anything else is
            // still an auth failure from the caller's point of view.
            return Err(SpotifyError::AuthExpired);
        }
        let parsed: TokenEnvelope =
            serde_json::from_str(&response.body).map_err(|_| SpotifyError::InvalidResponse)?;
        Ok(TokenResponse {
            access_token: AccessToken::new(parsed.access_token),
            expires_in_secs: parsed.expires_in,
            rotated_refresh_token: parsed.refresh_token,
        })
    }

    async fn call(
        &self,
        token: &AccessToken,
        request: ApiRequest,
        cancel: CancellationToken,
    ) -> Result<ApiResponse, SpotifyError> {
        let url = format!("{API_BASE}{}", request.path);
        let mut builder = match request.method {
            HttpMethod::Get => self.client.get(url),
            HttpMethod::Post => self.client.post(url),
            HttpMethod::Put => self.client.put(url),
        }
        .query(&request.query)
        .bearer_auth(token.expose());
        if let Some(body) = request.body {
            builder = builder
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        self.send(builder, cancel).await
    }
}
