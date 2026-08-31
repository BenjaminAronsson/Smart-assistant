//! Media tools (F3a.7, FR-22, docs/02 §11a, ADR-012): the **model's** path to
//! local playback control, registered in the standard catalogue with host-owned
//! policy — "no special path" (docs/02 §11a).
//!
//! Three tools. The first two are split because the risk tier differs by
//! *action class*, and `policy::evaluate` classifies a proposal by the
//! registered tool's [`ToolPolicy`] — not by its arguments (docs/06 §3):
//!
//! * [`MediaPlaybackTool`] (`media.playback`, **R1**) — transport verbs and
//!   volume **at or below** the configured cap. Reversible, local-only,
//!   auto-authorized within scope. It **cannot** raise volume above the cap:
//!   an above-cap request fails closed and names the R2 tool.
//! * [`MediaVolumeBoostTool`] (`media.volume_boost`, **R2**) — volume above the
//!   cap. Parks for explicit human approval and executes only against a
//!   validated grant, because sudden loudness is not meaningfully reversible
//!   (docs/02 §11a: "hearing protection is a real reversibility question").
//! * [`MediaOpenUrlTool`] (`media.open_url`, **R1**) — cast-a-link: open web
//!   video in the dedicated credential-free media window (ADR-012).
//!
//! Splitting the tools is what makes the cap a *policy* boundary rather than an
//! executor courtesy: the R1 tool has no code path to an above-cap level, so
//! nothing the model says can produce one without an approval.
//!
//! **Ambiguity is asked about, never guessed.** With two players playing, an
//! untargeted command returns the ADR-016 single fluent question
//! (`synthesis::clarifying_question`) instead of picking one — the same
//! mechanism as every other ambiguity in the system, not a media-specific
//! picker (media-integration skill §2).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_application::ports::{MediaController, MediaError};
use jarvis_domain::declare_tool_id;
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::media::{
    MediaSnapshot, PlayerId, TargetSelection, TransportCommand, TransportCommandError, VolumePct,
};
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::synthesis::clarifying_question;
use jarvis_domain::tools::{CanonicalValue, ToolError, ToolInvocation, ToolResult, ToolVersion};
use tokio_util::sync::CancellationToken;

/// The scope both media tools require. Absent from a run's granted scopes,
/// `policy::evaluate` rejects the proposal before any executor runs.
const MEDIA_SCOPE: &str = "media:control";

/// `media.playback` — R1 transport control and volume within the cap.
pub struct MediaPlaybackTool {
    controller: Arc<dyn MediaController>,
    max_volume: VolumePct,
}

impl MediaPlaybackTool {
    pub fn new(controller: Arc<dyn MediaController>, max_volume: VolumePct) -> Self {
        Self {
            controller,
            max_volume,
        }
    }

    declare_tool_id!("media.playback");

    /// Host-owned policy: **R1**, reversible, local egress. Reversible is
    /// honest here — every verb has an immediate opposite and nothing leaves
    /// the machine — which is exactly why transport control auto-authorizes
    /// while an above-cap volume does not.
    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R1,
            is_reversible: true,
            requires_user_presence: false,
            timeout: Duration::from_secs(5),
            required_scopes: [Scope::new(MEDIA_SCOPE).expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::Local,
        }
    }

    pub fn descriptor(
        controller: Arc<dyn MediaController>,
        max_volume: VolumePct,
    ) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: Arc::new(Self::new(controller, max_volume)),
        }
    }
}

#[async_trait]
impl ToolExecutor for MediaPlaybackTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R1: auto-authorized, never carries a grant.
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let args = MediaArgs::parse(&invocation.arguments)?;
        let snapshot = self
            .controller
            .snapshot(cancel.clone())
            .await
            .map_err(media_error)?;

        // "set_volume" is the one verb whose *argument* decides whether this
        // tool may act at all.
        if args.command == "set_volume" {
            let requested = args.volume()?;
            if !requested.within_cap(self.max_volume) {
                // Fail closed, and tell the model where the authorized path is.
                // No effect happens on this call — the human decides on the R2
                // card or not at all.
                return Err(ToolError::Denied(format!(
                    "{requested} is above the {} volume cap; propose media.volume_boost \
                     (needs approval) instead",
                    self.max_volume
                )));
            }
            let (player, label) = resolve_target(&snapshot, args.player.as_deref())?;
            self.controller
                .set_volume(&player, requested, cancel)
                .await
                .map_err(media_error)?;
            return Ok(ToolResult {
                content: format!("Set {label} volume to {requested}."),
                truncated: false,
                compensation: None,
            });
        }

        let command =
            TransportCommand::parse(&args.command, args.offset_secs).map_err(transport_error)?;
        let (player, label) = resolve_target(&snapshot, args.player.as_deref())?;
        self.controller
            .transport(&player, command, cancel)
            .await
            .map_err(media_error)?;

        Ok(ToolResult {
            content: describe(command, &label),
            truncated: false,
            compensation: compensation_for(command, &label),
        })
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        let args = MediaArgs::parse(arguments)
            .map_err(|e| ToolError::SchemaInvalid(short(&e.to_string())))?;
        if args.command == "set_volume" {
            let requested = args
                .volume()
                .map_err(|e| ToolError::SchemaInvalid(short(&e.to_string())))?;
            if !requested.within_cap(self.max_volume) {
                // Refuse at binding time too: an edited argument must not be
                // able to slip an above-cap level into this R1 tool.
                return Err(ToolError::SchemaInvalid(format!(
                    "{requested} is above the {} volume cap",
                    self.max_volume
                )));
            }
        } else {
            TransportCommand::parse(&args.command, args.offset_secs)
                .map_err(|e| ToolError::SchemaInvalid(short(&e.to_string())))?;
        }
        Ok(())
    }
}

