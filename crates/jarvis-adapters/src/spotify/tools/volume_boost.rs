use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::media::VolumePct;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{
    CanonicalValue, ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion,
};
use tokio_util::sync::CancellationToken;

use super::super::*;
use super::*;

/// `spotify.volume_boost` — **R2**: a level *above* the configured cap. Parks
/// for explicit approval and executes only against a grant whose argument hash
/// matches, because a sudden loud speaker is not meaningfully reversible
/// (docs/02 §11a: "hearing protection is a real reversibility question").
///
/// `device` is **required** here (unlike the R1 tool) so the approved arguments
/// name the target and the grant binds it — otherwise the human approves "95%"
/// and the effect can land wherever playback happens to be when the grant is
/// consumed (the M3a `media.volume_boost` rule).
pub struct SpotifyVolumeBoostTool {
    client: Arc<SpotifyClient>,
}

impl SpotifyVolumeBoostTool {
    pub fn new(client: Arc<SpotifyClient>) -> Self {
        Self { client }
    }

    pub fn id() -> ToolId {
        "spotify.volume_boost"
            .parse()
            .expect("static tool id is valid")
    }

    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R2,
            is_reversible: false,
            requires_user_presence: true,
            timeout: REQUEST_TIMEOUT,
            required_scopes: [Scope::new(CONTROL_SCOPE).expect("static scope is valid")]
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

    /// Shape rules applied identically at execution and at approval-binding
    /// time: the level must be **above** the cap (never solicit an approval the
    /// R1 tool already covers — approval fatigue is a control weakness) and the
    /// device must be named.
    fn parse(&self, arguments: &CanonicalValue) -> Result<(VolumePct, String), ToolError> {
        let map = object(arguments)?;
        let level = volume_arg(map)?;
        if level.within_cap(self.client.max_volume()) {
            return Err(ToolError::SchemaInvalid(format!(
                "{level} is within the {} cap; use spotify.volume",
                self.client.max_volume()
            )));
        }
        let device = optional_text(map, "device")?.ok_or_else(|| {
            ToolError::SchemaInvalid(
                "spotify.volume_boost requires an explicit `device`".to_owned(),
            )
        })?;
        Ok((level, device))
    }
}

#[async_trait]
impl ToolExecutor for SpotifyVolumeBoostTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        grant: Option<ExecutionGrant>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let (level, device) = self.parse(&invocation.arguments)?;
        check_grant(grant.as_ref(), &invocation, SystemTime::now())?;

        let target = self
            .client
            .resolve_device(Some(device.as_str()), &cancel)
            .await
            .map_err(SpotifyError::into_tool_error)?;
        // Read the level we are replacing *on the target device* so the
        // timeline carries a real undo, not a canned string and not the level
        // of whatever happens to be playing elsewhere. `device` is required, so
        // `resolve_device` always yields an id here; the `None` arm cannot
        // record an honest undo and therefore records none.
        let previous = match target.as_deref() {
            Some(id) => self.client.device_volume(id, &cancel).await,
            None => None,
        };
        self.client
            .set_volume(level, target.as_deref(), &cancel)
            .await
            .map_err(SpotifyError::into_tool_error)?;
        ok(
            format!("Set Spotify volume on {} to {level}.", short(&device)),
            previous.map(|p| format!("Set Spotify volume on {} back to {p}.", short(&device))),
        )
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        self.parse(arguments).map(|_| ())
    }
}
