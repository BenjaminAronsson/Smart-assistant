use std::sync::Arc;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::declare_tool_id;
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{CanonicalValue, ToolError, ToolInvocation, ToolResult, ToolVersion};
use tokio_util::sync::CancellationToken;

use super::super::*;
use super::*;

/// `spotify.search` — **R0** read-only catalogue search (docs/06 §3: "read
/// status … automatic within scope; audited"). Mutates nothing, but the query
/// leaves the host to Spotify, so egress is honestly `External`.
pub struct SpotifySearchTool {
    client: Arc<SpotifyClient>,
}

impl SpotifySearchTool {
    pub fn new(client: Arc<SpotifyClient>) -> Self {
        Self { client }
    }

    declare_tool_id!("spotify.search");

    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R0,
            is_reversible: true,
            requires_user_presence: false,
            timeout: REQUEST_TIMEOUT,
            required_scopes: [Scope::new(SEARCH_SCOPE).expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::External,
        }
    }

    pub fn descriptor(client: Arc<SpotifyClient>) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: Arc::new(Self::new(client)),
        }
    }

    fn parse(arguments: &CanonicalValue) -> Result<(String, String, i64), ToolError> {
        let map = object(arguments)?;
        let query = required_text(map, "query")?;
        let types = match optional_text(map, "types")? {
            Some(raw) => {
                let mut kinds = Vec::new();
                for kind in raw.split(',').map(str::trim) {
                    match kind {
                        "track" | "artist" | "album" | "playlist" => kinds.push(kind),
                        _ => {
                            return Err(ToolError::SchemaInvalid(
                                "`types` must be a comma list of track|artist|album|playlist"
                                    .to_owned(),
                            ));
                        }
                    }
                }
                if kinds.is_empty() {
                    return Err(ToolError::SchemaInvalid("`types` is empty".to_owned()));
                }
                kinds.join(",")
            }
            None => "track,artist,album,playlist".to_owned(),
        };
        let limit = optional_int(map, "limit")?.unwrap_or(DEFAULT_SEARCH_LIMIT);
        if !(1..=MAX_SEARCH_LIMIT).contains(&limit) {
            return Err(ToolError::SchemaInvalid(format!(
                "`limit` must be between 1 and {MAX_SEARCH_LIMIT}"
            )));
        }
        Ok((query, types, limit))
    }
}

#[async_trait]
impl ToolExecutor for SpotifySearchTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R0: auto-authorized, never carries a grant.
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let (query, types, limit) = Self::parse(&invocation.arguments)?;
        let hits = self
            .client
            .search(&query, &types, limit, &cancel)
            .await
            .map_err(SpotifyError::into_tool_error)?;
        ok(render_hits(&hits), None)
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        Self::parse(arguments).map(|_| ())
    }
}

/// Render search hits. Every Spotify-supplied string is sanitised first — a
/// track title is third-party content that the model will read (Z4, docs/06 §5).
fn render_hits(hits: &SearchHits) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let section = |title: &str, lines: Vec<String>, out: &mut String| {
        if lines.is_empty() {
            return;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = writeln!(out, "{title}:");
        for line in lines {
            let _ = writeln!(out, "  {line}");
        }
    };

    section(
        "Artists",
        hits.artists
            .iter()
            .map(|a| format!("{} ({})", short(&a.name), a.uri))
            .collect(),
        &mut out,
    );
    section(
        "Tracks",
        hits.tracks
            .iter()
            .map(|t| format!("{} ({})", track_label(t), t.uri))
            .collect(),
        &mut out,
    );
    section(
        "Albums",
        hits.albums
            .iter()
            .map(|a| format!("{} ({})", track_label(a), a.uri))
            .collect(),
        &mut out,
    );
    section(
        "Playlists",
        hits.playlists
            .iter()
            .map(|p| match &p.owner {
                Some(owner) => format!("{} by {} ({})", short(&p.name), short(owner), p.uri),
                None => format!("{} ({})", short(&p.name), p.uri),
            })
            .collect(),
        &mut out,
    );

    if out.is_empty() {
        "Nothing on Spotify matched that.".to_owned()
    } else {
        out
    }
}
