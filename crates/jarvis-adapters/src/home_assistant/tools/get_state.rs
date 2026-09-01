use std::sync::Arc;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::declare_tool_id;
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, SpeechSensitivity, ToolPolicy};
use jarvis_domain::tools::{CanonicalValue, ToolError, ToolInvocation, ToolResult, ToolVersion};
use tokio_util::sync::CancellationToken;

use super::super::*;
use super::*;

// ---------------------------------------------------------------------------
// home.get_state (R0)
// ---------------------------------------------------------------------------

/// `home.get_state` — read one allowlisted entity's **live** state.
pub struct HomeGetStateTool {
    client: Arc<HomeAssistantClient>,
    allowlist: Arc<EntityAllowlist>,
}

impl HomeGetStateTool {
    pub fn new(client: Arc<HomeAssistantClient>, allowlist: Arc<EntityAllowlist>) -> Self {
        Self { client, allowlist }
    }

    declare_tool_id!("home.get_state");

    /// Host-owned policy: **R0** — read-only, automatic within scope, audited
    /// (docs/06 §3). `Local` egress: the request reaches HA on the LAN and
    /// nothing leaves the home network.
    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R0,
            is_reversible: true,
            requires_user_presence: false,
            timeout: REQUEST_TIMEOUT,
            required_scopes: [Scope::new(READ_SCOPE).expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::Local,
            // S3/ADR-033 §4. Household state is lock state, occupancy and
            // presence: "the back door is unlocked and nobody's home" is not a
            // weather answer, and reading it out in a vendor voice sends the
            // one fact about this house that most warrants staying inside it.
            //
            // The second entry in the table where `Local` egress and
            // `Sensitive` speech disagree, and for the opposite reason to
            // `fs.read`'s: the *request* never leaves the LAN, which is exactly
            // why nothing about the answer suggests care is needed.
            speech_sensitivity: SpeechSensitivity::Sensitive,
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

    fn target(&self, arguments: &CanonicalValue) -> Result<EntityId, ToolError> {
        let [entity_id] = exact_string_args(arguments, &["entity_id"])?[..] else {
            return Err(ToolError::SchemaInvalid(
                "home arguments must be exactly {entity_id}".to_owned(),
            ));
        };
        let entity = parse_entity(entity_id)?;
        if !self.allowlist.is_readable(&entity) {
            return Err(not_allowlisted(&entity));
        }
        Ok(entity)
    }
}

#[async_trait]
impl ToolExecutor for HomeGetStateTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R0: auto-authorized, never carries a grant.
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        // Allowlist first: a denied read costs no request.
        let entity = self.target(&invocation.arguments)?;
        // Always live — HA is the system of record (docs/02 §10).
        let state = self.client.state(&entity, &cancel).await?;
        Ok(ToolResult {
            content: format!("{} is {}.", state.metadata.label(), state.state),
            truncated: false,
            compensation: None,
        })
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        self.target(arguments).map(|_| ())
    }
}
