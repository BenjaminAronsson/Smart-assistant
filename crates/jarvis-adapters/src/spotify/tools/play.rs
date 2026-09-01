use std::sync::Arc;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::declare_tool_id;
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::media::VolumePct;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, SpeechSensitivity, ToolPolicy};
use jarvis_domain::tools::{CanonicalValue, ToolError, ToolInvocation, ToolResult, ToolVersion};
use tokio_util::sync::CancellationToken;

use super::super::*;
use super::*;

/// `spotify.play` — **R1** (docs/06 §3 "reversible low impact"): starting
/// playback is undone by pausing, and nothing outside the owner's own account
/// changes. Auto-authorized within scope, shown live.
///
/// The optional `volume_pct` is checked against the configured cap **before any
/// network call** ([`enforce_cap`]); above-cap levels have no path through this
/// tool at all — they live in the R2 [`SpotifyVolumeBoostTool`].
pub struct SpotifyPlayTool {
    client: Arc<SpotifyClient>,
}

impl SpotifyPlayTool {
    pub fn new(client: Arc<SpotifyClient>) -> Self {
        Self { client }
    }

    declare_tool_id!("spotify.play");

    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R1,
            is_reversible: true,
            requires_user_presence: false,
            timeout: REQUEST_TIMEOUT,
            required_scopes: [Scope::new(CONTROL_SCOPE).expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::External,
            speech_sensitivity: SpeechSensitivity::Normal,
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

    fn parse(
        &self,
        arguments: &CanonicalValue,
    ) -> Result<(TargetArgs, Option<VolumePct>), ToolError> {
        let target = TargetArgs::parse(arguments)?;
        let map = object(arguments)?;
        let volume = match optional_int(map, "volume_pct")? {
            Some(_) => Some(volume_arg(map)?),
            None => None,
        };
        if let Some(level) = volume {
            enforce_cap(level, self.client.max_volume())?;
        }
        Ok((target, volume))
    }
}

#[async_trait]
impl ToolExecutor for SpotifyPlayTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R1: auto-authorized, never carries a grant.
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        // Cap first: a refused level must cost zero Spotify calls.
        let (args, volume) = self.parse(&invocation.arguments)?;
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let target = match (&args.uri, &args.query) {
            (Some(uri), _) => {
                let (kind, uri) = parse_uri(uri).expect("validated in parse");
                match kind {
                    "artist" => PlayTarget::ArtistContext {
                        label: uri.clone(),
                        uri,
                    },
                    "track" => PlayTarget::Tracks {
                        label: uri.clone(),
                        uris: vec![uri],
                    },
                    _ => PlayTarget::Context {
                        label: uri.clone(),
                        uri,
                    },
                }
            }
            (None, Some(query)) => self
                .client
                .resolve_play_query(query, &cancel)
                .await
                .map_err(SpotifyError::into_tool_error)?,
            (None, None) => unreachable!("TargetArgs::parse requires one of uri/query"),
        };

        let device = self
            .client
            .resolve_device(args.device.as_deref(), &cancel)
            .await
            .map_err(SpotifyError::into_tool_error)?;

        // Set the (already capped) volume *before* starting playback so nothing
        // ever plays at the old level first.
        if let Some(level) = volume {
            self.client
                .set_volume(level, device.as_deref(), &cancel)
                .await
                .map_err(SpotifyError::into_tool_error)?;
        }

        let content = match &target {
            PlayTarget::ArtistContext { uri, label } => {
                // ADR-022 (1): the artist's own context, shuffled — Spotify's
                // top-tracks/artist-radio behaviour.
                self.client
                    .set_shuffle(true, device.as_deref(), &cancel)
                    .await
                    .map_err(SpotifyError::into_tool_error)?;
                self.client
                    .play_context(uri, device.as_deref(), &cancel)
                    .await
                    .map_err(SpotifyError::into_tool_error)?;
                format!("Playing {label} on Spotify — shuffled top tracks.")
            }
            PlayTarget::Context { uri, label } => {
                self.client
                    .play_context(uri, device.as_deref(), &cancel)
                    .await
                    .map_err(SpotifyError::into_tool_error)?;
                format!("Playing {label} on Spotify.")
            }
            PlayTarget::Tracks { uris, label } => {
                self.client
                    .play_uris(uris, device.as_deref(), &cancel)
                    .await
                    .map_err(SpotifyError::into_tool_error)?;
                format!("Playing {label} on Spotify.")
            }
        };
        ok(
            match &args.device {
                Some(name) => format!("{content} (on {})", short(name)),
                None => content,
            },
            Some("Pause Spotify playback.".to_owned()),
        )
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        self.parse(arguments).map(|_| ())
    }
}
