use std::sync::Arc;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::media::VolumePct;
use jarvis_domain::policy::ToolPolicy;
use jarvis_domain::tools::{
    CanonicalValue, ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion,
};
use tokio_util::sync::CancellationToken;

use super::*;

/// `spotify.volume` — **R1**: set a Connect device's volume **at or below** the
/// configured cap. There is no argument to this tool that produces an above-cap
/// level: it fails closed and names the approved path (the M3a split, forced by
/// `policy::evaluate` not inspecting arguments — docs/06 §3).
pub struct SpotifyVolumeTool {
    client: Arc<SpotifyClient>,
}

impl SpotifyVolumeTool {
    pub fn new(client: Arc<SpotifyClient>) -> Self {
        Self { client }
    }

    pub fn id() -> ToolId {
        "spotify.volume".parse().expect("static tool id is valid")
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

    fn parse(&self, arguments: &CanonicalValue) -> Result<(VolumePct, Option<String>), ToolError> {
        let map = object(arguments)?;
        let level = volume_arg(map)?;
        enforce_cap(level, self.client.max_volume())?;
        Ok((level, optional_text(map, "device")?))
    }
}

#[async_trait]
impl ToolExecutor for SpotifyVolumeTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R1: auto-authorized, never carries a grant.
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let (level, device) = self.parse(&invocation.arguments)?;
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let target = self
            .client
            .resolve_device(device.as_deref(), &cancel)
            .await
            .map_err(SpotifyError::into_tool_error)?;
        self.client
            .set_volume(level, target.as_deref(), &cancel)
            .await
            .map_err(SpotifyError::into_tool_error)?;
        ok(
            match &device {
                Some(name) => format!("Set Spotify volume on {} to {level}.", short(name)),
                None => format!("Set Spotify volume to {level}."),
            },
            None,
        )
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        self.parse(arguments).map(|_| ())
    }
}
