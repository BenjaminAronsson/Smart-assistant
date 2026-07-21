//! The policy engine (docs/02 §5 step 6, docs/06 §3, invariant #1). Models
//! propose; this module decides. Every tool proposal — R0 included — is routed
//! through [`evaluate`], which is the *only* thing that authorizes execution.
//! There is no read-only shortcut and no path from model/tool text to an
//! execution decision that skips this function.
//!
//! Policy metadata is host-owned: a tool is registered with a [`ToolPolicy`],
//! and a descriptor that carries none is refused ([`RegistrationError`]). An
//! MCP-imported descriptor (F2.7) gets host policy overlaid here — a server can
//! never declare its own safety (docs/06 §5).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use jarvis_domain::audit::AuditEvent;
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::ids::{DeviceId, UserId};
use jarvis_domain::policy::{Scope, ToolPolicy};
use jarvis_domain::tools::{
    ToolError, ToolId, ToolInvocation, ToolProposal, ToolResult, ToolVersion,
};

/// Executes one bounded tool call (docs/05 §4). Implemented by native adapters
/// and the MCP host (F2.6/F2.7). `grant` is `None` only for auto-authorized
/// R0/R1 calls; R2+ calls always carry a validated [`ExecutionGrant`] (F2.3).
/// The [`CancellationToken`] must abort in-flight work promptly (invariant #4).
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        grant: Option<ExecutionGrant>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError>;
}

/// Append-only audit sink (invariant #6, docs/06 §7). F2.4 implements this to
/// write in the same transaction as the run's domain change; the port lets the
/// orchestrator record a policy decision without depending on infra.
#[async_trait]
pub trait AuditSink: Send + Sync {
    async fn record(&self, event: AuditEvent);
}

/// A tool as offered for registration. `policy` is `Option` so the registry can
/// *refuse* a descriptor that arrives without host policy (an untrusted MCP
/// descriptor, docs/06 §5) — the refusal is the test-observable enforcement of
/// "no tool without policy metadata".
pub struct ToolDescriptor {
    pub id: ToolId,
    pub version: ToolVersion,
    pub policy: Option<ToolPolicy>,
    pub executor: Arc<dyn ToolExecutor>,
}

/// Why a tool could not be registered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistrationError {
    #[error("tool {0} was offered without host policy metadata")]
    MissingPolicy(ToolId),
    #[error("tool {0} is already registered")]
    Duplicate(ToolId),
}

struct RegisteredTool {
    version: ToolVersion,
    policy: ToolPolicy,
    executor: Arc<dyn ToolExecutor>,
}

/// The host-owned catalogue of executable tools. Construction is the only way a
/// tool becomes callable; there is no ambient tool set.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<ToolId, RegisteredTool>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. A descriptor with no policy is refused (docs/06 §5):
    /// safety metadata is host-owned, never server-declared.
    pub fn register(&mut self, descriptor: ToolDescriptor) -> Result<(), RegistrationError> {
        let policy = descriptor
            .policy
            .ok_or_else(|| RegistrationError::MissingPolicy(descriptor.id.clone()))?;
        if self.tools.contains_key(&descriptor.id) {
            return Err(RegistrationError::Duplicate(descriptor.id));
        }
        self.tools.insert(
            descriptor.id,
            RegisteredTool {
                version: descriptor.version,
                policy,
                executor: descriptor.executor,
            },
        );
        Ok(())
    }

    fn get(&self, id: &ToolId) -> Option<&RegisteredTool> {
        self.tools.get(id)
    }

    /// The policy a proposal would be evaluated against, if the tool exists.
    pub fn policy_of(&self, id: &ToolId) -> Option<&ToolPolicy> {
        self.get(id).map(|t| &t.policy)
    }

    /// The executor + version for an authorized invocation. Callers reach this
    /// only after [`evaluate`] returned `Auto` (or a grant validated, F2.3).
    pub fn resolve(&self, id: &ToolId) -> Option<(ToolVersion, Arc<dyn ToolExecutor>)> {
        self.get(id).map(|t| (t.version, t.executor.clone()))
    }
}

/// The per-run authorization context (docs/02 §5). Carries the actor and the
/// scopes the run's device holds; policy checks a tool's `required_scopes`
/// against these. Never derived from model or tool text (invariant #1).
#[derive(Debug, Clone)]
pub struct PolicyContext {
    pub user_id: UserId,
    pub device_id: DeviceId,
    pub granted_scopes: BTreeSet<Scope>,
}

/// The decision `evaluate` returns (docs/06 §3). `Auto` authorizes an R0/R1
/// call; `NeedsApproval` carries the exact effect a human will see (F2.5);
/// `Reject` blocks the call outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Auto,
    NeedsApproval { exact_effect: String },
    Reject { reason: DenyReason },
}

/// Why a proposal was rejected. Each maps to a stable machine code (docs/05 §7).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DenyReason {
    #[error("tool is not registered")]
    UnknownTool,
    #[error("action is prohibited (R4) and cannot be authorized")]
    Prohibited,
    #[error("run is missing required scope {0}")]
    MissingScope(Scope),
}

impl DenyReason {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownTool => "policy.unknown_tool",
            Self::Prohibited => "policy.prohibited",
            Self::MissingScope(_) => "policy.missing_scope",
        }
    }
}

/// Classify a proposal (docs/06 §3). The order is security-first: an unknown
/// tool and a prohibited (R4) tool are rejected before any scope or risk logic,
/// and a missing scope is rejected before an otherwise-allowed risk tier could
/// auto-authorize. Only an in-scope R0/R1 tool reaches `Auto`.
pub fn evaluate(
    proposal: &ToolProposal,
    registry: &ToolRegistry,
    ctx: &PolicyContext,
) -> PolicyDecision {
    let Some(policy) = registry.policy_of(&proposal.tool_id) else {
        return PolicyDecision::Reject {
            reason: DenyReason::UnknownTool,
        };
    };
    if policy.risk.is_prohibited() {
        return PolicyDecision::Reject {
            reason: DenyReason::Prohibited,
        };
    }
    if let Some(missing) = policy
        .required_scopes
        .iter()
        .find(|s| !ctx.granted_scopes.contains(*s))
    {
        return PolicyDecision::Reject {
            reason: DenyReason::MissingScope(missing.clone()),
        };
    }
    if policy.requires_grant() {
        return PolicyDecision::NeedsApproval {
            exact_effect: exact_effect(proposal),
        };
    }
    PolicyDecision::Auto
}

/// A human-readable rendering of exactly what will execute (docs/06 §3): the
/// tool and its concrete arguments, never a model paraphrase. F2.5 replaces this
/// with a tool-aware, structured approval card; F2.2 needs the string form so
/// the decision carries the real payload from the start.
pub fn exact_effect(proposal: &ToolProposal) -> String {
    format!("{} {}", proposal.tool_id, render_value(&proposal.arguments))
}

fn render_value(value: &jarvis_domain::tools::CanonicalValue) -> String {
    use jarvis_domain::tools::CanonicalValue as V;
    match value {
        V::Null => "null".to_owned(),
        V::Bool(b) => b.to_string(),
        V::Int(n) => n.to_string(),
        V::Float(t) => t.clone(),
        V::Str(s) => format!("{s:?}"),
        V::Array(items) => {
            let inner: Vec<String> = items.iter().map(render_value).collect();
            format!("[{}]", inner.join(", "))
        }
        V::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{k}={}", render_value(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}
