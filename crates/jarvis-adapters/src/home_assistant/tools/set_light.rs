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

// ---------------------------------------------------------------------------
// home.set_light (R1 + allowlist)
// ---------------------------------------------------------------------------

/// The desired light state. Deliberately binary.
///
/// Brightness, colour and transition are **out of scope for F5.3** — every
/// extra parameter is another argument the policy tier cannot see, and the
/// milestone's exit evidence is "safely control one allowlisted entity".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LightState {
    On,
    Off,
}

impl LightState {
    pub(crate) fn parse(value: &str) -> Result<Self, ToolError> {
        match value {
            "on" => Ok(Self::On),
            "off" => Ok(Self::Off),
            _ => Err(ToolError::SchemaInvalid(
                "home argument `state` must be `on` or `off`".to_owned(),
            )),
        }
    }

    pub(crate) fn service(self) -> CuratedService {
        match self {
            Self::On => CuratedService::LightTurnOn,
            Self::Off => CuratedService::LightTurnOff,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
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

    declare_tool_id!("home.set_light");

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
pub(crate) async fn apply_one(
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