/// `media.volume_boost` — **R2**: raise volume above the configured cap. Parks
/// for explicit approval; the approval card shows the exact level.
pub struct MediaVolumeBoostTool {
    controller: Arc<dyn MediaController>,
    max_volume: VolumePct,
}

impl MediaVolumeBoostTool {
    pub fn new(controller: Arc<dyn MediaController>, max_volume: VolumePct) -> Self {
        Self {
            controller,
            max_volume,
        }
    }

    declare_tool_id!("media.volume_boost");

    /// Host-owned policy: **R2**, **not** reversible (that is the whole point —
    /// you cannot un-hear a sudden 100%), local egress, user presence required.
    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R2,
            is_reversible: false,
            requires_user_presence: true,
            timeout: Duration::from_secs(5),
            required_scopes: [Scope::new(MEDIA_SCOPE).expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::Local,
        }
    }

    pub fn descriptor(
        controller: Arc<dyn MediaController>,
        max_volume: VolumePct,
    ) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: Arc::new(Self::new(controller, max_volume)),
        }
    }
}

#[async_trait]
impl ToolExecutor for MediaVolumeBoostTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R2: validated + consumed by the orchestrator before we run.
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let args = MediaArgs::parse(&invocation.arguments)?;
        boost_shape(&args, self.max_volume)?;
        let requested = args.volume()?;
        let snapshot = self
            .controller
            .snapshot(cancel.clone())
            .await
            .map_err(media_error)?;
        // `player` is REQUIRED here (unlike the R1 tool), so the approved
        // arguments name the target and the grant's argument hash binds it. With
        // an ambient target, the human could approve "95% on Spotify" and the
        // effect could land on whatever happened to be playing when the grant
        // was consumed.
        let (player, label) = resolve_target(&snapshot, Some(args.require_player()?))?;
        // Capture the level we are replacing so the run timeline carries a real
        // compensation, not a canned string.
        let previous = snapshot.get(&player).and_then(|p| p.volume);
        self.controller
            .set_volume(&player, requested, cancel)
            .await
            .map_err(media_error)?;

        Ok(ToolResult {
            content: format!("Set {label} volume to {requested}."),
            truncated: false,
            compensation: previous.map(|p| format!("Set {label} volume back to {p}.")),
        })
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        let args = MediaArgs::parse(arguments)
            .map_err(|e| ToolError::SchemaInvalid(short(&e.to_string())))?;
        boost_shape(&args, self.max_volume)
            .map_err(|e| ToolError::SchemaInvalid(short(&e.to_string())))?;
        args.require_player()
            .map_err(|e| ToolError::SchemaInvalid(short(&e.to_string())))?;
        Ok(())
    }
}

/// Shape rules specific to the R2 boost tool, applied identically at execution
/// and at approval-binding time so an edited argument set cannot take a
/// different path than the proposal did.
///
/// Two refusals, both about what the human actually approved:
/// * the verb must be `set_volume` — the executor only ever sets a volume, so a
///   proposal saying `pause` would render "pause" on the approval card while
///   setting a volume. The model must not control text that misdescribes the
///   effect (docs/06 §3: the card shows the *exact* effect).
/// * the level must be **above** the cap — soliciting an R2 approval for
///   something the R1 tool already does trains the owner to approve routine
///   actions, and approval fatigue is a real control weakness.
fn boost_shape(args: &MediaArgs, max_volume: VolumePct) -> Result<(), ToolError> {
    if args.command != "set_volume" {
        return Err(ToolError::ExecutionFailed(format!(
            "media.volume_boost only sets volume, got command `{}`",
            short(&args.command)
        )));
    }
    let requested = args.volume()?;
    if requested.within_cap(max_volume) {
        return Err(ToolError::ExecutionFailed(format!(
            "{requested} is within the {max_volume} cap; use media.playback"
        )));
    }
    Ok(())
}

/// The shared argument shape of both media tools.
struct MediaArgs {
    command: String,
    player: Option<String>,
    offset_secs: Option<i64>,
    volume_pct: Option<i64>,
}

impl MediaArgs {
    fn parse(arguments: &CanonicalValue) -> Result<Self, ToolError> {
        let CanonicalValue::Object(map) = arguments else {
            return Err(ToolError::ExecutionFailed(
                "arguments must be an object".to_owned(),
            ));
        };
        let command = match map.get("command") {
            Some(CanonicalValue::Str(s)) => s.clone(),
            Some(_) => {
                return Err(ToolError::ExecutionFailed(
                    "argument `command` must be a string".to_owned(),
                ));
            }
            // `volume_pct` with no verb is unambiguously a volume set — the
            // boost tool's natural shape.
            None if map.contains_key("volume_pct") => "set_volume".to_owned(),
            None => {
                return Err(ToolError::ExecutionFailed(
                    "missing required argument `command`".to_owned(),
                ));
            }
        };
        let player = match map.get("player") {
            Some(CanonicalValue::Str(s)) => Some(s.clone()),
            Some(CanonicalValue::Null) | None => None,
            Some(_) => {
                return Err(ToolError::ExecutionFailed(
                    "argument `player` must be a string".to_owned(),
                ));
            }
        };
        Ok(Self {
            command,
            player,
            offset_secs: int_arg(map, "offset_secs")?,
            volume_pct: int_arg(map, "volume_pct")?,
        })
    }

    /// The explicitly named player. Required by the R2 boost tool so the grant
    /// binds the target (see its `execute`).
    fn require_player(&self) -> Result<&str, ToolError> {
        self.player.as_deref().ok_or_else(|| {
            ToolError::ExecutionFailed(
                "media.volume_boost requires an explicit `player`".to_owned(),
            )
        })
    }

    fn volume(&self) -> Result<VolumePct, ToolError> {
        let raw = self.volume_pct.ok_or_else(|| {
            ToolError::ExecutionFailed("missing required argument `volume_pct`".to_owned())
        })?;
        VolumePct::from_i64(raw).map_err(|e| ToolError::ExecutionFailed(short(&e.to_string())))
    }
}

