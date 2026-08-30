use std::sync::Arc;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::ToolPolicy;
use jarvis_domain::tools::{
    CanonicalValue, ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion,
};
use tokio_util::sync::CancellationToken;

use super::*;

/// `spotify.play_playlist { name }` — **R1**, same reasoning as
/// [`SpotifyPlayTool`]: it starts playback, it changes no library.
///
/// ADR-022 (2): the owner's **own** saved playlists are matched first; the
/// public catalogue is a fallback and the result says so, so "play my running
/// playlist" cannot silently start a stranger's.
pub struct SpotifyPlayPlaylistTool {
    client: Arc<SpotifyClient>,
}

impl SpotifyPlayPlaylistTool {
    pub fn new(client: Arc<SpotifyClient>) -> Self {
        Self { client }
    }

    pub fn id() -> ToolId {
        "spotify.play_playlist"
            .parse()
            .expect("static tool id is valid")
    }

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

    fn parse(arguments: &CanonicalValue) -> Result<(String, Option<String>), ToolError> {
        let map = object(arguments)?;
        Ok((required_text(map, "name")?, optional_text(map, "device")?))
    }
}

#[async_trait]
impl ToolExecutor for SpotifyPlayPlaylistTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R1: auto-authorized, never carries a grant.
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let (name, device) = Self::parse(&invocation.arguments)?;
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let found = self
            .client
            .resolve_playlist(&name, &cancel)
            .await
            .map_err(SpotifyError::into_tool_error)?;
        let device = self
            .client
            .resolve_device(device.as_deref(), &cancel)
            .await
            .map_err(SpotifyError::into_tool_error)?;
        self.client
            .play_context(&found.playlist.uri, device.as_deref(), &cancel)
            .await
            .map_err(SpotifyError::into_tool_error)?;

        let label = short(&found.playlist.name);
        ok(
            if found.from_library {
                format!("Playing your playlist \"{label}\" on Spotify.")
            } else {
                format!(
                    "Playing the public playlist \"{label}\" on Spotify — it isn't in your library."
                )
            },
            Some("Pause Spotify playback.".to_owned()),
        )
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        Self::parse(arguments).map(|_| ())
    }
}
