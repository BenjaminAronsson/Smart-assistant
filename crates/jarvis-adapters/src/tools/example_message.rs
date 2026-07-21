//! `message.send` — a **fake R2 external tool** (F2.6, exit evidence #3, golden
//! 5). A demonstration stand-in for the real SMTP send adapter (M4/ADR-026): it
//! classifies as R2 with **external egress**, so a proposal parks at
//! `WaitingApproval` and executes only against a validated single-use
//! `ExecutionGrant`. This drives the full approval → grant → execute →
//! edit-invalidation flow while sending nothing — it performs no real external
//! effect, and is clearly a tier demonstration, not a shipping integration.

use std::time::Duration;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{
    CanonicalValue, ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion,
};
use tokio_util::sync::CancellationToken;

use crate::tools::{require_str_arg, required_str};

/// The `message.send` demonstration executor. Stateless: it validates the shape
/// of its arguments and returns a confirmation without contacting anything.
#[derive(Default)]
pub struct ExampleMessageTool;

impl ExampleMessageTool {
    pub fn new() -> Self {
        Self
    }

    pub fn id() -> ToolId {
        "message.send".parse().expect("static tool id is valid")
    }

    /// Host-owned policy: R2 (requires human approval + a grant), **not
    /// reversible**, **external** egress, gated behind the `message:send` scope.
    /// R2 means a proposal never auto-authorizes — it parks for a human.
    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R2,
            is_reversible: false,
            requires_user_presence: true,
            timeout: Duration::from_secs(10),
            required_scopes: [Scope::new("message:send").expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::External,
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
impl ToolExecutor for ExampleMessageTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R2: validated + consumed by the orchestrator before we run.
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        // Shape validation only — authorization already happened in the policy
        // engine + grant validator (invariant #1); the executor never re-decides.
        let recipient = required_str(&invocation.arguments, "to")?;
        let _body = required_str(&invocation.arguments, "body")?;

        Ok(ToolResult {
            content: format!("Message queued to {recipient} (demo stand-in — nothing was sent)."),
            truncated: false,
            compensation: None,
        })
    }

    /// CF-9: this R2 tool parks for human approval, and the human may *edit* the
    /// recipient/body before approving. The orchestrator runs this on the final
    /// approved arguments BEFORE minting the grant, so an edit that drops or
    /// malforms `to`/`body` is rejected at binding time — no grant is minted for
    /// an effect the tool cannot honour — rather than surfacing only at execution.
    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        require_str_arg(arguments, "to")?;
        require_str_arg(arguments, "body")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::tools::CanonicalValue;

    fn invocation(to: &str, body: &str) -> ToolInvocation {
        ToolInvocation {
            tool_id: ExampleMessageTool::id(),
            tool_version: ToolVersion::new(1, 0, 0),
            arguments: CanonicalValue::obj([
                ("to", CanonicalValue::str(to)),
                ("body", CanonicalValue::str(body)),
            ]),
        }
    }

    #[test]
    fn policy_is_r2_external_requires_grant() {
        let policy = ExampleMessageTool::policy();
        assert_eq!(policy.risk, RiskLevel::R2);
        assert!(!policy.is_reversible);
        assert!(policy.requires_grant());
        assert_eq!(policy.egress, DataEgress::External);
        assert!(policy.requires_user_presence);
    }

    #[tokio::test]
    async fn confirms_without_sending() {
        let tool = ExampleMessageTool::new();
        let result = tool
            .execute(
                invocation("carol@example.com", "hi"),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(result.content.contains("carol@example.com"));
        assert!(result.content.contains("nothing was sent"));
    }

    #[test]
    fn validate_args_accepts_a_well_formed_edit() {
        let tool = ExampleMessageTool::new();
        let args = CanonicalValue::obj([
            ("to", CanonicalValue::str("carol@example.com")),
            ("body", CanonicalValue::str("hi")),
        ]);
        assert!(tool.validate_args(&args).is_ok());
    }

    #[test]
    fn validate_args_rejects_an_edit_missing_the_body() {
        let tool = ExampleMessageTool::new();
        // A human edit that drops `body`: rejected at approval time (CF-9),
        // before a grant can bind — never surfaced only at execution.
        let args = CanonicalValue::obj([("to", CanonicalValue::str("carol@example.com"))]);
        let err = tool.validate_args(&args).unwrap_err();
        assert!(matches!(err, ToolError::SchemaInvalid(_)), "got {err:?}");
    }

    #[test]
    fn validate_args_rejects_a_non_object() {
        let tool = ExampleMessageTool::new();
        let err = tool
            .validate_args(&CanonicalValue::str("not an object"))
            .unwrap_err();
        assert!(matches!(err, ToolError::SchemaInvalid(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn requires_both_recipient_and_body() {
        let tool = ExampleMessageTool::new();
        let invocation = ToolInvocation {
            tool_id: ExampleMessageTool::id(),
            tool_version: ToolVersion::new(1, 0, 0),
            arguments: CanonicalValue::obj([("to", CanonicalValue::str("carol@example.com"))]),
        };
        let err = tool
            .execute(invocation, None, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(_)), "got {err:?}");
    }
}