fn int_arg(
    map: &std::collections::BTreeMap<String, CanonicalValue>,
    key: &str,
) -> Result<Option<i64>, ToolError> {
    match map.get(key) {
        Some(CanonicalValue::Int(n)) => Ok(Some(*n)),
        Some(CanonicalValue::Null) | None => Ok(None),
        Some(_) => Err(ToolError::ExecutionFailed(format!(
            "argument `{key}` must be an integer"
        ))),
    }
}

/// Resolve which player a command applies to, returning the id and a spoken
/// label. An explicit `player` must be a well-formed MPRIS name **and** be
/// present on the bus; an untargeted command uses the unambiguous active player
/// and otherwise asks (ADR-016), never guesses.
fn resolve_target(
    snapshot: &MediaSnapshot,
    requested: Option<&str>,
) -> Result<(PlayerId, String), ToolError> {
    if let Some(raw) = requested {
        let id =
            PlayerId::new(raw).map_err(|e| ToolError::ExecutionFailed(short(&e.to_string())))?;
        let state = snapshot
            .get(&id)
            .ok_or_else(|| ToolError::ExecutionFailed("that player is not running".to_owned()))?;
        return Ok((id, state.identity.clone()));
    }

    match snapshot.target() {
        TargetSelection::One(id) => {
            let label = snapshot
                .get(&id)
                .map(|s| s.identity.clone())
                .unwrap_or_else(|| id.short_name().to_owned());
            Ok((id, label))
        }
        TargetSelection::None => Err(ToolError::ExecutionFailed(
            "nothing is playing right now".to_owned(),
        )),
        TargetSelection::Ambiguous(ids) => {
            // One fluent spoken question naming the candidates — never a picker
            // (ADR-016). The labels are the players' sanitized identities.
            let labels: Vec<String> = ids
                .iter()
                .map(|id| {
                    snapshot
                        .get(id)
                        .map(|s| s.identity.clone())
                        .unwrap_or_else(|| id.short_name().to_owned())
                })
                .collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let question = clarifying_question(&refs)
                .unwrap_or_else(|| "Which player did you mean?".to_owned());
            Err(ToolError::ExecutionFailed(question))
        }
    }
}

fn describe(command: TransportCommand, label: &str) -> String {
    match command {
        TransportCommand::Play => format!("Resumed {label}."),
        TransportCommand::Pause => format!("Paused {label}."),
        TransportCommand::PlayPause => format!("Toggled playback on {label}."),
        TransportCommand::Stop => format!("Stopped {label}."),
        TransportCommand::Next => format!("Skipped to the next track on {label}."),
        TransportCommand::Previous => format!("Went back a track on {label}."),
        TransportCommand::Seek { offset_secs } if offset_secs < 0 => {
            format!("Rewound {label} by {}s.", offset_secs.abs())
        }
        TransportCommand::Seek { offset_secs } => format!("Skipped {label} ahead {offset_secs}s."),
    }
}

/// The compensating undo the orchestrator surfaces for a reversible R1 action.
/// Only the verbs with an exact inverse register one — "next" has no honest
/// undo (the previous track may not be where you were), so it registers none
/// rather than a plausible-looking lie.
fn compensation_for(command: TransportCommand, label: &str) -> Option<String> {
    match command {
        TransportCommand::Play => Some(format!("Pause {label} again.")),
        TransportCommand::Pause => Some(format!("Resume {label}.")),
        TransportCommand::Seek { offset_secs } => {
            Some(format!("Seek {label} by {}s.", -offset_secs))
        }
        TransportCommand::PlayPause
        | TransportCommand::Stop
        | TransportCommand::Next
        | TransportCommand::Previous => None,
    }
}

/// Map a controller failure to a tool error. Absence is a *clean* outcome
/// message, not a stack of D-Bus detail (invariant 5).
fn media_error(error: MediaError) -> ToolError {
    match error {
        MediaError::Cancelled => ToolError::Cancelled,
        MediaError::PlayerGone
        | MediaError::Unsupported
        | MediaError::Unavailable
        | MediaError::Failed(_) => ToolError::ExecutionFailed(short(&error.to_string())),
    }
}

fn transport_error(error: TransportCommandError) -> ToolError {
    ToolError::ExecutionFailed(short(&error.to_string()))
}

/// Bound and control-strip any diagnostic that could carry player-controlled
/// text before it reaches a tool result (invariant 5).
fn short(raw: &str) -> String {
    jarvis_domain::tools::sanitize_result_content(raw, 200).text
}

/// `media.open_url` — **cast-a-link** (FR-22, ADR-012, docs/02 §11a): open web
/// video in the dedicated, credential-free media window on the configured
/// display; from there MPRIS provides transport control.
///
/// **R1** per ADR-012, and the tiering deserves its reasoning stated: opening a
/// page is reversible (close the window), local in effect, and is the literal
/// thing the owner asked for when they said "put this on the TV". What makes
/// R1 defensible despite the window fetching a third-party URL is the isolation
/// the directive guarantees — own app-id, own profile directory, **no
/// credentials** — so a hostile URL reaching this tool loads an attacker page in
/// an empty browser profile rather than one carrying the owner's sessions. The
/// egress is nonetheless classified `External`: bytes do leave the machine.
///
/// The URL is `https`-only and is written **verbatim** into a durable audit
/// event *before* the window is opened (docs/02 §11a) — never a model paraphrase
/// of where it points, and never an effect that left no record. A cast that
/// cannot be audited does not happen (invariant 6, the same fail-closed reading
/// as the F3a.4 display placement).
pub struct MediaOpenUrlTool {
    profile: Arc<jarvis_domain::display::DisplayProfile>,
    sink: Arc<dyn jarvis_application::ports::MediaWindowSink>,
    audit: Arc<dyn jarvis_application::ports::AuditLog>,
}

