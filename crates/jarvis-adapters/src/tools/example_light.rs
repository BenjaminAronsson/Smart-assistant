//! `example.light` — a **reversible R1 example tool** (F2.6, exit evidence #2,
//! golden 4). A deliberately minimal stand-in for the M5 Home Assistant
//! `home.set_light`: it toggles an in-memory light state and returns a
//! **compensating undo** with its result, which the orchestrator surfaces in the
//! run timeline (`CompensationRegistered`). This exercises the reversible-action
//! path — auto-authorized as R1, undo registered — without pulling the real HA
//! adapter forward (that is M5). It performs no real-world effect.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion};
use tokio_util::sync::CancellationToken;

use crate::tools::required_str;

/// A reversible example tool holding an in-memory light state so the undo it
/// registers is real (the compensation restores the *previous* value).
pub struct ExampleLightTool {
    state: Mutex<String>,
}

impl Default for ExampleLightTool {
    fn default() -> Self {
        Self {
            state: Mutex::new("off".to_owned()),
        }
    }
}

impl ExampleLightTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn id() -> ToolId {
        "example.light".parse().expect("static tool id is valid")
    }

    /// Host-owned policy: R1 (auto-authorized), **reversible**, local-only, gated
    /// behind the `demo:light` scope. Being reversible is what lets the R1 auto
    /// path proceed while still registering an undo.
    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R1,
            is_reversible: true,
            requires_user_presence: false,
            timeout: Duration::from_secs(5),
            required_scopes: [Scope::new("demo:light").expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::Local,
        }
    }

    pub fn descriptor() -> ToolDescriptor {
        ToolDescriptor {
            id: Self::id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: std::sync::Arc::new(Self::new()),
        }
    }
}

#[async_trait]
impl ToolExecutor for ExampleLightTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R1: auto-authorized, never carries a grant.
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let requested = required_str(&invocation.arguments, "state")?;
        let next = match requested {
            "on" | "off" => requested.to_owned(),
            other => {
                return Err(ToolError::ExecutionFailed(format!(
                    "state must be `on` or `off`, got `{other}`"
                )));
            }
        };

        // Swap under the lock (no await held); capture the previous value so the
        // compensation restores exactly what was there before.
        let previous = {
            let mut state = self.state.lock().expect("light state mutex poisoned");
            std::mem::replace(&mut *state, next.clone())
        };

        Ok(ToolResult {
            content: format!("Light is now {next}."),
            truncated: false,
            compensation: Some(format!("Set the light back to {previous}.")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::tools::CanonicalValue;

    fn invocation(state: &str) -> ToolInvocation {
        ToolInvocation {
            tool_id: ExampleLightTool::id(),
            tool_version: ToolVersion::new(1, 0, 0),
            arguments: CanonicalValue::obj([("state", CanonicalValue::str(state))]),
        }
    }

    #[test]
    fn policy_is_reversible_r1_local() {
        let policy = ExampleLightTool::policy();
        assert_eq!(policy.risk, RiskLevel::R1);
        assert!(policy.is_reversible);
        assert!(!policy.requires_grant());
        assert_eq!(policy.egress, DataEgress::Local);
    }

    #[tokio::test]
    async fn registers_a_compensating_undo_reflecting_the_previous_state() {
        let tool = ExampleLightTool::new(); // starts "off"
        let result = tool
            .execute(invocation("on"), None, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result.content, "Light is now on.");
        assert_eq!(
            result.compensation.as_deref(),
            Some("Set the light back to off.")
        );

        // The next call's undo must reflect the now-current state, proving the
        // reversal is real state, not a canned string.
        let result = tool
            .execute(invocation("off"), None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            result.compensation.as_deref(),
            Some("Set the light back to on.")
        );
    }

    #[tokio::test]
    async fn rejects_an_invalid_state() {
        let tool = ExampleLightTool::new();
        let err = tool
            .execute(invocation("dim"), None, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(_)), "got {err:?}");
    }
}
