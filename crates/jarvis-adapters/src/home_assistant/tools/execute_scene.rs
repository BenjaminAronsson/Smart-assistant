use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{
    CanonicalValue, ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion,
};
use tokio_util::sync::CancellationToken;

use super::super::*;
use super::*;

// ---------------------------------------------------------------------------
// home.execute_scene / home.run_script (R2 + allowlist + grant)
// ---------------------------------------------------------------------------

/// Which broad-effect tool a [`HomeBroadTool`] instance is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BroadKind {
    Scene,
    Script,
}

impl BroadKind {
    fn tool_id(self) -> ToolId {
        match self {
            Self::Scene => "home.execute_scene",
            Self::Script => "home.run_script",
        }
        .parse()
        .expect("static tool id is valid")
    }

    fn service(self) -> CuratedService {
        match self {
            Self::Scene => CuratedService::SceneTurnOn,
            Self::Script => CuratedService::ScriptTurnOn,
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Scene => "Activated",
            Self::Script => "Ran",
        }
    }
}

/// `home.execute_scene` and `home.run_script` — the two broad-blast-radius home
/// tools. One implementation, two registered tools: they differ only in the
/// curated service they call and the allowlist they consult, and keeping them as
/// separate `ToolId`s means an approval for a scene can never execute a script.
pub struct HomeBroadTool {
    kind: BroadKind,
    client: Arc<HomeAssistantClient>,
    allowlist: Arc<EntityAllowlist>,
}

impl HomeBroadTool {
    fn new(
        kind: BroadKind,
        client: Arc<HomeAssistantClient>,
        allowlist: Arc<EntityAllowlist>,
    ) -> Self {
        Self {
            kind,
            client,
            allowlist,
        }
    }

    pub fn scene(client: Arc<HomeAssistantClient>, allowlist: Arc<EntityAllowlist>) -> Self {
        Self::new(BroadKind::Scene, client, allowlist)
    }

    pub fn script(client: Arc<HomeAssistantClient>, allowlist: Arc<EntityAllowlist>) -> Self {
        Self::new(BroadKind::Script, client, allowlist)
    }

    pub fn scene_id() -> ToolId {
        BroadKind::Scene.tool_id()
    }

    pub fn script_id() -> ToolId {
        BroadKind::Script.tool_id()
    }

    /// Host-owned policy: **R2** — a scene/script is a set of effects behind one
    /// name (docs/06 §3 "meaningful mutation / change automation"). Not
    /// reversible: there is no single undo for "whatever that script did", so
    /// claiming reversibility would be a lie the approval card repeats. User
    /// presence is required — a broad physical change should not fire while
    /// nobody is at a device to see it. Egress is `Local`: the payload reaches
    /// HA only.
    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R2,
            is_reversible: false,
            requires_user_presence: true,
            timeout: REQUEST_TIMEOUT,
            required_scopes: [Scope::new(CONTROL_SCOPE).expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::Local,
        }
    }

    pub fn scene_descriptor(
        client: Arc<HomeAssistantClient>,
        allowlist: Arc<EntityAllowlist>,
    ) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::scene_id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: Arc::new(Self::scene(client, allowlist)),
        }
    }

    pub fn script_descriptor(
        client: Arc<HomeAssistantClient>,
        allowlist: Arc<EntityAllowlist>,
    ) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::script_id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: Arc::new(Self::script(client, allowlist)),
        }
    }

    /// Arguments are `{entity_id, friendly_name}`. The friendly name is present
    /// because `policy::exact_effect` renders the *arguments* onto the approval
    /// card: carrying it is what makes docs/02 §10's "approvals show friendly
    /// name + entity ID" true of the text a human actually reads. It is checked
    /// against HA before execution (see [`verify_label`]), so it is a claim the
    /// system verifies, never a label the model gets to choose.
    fn target(&self, arguments: &CanonicalValue) -> Result<(EntityId, String), ToolError> {
        let values = exact_string_args(arguments, &["entity_id", "friendly_name"])?;
        let [entity_id, friendly_name] = values[..] else {
            return Err(ToolError::SchemaInvalid(
                "home arguments must be exactly {entity_id, friendly_name}".to_owned(),
            ));
        };
        let entity = parse_entity(entity_id)?;
        let allowed = match self.kind {
            BroadKind::Scene => self.allowlist.allows_scene(&entity),
            BroadKind::Script => self.allowlist.allows_script(&entity),
        };
        if !allowed {
            return Err(not_allowlisted(&entity));
        }
        if friendly_name.is_empty() || friendly_name.len() > MAX_FRIENDLY_NAME_CHARS * 4 {
            return Err(ToolError::SchemaInvalid(
                "home argument `friendly_name` is out of range".to_owned(),
            ));
        }
        Ok((entity, friendly_name.to_owned()))
    }
}

#[async_trait]
impl ToolExecutor for HomeBroadTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        grant: Option<ExecutionGrant>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        // Order matters and is security-first: shape, then allowlist, then
        // grant — all before the transport is touched at all.
        let (entity, claimed_name) = self.target(&invocation.arguments)?;
        check_grant(grant.as_ref(), &invocation, SystemTime::now())?;

        let metadata = verify_label(&self.client, &entity, &claimed_name, &cancel).await?;
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        self.client
            .call_service(self.kind.service(), &entity, &cancel)
            .await?;
        Ok(ToolResult {
            content: format!("{} {}.", self.kind.verb(), metadata.label()),
            truncated: false,
            // Honest: R2 here is not reversible, so no undo is registered.
            compensation: None,
        })
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        self.target(arguments).map(|_| ())
    }
}