impl MediaOpenUrlTool {
    pub fn new(
        profile: Arc<jarvis_domain::display::DisplayProfile>,
        sink: Arc<dyn jarvis_application::ports::MediaWindowSink>,
        audit: Arc<dyn jarvis_application::ports::AuditLog>,
    ) -> Self {
        Self {
            profile,
            sink,
            audit,
        }
    }

    declare_tool_id!("media.open_url");

    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R1,
            is_reversible: true,
            requires_user_presence: false,
            timeout: Duration::from_secs(10),
            required_scopes: [Scope::new(MEDIA_SCOPE).expect("static scope is valid")]
                .into_iter()
                .collect(),
            // The window fetches the URL: bytes leave the machine, so this is
            // honestly External even though the *control* is local.
            egress: DataEgress::External,
        }
    }

    pub fn descriptor(
        profile: Arc<jarvis_domain::display::DisplayProfile>,
        sink: Arc<dyn jarvis_application::ports::MediaWindowSink>,
        audit: Arc<dyn jarvis_application::ports::AuditLog>,
    ) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: Arc::new(Self::new(profile, sink, audit)),
        }
    }

    /// Validate a proposed URL. `https` only, single-line, bounded — the same
    /// rules the agent re-applies before it launches anything.
    fn validated_url(arguments: &CanonicalValue) -> Result<String, ToolError> {
        let url = crate::tools::required_str(arguments, "url")?;
        if url.len() > jarvis_domain::media::MAX_MEDIA_URL_BYTES {
            return Err(ToolError::ExecutionFailed(
                "that URL is too long".to_owned(),
            ));
        }
        if url.chars().any(char::is_control) {
            return Err(ToolError::ExecutionFailed(
                "a URL must not contain control characters".to_owned(),
            ));
        }
        if !jarvis_domain::media::is_https_url(url) {
            return Err(ToolError::ExecutionFailed(
                "only https URLs can be cast to the media window".to_owned(),
            ));
        }
        Ok(url.to_owned())
    }
}

