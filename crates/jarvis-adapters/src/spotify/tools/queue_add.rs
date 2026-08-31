use std::sync::Arc;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::declare_tool_id;
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::ToolPolicy;
use jarvis_domain::tools::{CanonicalValue, ToolError, ToolInvocation, ToolResult, ToolVersion};
use tokio_util::sync::CancellationToken;

use super::super::*;
use super::*;

/// `spotify.queue_add` — **R1**: appending to the play queue is reversible in
/// practice (skip it) and touches no saved library object. Only tracks are
/// queueable, so a free-text query resolves against tracks only.
pub struct SpotifyQueueAddTool {
    client: Arc<SpotifyClient>,
}

impl SpotifyQueueAddTool {
    pub fn new(client: Arc<SpotifyClient>) -> Self {
        Self { client }
    }

    declare_tool_id!("spotify.queue_add");

    pub fn policy() -> ToolPolicy {
        SpotifyPlayTool::policy()
    }

    pub fn descriptor(client: Arc<SpotifyClient>) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: Arc::new(Self::new(client)),
        }
    }
}

#[async_trait]
impl ToolExecutor for SpotifyQueueAddTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R1: auto-authorized, never carries a grant.
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let args = TargetArgs::parse(&invocation.arguments)?;
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let (uri, label) = match (&args.uri, &args.query) {
            (Some(raw), _) => {
                let (kind, uri) = parse_uri(raw).expect("validated in parse");
                if kind != "track" {
                    return Err(ToolError::ExecutionFailed(
                        "only a track can be queued".to_owned(),
                    ));
                }
                (uri.clone(), uri)
            }
            (None, Some(query)) => {
                let hits = self
                    .client
                    .search(query, "track", DEFAULT_SEARCH_LIMIT, &cancel)
                    .await
                    .map_err(SpotifyError::into_tool_error)?;
                let track = hits
                    .tracks
                    .first()
                    .ok_or_else(|| SpotifyError::NoMatch.into_tool_error())?;
                (track.uri.clone(), track_label(track))
            }
            (None, None) => unreachable!("TargetArgs::parse requires one of uri/query"),
        };

        let device = self
            .client
            .resolve_device(args.device.as_deref(), &cancel)
            .await
            .map_err(SpotifyError::into_tool_error)?;
        self.client
            .queue(&uri, device.as_deref(), &cancel)
            .await
            .map_err(SpotifyError::into_tool_error)?;
        ok(format!("Queued {label} on Spotify."), None)
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        TargetArgs::parse(arguments).map(|_| ())
    }
}
