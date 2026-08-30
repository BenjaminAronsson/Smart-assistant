use std::sync::Arc;

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

    pub fn id() -> ToolId {
        "home.get_state".parse().expect("static tool id is valid")
    }

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