#[async_trait]
impl ToolExecutor for MediaOpenUrlTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R1: auto-authorized, never carries a grant.
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let url = Self::validated_url(&invocation.arguments)?;

        // An explicit `display` overrides the profile; with neither, fail closed
        // rather than casting onto an arbitrary monitor (same rule as artifact
        // placement, F3a.4).
        let requested = match &invocation.arguments {
            CanonicalValue::Object(map) => match map.get("display") {
                Some(CanonicalValue::Str(s)) => Some(
                    jarvis_domain::display::MonitorId::new(s.clone())
                        .map_err(|e| ToolError::ExecutionFailed(short(&e.to_string())))?,
                ),
                _ => None,
            },
            _ => None,
        };
        let placement = self
            .profile
            .resolve(jarvis_domain::display::Surface::MediaWindow, requested)
            .ok_or_else(|| {
                ToolError::ExecutionFailed(
                    "no monitor is configured for the media window".to_owned(),
                )
            })?;

        // Durable audit BEFORE the window opens, carrying the URL verbatim
        // (docs/02 §11a). This is the feature's only external-egress action and
        // its only process launch: an unauditable cast must not happen.
        //
        // The actor is a placeholder until the coding/browser-style orchestrator
        // wiring lands and supplies the run's real actor + correlation id (the
        // same deferral as D-M3a-2/D-M3a-3).
        let event = jarvis_domain::audit::AuditEvent {
            occurred_at: std::time::SystemTime::now(),
            actor: "system".to_owned(),
            event_type: "media.cast".to_owned(),
            target: url.clone(),
            correlation_id: None,
            payload_json: serde_json::json!({
                "url": url,
                "monitor": placement.monitor.as_str(),
            })
            .to_string(),
        };
        self.audit.record(&event).await.map_err(|e| {
            tracing::error!(error = %e, "media cast audit failed; not opening the window");
            ToolError::ExecutionFailed("that cast could not be recorded".to_owned())
        })?;

        // The tool has no device vocabulary — casting names a *screen* only in
        // the deployment's terms — so the target is supplied by the host at the
        // sink boundary (`jarvisd::media_target`, M7 gate D-M7-2).
        let delivered = self.sink.open_url(&url, &placement.monitor, None).await;
        Ok(ToolResult {
            // The URL verbatim (docs/02 §11a) — never a paraphrase of where it
            // points.
            content: if delivered {
                format!("Opened {url} in the media window on {}.", placement.monitor)
            } else {
                format!(
                    "Queued {url} for the media window on {} — no desktop agent is connected.",
                    placement.monitor
                )
            },
            truncated: false,
            compensation: None,
        })
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        Self::validated_url(arguments)
            .map(|_| ())
            .map_err(|e| ToolError::SchemaInvalid(short(&e.to_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::media::{MPRIS_NAME_PREFIX, PlaybackStatus, PlayerState, TrackMetadata};
    use jarvis_domain::tools::ToolId;
    use jarvis_test_support::FakeAuditLog;
    use std::sync::Mutex;

    #[derive(Debug, PartialEq, Eq)]
    enum Applied {
        Transport(String, TransportCommand),
        Volume(String, u8),
    }

    struct FakeController {
        snapshot: MediaSnapshot,
        applied: Mutex<Vec<Applied>>,
        fail_with: Option<MediaError>,
    }

    impl FakeController {
        fn with(snapshot: MediaSnapshot) -> Arc<Self> {
            Arc::new(Self {
                snapshot,
                applied: Mutex::new(Vec::new()),
                fail_with: None,
            })
        }
        fn failing(error: MediaError) -> Arc<Self> {
            Arc::new(Self {
                snapshot: playing_snapshot(),
                applied: Mutex::new(Vec::new()),
                fail_with: Some(error),
            })
        }
        fn applied(&self) -> Vec<Applied> {
            std::mem::take(&mut self.applied.lock().unwrap())
        }
    }

    #[async_trait]
    impl MediaController for FakeController {
        async fn snapshot(&self, _cancel: CancellationToken) -> Result<MediaSnapshot, MediaError> {
            Ok(self.snapshot.clone())
        }
        async fn transport(
            &self,
            player: &PlayerId,
            command: TransportCommand,
            _cancel: CancellationToken,
        ) -> Result<(), MediaError> {
            if let Some(e) = &self.fail_with {
                return Err(e.clone());
            }
            self.applied
                .lock()
                .unwrap()
                .push(Applied::Transport(player.to_string(), command));
            Ok(())
        }
        async fn set_volume(
            &self,
            player: &PlayerId,
            volume: VolumePct,
            _cancel: CancellationToken,
        ) -> Result<(), MediaError> {
            if let Some(e) = &self.fail_with {
                return Err(e.clone());
            }
            self.applied
                .lock()
                .unwrap()
                .push(Applied::Volume(player.to_string(), volume.get()));
            Ok(())
        }
    }

    fn player(name: &str) -> PlayerId {
        PlayerId::new(format!("{MPRIS_NAME_PREFIX}{name}")).unwrap()
    }

    fn state(
        name: &str,
        identity: &str,
        status: PlaybackStatus,
        volume: Option<u8>,
    ) -> PlayerState {
        PlayerState::new(
            player(name),
            Some(identity),
            status,
            TrackMetadata::default(),
            volume.map(|v| VolumePct::new(v).unwrap()),
        )
    }

    fn playing_snapshot() -> MediaSnapshot {
        MediaSnapshot::new([state(
            "spotify",
            "Spotify",
            PlaybackStatus::Playing,
            Some(40),
        )])
    }

    fn two_playing() -> MediaSnapshot {
        MediaSnapshot::new([
            state("spotify", "Spotify", PlaybackStatus::Playing, Some(40)),
            state("chromium", "Chromium", PlaybackStatus::Playing, None),
        ])
    }

    fn cap() -> VolumePct {
        VolumePct::new(70).unwrap()
    }

    fn invocation(id: ToolId, args: Vec<(&'static str, CanonicalValue)>) -> ToolInvocation {
        ToolInvocation {
            tool_id: id,
            tool_version: ToolVersion::new(1, 0, 0),
            arguments: CanonicalValue::obj(args),
        }
    }

    fn playback(controller: Arc<FakeController>) -> MediaPlaybackTool {
        MediaPlaybackTool::new(controller, cap())
    }

    fn boost(controller: Arc<FakeController>) -> MediaVolumeBoostTool {
        MediaVolumeBoostTool::new(controller, cap())
    }

    // ---- policy ----------------------------------------------------------

    #[test]
    fn transport_is_r1_reversible_and_local_boost_is_r2() {
        let r1 = MediaPlaybackTool::policy();
        assert_eq!(r1.risk, RiskLevel::R1);
        assert!(r1.is_reversible);
        assert!(!r1.requires_grant(), "transport must auto-authorize");
        assert_eq!(r1.egress, DataEgress::Local);

        let r2 = MediaVolumeBoostTool::policy();
        assert_eq!(r2.risk, RiskLevel::R2);
        assert!(!r2.is_reversible, "you cannot un-hear a volume spike");
        assert!(
            r2.requires_grant(),
            "above-cap volume must park for approval"
        );
        assert_eq!(r2.egress, DataEgress::Local);

        // Both are gated behind the same scope, so a run without it reaches
        // neither.
        let scope = Scope::new(MEDIA_SCOPE).unwrap();
        assert!(r1.required_scopes.contains(&scope));
        assert!(r2.required_scopes.contains(&scope));
    }

    // ---- transport -------------------------------------------------------

    #[tokio::test]
    async fn pauses_the_single_playing_player_without_being_told_which() {
        let controller = FakeController::with(playing_snapshot());
        let result = playback(controller.clone())
            .execute(
                invocation(
                    MediaPlaybackTool::id(),
                    vec![("command", CanonicalValue::str("pause"))],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.content, "Paused Spotify.");
        assert_eq!(result.compensation.as_deref(), Some("Resume Spotify."));
        assert_eq!(
            controller.applied(),
            vec![Applied::Transport(
                "org.mpris.MediaPlayer2.spotify".into(),
                TransportCommand::Pause
            )]
        );
    }

    #[tokio::test]
    async fn two_playing_players_produce_one_spoken_question_and_no_effect() {
        let controller = FakeController::with(two_playing());
        let err = playback(controller.clone())
            .execute(
                invocation(
                    MediaPlaybackTool::id(),
                    vec![("command", CanonicalValue::str("pause"))],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        let ToolError::ExecutionFailed(question) = err else {
            panic!("expected a clarifying question, got {err:?}");
        };
        assert!(
            question.contains("Chromium") && question.contains("Spotify"),
            "{question}"
        );
        assert!(
            !question.contains('\n'),
            "the ask is one spoken line, not a picker"
        );
        assert!(
            controller.applied().is_empty(),
            "an ambiguous command must not act on either player"
        );
    }

    #[tokio::test]
    async fn an_explicit_player_is_honoured_and_an_absent_one_is_refused() {
        let controller = FakeController::with(two_playing());
        playback(controller.clone())
            .execute(
                invocation(
                    MediaPlaybackTool::id(),
                    vec![
                        ("command", CanonicalValue::str("next")),
                        (
                            "player",
                            CanonicalValue::str("org.mpris.MediaPlayer2.chromium"),
                        ),
                    ],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            controller.applied(),
            vec![Applied::Transport(
                "org.mpris.MediaPlayer2.chromium".into(),
                TransportCommand::Next
            )]
        );

        let err = playback(controller.clone())
            .execute(
                invocation(
                    MediaPlaybackTool::id(),
                    vec![
                        ("command", CanonicalValue::str("pause")),
                        ("player", CanonicalValue::str("org.mpris.MediaPlayer2.vlc")),
                    ],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(m) if m.contains("not running")));
        assert!(controller.applied().is_empty());
    }

    #[tokio::test]
    async fn a_malformed_player_name_never_reaches_the_controller() {
        let controller = FakeController::with(playing_snapshot());
        let err = playback(controller.clone())
            .execute(
                invocation(
                    MediaPlaybackTool::id(),
                    vec![
                        ("command", CanonicalValue::str("pause")),
                        (
                            "player",
                            CanonicalValue::str("org.mpris.MediaPlayer2.spotify\nNext"),
                        ),
                    ],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(_)), "got {err:?}");
        assert!(controller.applied().is_empty());
    }

    #[tokio::test]
    async fn nothing_playing_is_a_clean_answer_not_a_crash() {
        let controller = FakeController::with(MediaSnapshot::none());
        let err = playback(controller.clone())
            .execute(
                invocation(
                    MediaPlaybackTool::id(),
                    vec![("command", CanonicalValue::str("pause"))],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(m) if m.contains("nothing is playing")));
    }

    #[tokio::test]
    async fn a_player_that_quits_mid_command_reports_cleanly() {
        let controller = FakeController::failing(MediaError::PlayerGone);
        let err = playback(controller)
            .execute(
                invocation(
                    MediaPlaybackTool::id(),
                    vec![("command", CanonicalValue::str("pause"))],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(m) if m.contains("no longer running")));
    }

    #[tokio::test]
    async fn cancellation_propagates_as_cancelled_not_as_a_failure() {
        let controller = FakeController::failing(MediaError::Cancelled);
        let err = playback(controller)
            .execute(
                invocation(
                    MediaPlaybackTool::id(),
                    vec![("command", CanonicalValue::str("pause"))],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(err, ToolError::Cancelled);
    }

    #[tokio::test]
    async fn an_unknown_verb_is_rejected_before_it_reaches_the_bus() {
        let controller = FakeController::with(playing_snapshot());
        let err = playback(controller.clone())
            .execute(
                invocation(
                    MediaPlaybackTool::id(),
                    vec![("command", CanonicalValue::str("exec"))],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::ExecutionFailed(m) if m.contains("unknown transport verb"))
        );
        assert!(controller.applied().is_empty());
    }

    // ---- the volume cap --------------------------------------------------

    #[tokio::test]
    async fn volume_at_or_below_the_cap_is_applied_by_the_r1_tool() {
        let controller = FakeController::with(playing_snapshot());
        let result = playback(controller.clone())
            .execute(
                invocation(
                    MediaPlaybackTool::id(),
                    vec![
                        ("command", CanonicalValue::str("set_volume")),
                        ("volume_pct", CanonicalValue::Int(70)),
                    ],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.content, "Set Spotify volume to 70%.");
        assert_eq!(
            controller.applied(),
            vec![Applied::Volume("org.mpris.MediaPlayer2.spotify".into(), 70)]
        );
    }

    #[tokio::test]
    async fn the_r1_tool_cannot_raise_volume_above_the_cap() {
        // The security property of the split: no argument to the auto-authorized
        // tool produces an above-cap effect. It fails closed and names the
        // approved path.
        let controller = FakeController::with(playing_snapshot());
        let err = playback(controller.clone())
            .execute(
                invocation(
                    MediaPlaybackTool::id(),
                    vec![
                        ("command", CanonicalValue::str("set_volume")),
                        ("volume_pct", CanonicalValue::Int(85)),
                    ],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        let ToolError::Denied(message) = err else {
            panic!("above-cap must be denied, got {err:?}");
        };
        assert!(
            message.contains("85%") && message.contains("70%"),
            "{message}"
        );
        assert!(message.contains("media.volume_boost"), "{message}");
        assert!(
            controller.applied().is_empty(),
            "a denied volume request must have no effect at all"
        );
    }

    #[test]
    fn an_edited_above_cap_argument_is_refused_at_binding_time() {
        // CF-9: the orchestrator validates the human's (possibly edited)
        // arguments before a grant binds. An edit that pushes the R1 tool above
        // the cap must be rejected there, not discovered at execution.
        let tool = playback(FakeController::with(playing_snapshot()));
        let err = tool
            .validate_args(&CanonicalValue::obj(vec![
                ("command", CanonicalValue::str("set_volume")),
                ("volume_pct", CanonicalValue::Int(90)),
            ]))
            .unwrap_err();
        assert!(
            matches!(err, ToolError::SchemaInvalid(ref m) if m.contains("90%") && m.contains("above the 70% volume cap")),
            "got {err:?}"
        );

        tool.validate_args(&CanonicalValue::obj(vec![
            ("command", CanonicalValue::str("set_volume")),
            ("volume_pct", CanonicalValue::Int(70)),
        ]))
        .expect("at the cap is valid");
        tool.validate_args(&CanonicalValue::obj(vec![(
            "command",
            CanonicalValue::str("pause"),
        )]))
        .expect("a known verb is valid");
        assert!(
            tool.validate_args(&CanonicalValue::obj(vec![(
                "command",
                CanonicalValue::str("detonate")
            )]))
            .is_err()
        );
    }

    #[tokio::test]
    async fn the_r2_tool_applies_an_above_cap_level_and_registers_the_undo() {
        let controller = FakeController::with(playing_snapshot()); // at 40%
        let result = boost(controller.clone())
            .execute(
                invocation(
                    MediaVolumeBoostTool::id(),
                    vec![
                        ("command", CanonicalValue::str("set_volume")),
                        ("volume_pct", CanonicalValue::Int(85)),
                        (
                            "player",
                            CanonicalValue::str("org.mpris.MediaPlayer2.spotify"),
                        ),
                    ],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.content, "Set Spotify volume to 85%.");
        assert_eq!(
            result.compensation.as_deref(),
            Some("Set Spotify volume back to 40%."),
            "the undo must restore the real previous level"
        );
        assert_eq!(
            controller.applied(),
            vec![Applied::Volume("org.mpris.MediaPlayer2.spotify".into(), 85)]
        );
    }

    #[tokio::test]
    async fn the_r2_tool_refuses_a_within_cap_level() {
        // Do not solicit an approval for something that needs none — approval
        // fatigue is a control weakness.
        let controller = FakeController::with(playing_snapshot());
        let err = boost(controller.clone())
            .execute(
                invocation(
                    MediaVolumeBoostTool::id(),
                    vec![
                        ("command", CanonicalValue::str("set_volume")),
                        ("volume_pct", CanonicalValue::Int(50)),
                        (
                            "player",
                            CanonicalValue::str("org.mpris.MediaPlayer2.spotify"),
                        ),
                    ],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(m) if m.contains("media.playback")));
        assert!(controller.applied().is_empty());
    }

    #[tokio::test]
    async fn out_of_range_volumes_are_rejected_never_clamped() {
        let controller = FakeController::with(playing_snapshot());
        for level in [101_i64, 500, -1] {
            let err = boost(controller.clone())
                .execute(
                    invocation(
                        MediaVolumeBoostTool::id(),
                        vec![
                            ("command", CanonicalValue::str("set_volume")),
                            ("volume_pct", CanonicalValue::Int(level)),
                            (
                                "player",
                                CanonicalValue::str("org.mpris.MediaPlayer2.spotify"),
                            ),
                        ],
                    ),
                    None,
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(err, ToolError::ExecutionFailed(ref m) if m.contains("between 0 and 100")),
                "level {level} gave {err:?}"
            );
        }
        assert!(controller.applied().is_empty());
    }

    #[test]
    fn seek_registers_the_inverse_and_next_registers_no_false_undo() {
        assert_eq!(
            compensation_for(TransportCommand::Seek { offset_secs: -30 }, "Spotify").as_deref(),
            Some("Seek Spotify by 30s.")
        );
        assert_eq!(compensation_for(TransportCommand::Next, "Spotify"), None);
        assert_eq!(compensation_for(TransportCommand::Stop, "Spotify"), None);
    }

    // ---- cast-a-link (media.open_url) ------------------------------------

    #[derive(Default)]
    struct FakeWindowSink {
        opened: Mutex<Vec<(String, String)>>,
        connected: bool,
    }

    #[async_trait]
    impl jarvis_application::ports::MediaWindowSink for FakeWindowSink {
        async fn open_url(
            &self,
            url: &str,
            monitor: &jarvis_domain::display::MonitorId,
            _target: Option<&str>,
        ) -> bool {
            self.opened
                .lock()
                .unwrap()
                .push((url.to_owned(), monitor.as_str().to_owned()));
            self.connected
        }
    }

    // FakeAuditLog: F9.4, jarvis-test-support — verified identical against
    // this file's original before moving.

    fn media_profile() -> Arc<jarvis_domain::display::DisplayProfile> {
        Arc::new(jarvis_domain::display::DisplayProfile::new([(
            jarvis_domain::display::Surface::MediaWindow,
            jarvis_domain::display::MonitorId::new("HDMI-A-1").unwrap(),
        )]))
    }

    fn open_url_tool(
        profile: Arc<jarvis_domain::display::DisplayProfile>,
        sink: Arc<FakeWindowSink>,
    ) -> MediaOpenUrlTool {
        MediaOpenUrlTool::new(profile, sink, Arc::new(FakeAuditLog::default()))
    }

    fn open_url_tool_with_audit(
        sink: Arc<FakeWindowSink>,
        audit: Arc<FakeAuditLog>,
    ) -> MediaOpenUrlTool {
        MediaOpenUrlTool::new(media_profile(), sink, audit)
    }

    #[tokio::test]
    async fn casts_an_https_url_to_the_profile_monitor_and_echoes_it_verbatim() {
        let sink = Arc::new(FakeWindowSink {
            connected: true,
            ..FakeWindowSink::default()
        });
        let result = open_url_tool(media_profile(), sink.clone())
            .execute(
                invocation(
                    MediaOpenUrlTool::id(),
                    vec![(
                        "url",
                        CanonicalValue::str("https://www.youtube.com/watch?v=abc"),
                    )],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(
            *sink.opened.lock().unwrap(),
            vec![(
                "https://www.youtube.com/watch?v=abc".to_owned(),
                "HDMI-A-1".to_owned()
            )]
        );
        // The URL appears verbatim in the result (and so in the audit record) —
        // never a paraphrase of where it points (docs/02 §11a).
        assert!(
            result
                .content
                .contains("https://www.youtube.com/watch?v=abc"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn casting_refuses_every_non_https_scheme() {
        let sink = Arc::new(FakeWindowSink::default());
        for hostile in [
            "file:///etc/passwd",
            "http://example.com/v",
            "javascript:alert(1)",
            "data:text/html,x",
            "https://",
            "https://ok.example\nsecond",
            // Multi-byte at the scheme boundary: reject, never panic.
            "https:/\u{20ac}evil.example/v",
        ] {
            let err = open_url_tool(media_profile(), sink.clone())
                .execute(
                    invocation(
                        MediaOpenUrlTool::id(),
                        vec![("url", CanonicalValue::str(hostile))],
                    ),
                    None,
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(err, ToolError::ExecutionFailed(_)),
                "must refuse {hostile:?}, got {err:?}"
            );
        }
        assert!(
            sink.opened.lock().unwrap().is_empty(),
            "nothing may reach the agent"
        );
    }

    #[tokio::test]
    async fn casting_fails_closed_without_a_configured_monitor() {
        // Never cast onto an arbitrary screen — the same rule as artifact
        // placement (F3a.4).
        let sink = Arc::new(FakeWindowSink::default());
        let empty = Arc::new(jarvis_domain::display::DisplayProfile::default());
        let err = open_url_tool(empty, sink.clone())
            .execute(
                invocation(
                    MediaOpenUrlTool::id(),
                    vec![("url", CanonicalValue::str("https://example.com/v"))],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(m) if m.contains("no monitor")));
        assert!(sink.opened.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_explicit_display_overrides_the_profile() {
        let sink = Arc::new(FakeWindowSink {
            connected: true,
            ..FakeWindowSink::default()
        });
        open_url_tool(media_profile(), sink.clone())
            .execute(
                invocation(
                    MediaOpenUrlTool::id(),
                    vec![
                        ("url", CanonicalValue::str("https://example.com/v")),
                        ("display", CanonicalValue::str("DP-2")),
                    ],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(sink.opened.lock().unwrap()[0].1, "DP-2");
    }

    #[tokio::test]
    async fn a_disconnected_agent_is_reported_not_silently_swallowed() {
        let sink = Arc::new(FakeWindowSink::default()); // connected: false
        let result = open_url_tool(media_profile(), sink)
            .execute(
                invocation(
                    MediaOpenUrlTool::id(),
                    vec![("url", CanonicalValue::str("https://example.com/v"))],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            result.content.contains("no desktop agent is connected"),
            "{}",
            result.content
        );
    }

    #[test]
    fn cast_a_link_is_r1_but_declares_external_egress() {
        let policy = MediaOpenUrlTool::policy();
        assert_eq!(policy.risk, RiskLevel::R1);
        assert!(policy.is_reversible, "closing the window undoes it");
        assert!(!policy.requires_grant());
        // The window fetches the URL — bytes leave the machine, and the policy
        // says so honestly even though the control itself is local.
        assert_eq!(policy.egress, DataEgress::External);
    }

    #[test]
    fn a_malformed_cast_url_is_refused_at_binding_time_too() {
        let tool = open_url_tool(media_profile(), Arc::new(FakeWindowSink::default()));
        assert!(
            tool.validate_args(&CanonicalValue::obj(vec![(
                "url",
                CanonicalValue::str("http://example.com/v")
            )]))
            .is_err()
        );
        tool.validate_args(&CanonicalValue::obj(vec![(
            "url",
            CanonicalValue::str("https://example.com/v"),
        )]))
        .expect("an https URL binds");
    }

    #[tokio::test]
    async fn a_cast_is_audited_verbatim_before_the_window_opens() {
        // docs/02 §11a: the URL appears verbatim in the audit event — and the
        // record is written BEFORE the effect (invariant 6).
        let sink = Arc::new(FakeWindowSink {
            connected: true,
            ..FakeWindowSink::default()
        });
        let audit = Arc::new(FakeAuditLog::default());
        open_url_tool_with_audit(sink.clone(), audit.clone())
            .execute(
                invocation(
                    MediaOpenUrlTool::id(),
                    vec![(
                        "url",
                        CanonicalValue::str("https://www.youtube.com/watch?v=abc"),
                    )],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let events = audit.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "media.cast");
        assert_eq!(events[0].target, "https://www.youtube.com/watch?v=abc");
        assert!(
            events[0]
                .payload_json
                .contains("https://www.youtube.com/watch?v=abc"),
            "the payload carries the URL verbatim: {}",
            events[0].payload_json
        );
    }

    #[tokio::test]
    async fn a_cast_that_cannot_be_audited_does_not_open_a_window() {
        let sink = Arc::new(FakeWindowSink::default());
        let audit = Arc::new(FakeAuditLog {
            fail: true,
            ..FakeAuditLog::default()
        });
        let err = open_url_tool_with_audit(sink.clone(), audit)
            .execute(
                invocation(
                    MediaOpenUrlTool::id(),
                    vec![("url", CanonicalValue::str("https://example.com/v"))],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::ExecutionFailed(m) if m.contains("could not be recorded"))
        );
        assert!(
            sink.opened.lock().unwrap().is_empty(),
            "no audit, no launch"
        );
    }

    #[tokio::test]
    async fn the_r2_boost_requires_an_explicit_player_so_the_grant_binds_it() {
        // Without this, the human approves "95%" and the effect can land on
        // whatever happens to be playing when the grant is consumed.
        let controller = FakeController::with(playing_snapshot());
        let err = boost(controller.clone())
            .execute(
                invocation(
                    MediaVolumeBoostTool::id(),
                    vec![
                        ("command", CanonicalValue::str("set_volume")),
                        ("volume_pct", CanonicalValue::Int(85)),
                    ],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(m) if m.contains("explicit `player`")));
        assert!(controller.applied().is_empty());

        // And the same refusal at approval-binding time.
        assert!(
            boost(controller)
                .validate_args(&CanonicalValue::obj(vec![
                    ("command", CanonicalValue::str("set_volume")),
                    ("volume_pct", CanonicalValue::Int(85)),
                ]))
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_r2_boost_refuses_a_verb_that_would_misdescribe_the_effect() {
        // A proposal saying `pause` would render "pause" on the approval card
        // while setting a volume — the model must not control text that
        // misdescribes what executes (docs/06 §3).
        let controller = FakeController::with(playing_snapshot());
        let err = boost(controller.clone())
            .execute(
                invocation(
                    MediaVolumeBoostTool::id(),
                    vec![
                        ("command", CanonicalValue::str("pause")),
                        ("volume_pct", CanonicalValue::Int(95)),
                        (
                            "player",
                            CanonicalValue::str("org.mpris.MediaPlayer2.spotify"),
                        ),
                    ],
                ),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(m) if m.contains("only sets volume")));
        assert!(controller.applied().is_empty());
    }
}
