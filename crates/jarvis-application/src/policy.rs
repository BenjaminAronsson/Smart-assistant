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
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use jarvis_domain::audit::AuditEvent;
use jarvis_domain::grants::{ExecutionGrant, GrantError};
use jarvis_domain::ids::{DeviceId, RunId, UserId};
use jarvis_domain::policy::{DataEgress, ResourcePattern, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{
    CanonicalValue, ToolError, ToolId, ToolInvocation, ToolProposal, ToolResult, ToolVersion,
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

    /// Validate a set of arguments *before* a grant binds them (CF-9, docs/06
    /// §4). The orchestrator calls this on the human's *approved* arguments —
    /// which may have been edited away from the proposal — so a malformed edit
    /// is caught at approval time and never mints a grant, rather than only
    /// failing later inside [`Self::execute`]. The default accepts everything;
    /// a tool with an argument schema overrides it to reject shape violations.
    /// Returning `Ok` is not authority to execute — the policy/grant gates still
    /// apply (invariant #1); this only rejects arguments the tool cannot honour.
    fn validate_args(&self, _arguments: &CanonicalValue) -> Result<(), ToolError> {
        Ok(())
    }
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

// ---- Approvals & execution grants (F2.3, docs/06 §4) ---------------------

/// What a human is asked to approve (docs/06 §3). Carries the *exact effect* —
/// the real tool and its concrete arguments — never a model paraphrase — plus
/// the host policy attributes the human weighs the decision against (F2.5): the
/// risk tier, whether the effect is reversible, and how far its data travels.
/// These come from the tool's [`ToolPolicy`], never from model or tool text.
#[derive(Clone)]
pub struct ApprovalRequest {
    pub run_id: RunId,
    pub tool_id: ToolId,
    pub exact_effect: String,
    pub proposed_arguments: CanonicalValue,
    pub risk: RiskLevel,
    pub reversible: bool,
    pub egress: DataEgress,
}

// CF-12: `exact_effect` (real target + payload the human sees) and the proposed
// arguments are sensitive — redact both from `Debug`, which flows through spans/
// logs (invariant #5). The risk/reversible/egress attributes stay for
// correlation. Same treatment as `GrantBinding` (CF-7).
impl std::fmt::Debug for ApprovalRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalRequest")
            .field("run_id", &self.run_id)
            .field("tool_id", &self.tool_id)
            .field("exact_effect", &"<redacted>")
            .field("proposed_arguments", &"<redacted>")
            .field("risk", &self.risk)
            .field("reversible", &self.reversible)
            .field("egress", &self.egress)
            .finish()
    }
}

/// A human's decision. `Approved` carries the *final* arguments the human
/// authorized, which may differ from the proposal if they edited the effect —
/// the grant binds these, so executing anything else fails validation
/// (docs/06 §4). Editing is therefore invalidation-by-rebinding, not a flag.
#[derive(Clone, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved { arguments: CanonicalValue },
    Denied,
}

// CF-12: redact the approved arguments from `Debug` (invariant #5).
impl std::fmt::Debug for ApprovalOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Approved { .. } => f
                .debug_struct("Approved")
                .field("arguments", &"<redacted>")
                .finish(),
            Self::Denied => f.write_str("Denied"),
        }
    }
}

/// The seam to the human (F2.5 implements it over the WS/REST approval tray;
/// tests script it). Blocks until a decision or cancellation (invariant #4).
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn request(&self, request: ApprovalRequest, cancel: CancellationToken)
    -> ApprovalOutcome;
}

/// Inputs to mint a grant (docs/06 §4). The minter (infra, F2.4) supplies the
/// cryptographically random id, the SHA-256 of `canonical_form(arguments)`, and
/// `expires_at = now + ttl`; the application never computes crypto itself
/// (domain/application dep rule).
///
/// `Debug` is **manual** and redacts `arguments` (CF-7, invariant #5): the raw
/// approved arguments may carry a secret (a recipient, a file path, a message
/// body), so a `tracing` field or accidental `{:?}` on a binding must never
/// spill them into a log. The field renders as `<redacted>`; the argument
/// *hash* is what the audit trail records instead (see the grant lifecycle).
#[derive(Clone)]
pub struct GrantBinding {
    pub user_id: UserId,
    pub device_id: DeviceId,
    pub run_id: RunId,
    pub tool_id: ToolId,
    pub tool_version: ToolVersion,
    pub arguments: CanonicalValue,
    pub target_resource: ResourcePattern,
    pub ttl: Duration,
}

impl std::fmt::Debug for GrantBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrantBinding")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("run_id", &self.run_id)
            .field("tool_id", &self.tool_id)
            .field("tool_version", &self.tool_version)
            // Never render the raw arguments — they may be secret (invariant #5).
            .field("arguments", &"<redacted>")
            .field("target_resource", &self.target_resource)
            .field("ttl", &self.ttl)
            .finish()
    }
}

/// An infra fault that prevented a grant from being minted (docs/06 §4, CF-6).
/// The grant ports were infallible through F2.3, so a DB fault in the store
/// `panic`ked the task — FAIL-SAFE (a panicked mint authorizes nothing) but not
/// graceful. This error arm lets the orchestrator route such a fault to
/// [`RunState::Failed`](jarvis_domain::run::RunState) instead of aborting. The
/// message is for the host span/log only, never the user outcome (invariant #5).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("grant mint failed: {0}")]
pub struct GrantMintError(pub String);

/// Mints a single-use grant on approval (docs/06 §4). Implemented in infra
/// (F2.4) with real randomness + SHA-256; a test fake mints deterministically.
/// Returns [`GrantMintError`] on an infra fault so the orchestrator fails the
/// run gracefully rather than panicking (CF-6). A mint failure authorizes
/// nothing — no grant, no execution (invariant #1).
#[async_trait]
pub trait GrantMinter: Send + Sync {
    async fn mint(&self, binding: GrantBinding) -> Result<ExecutionGrant, GrantMintError>;
}

/// Validates + consumes a grant immediately before execution (docs/06 §4).
/// Recomputes the argument hash, checks the full binding and expiry against
/// `now`, and consumes `single_use` so a replay fails. Any failure ⇒ the
/// executor is never called (invariant #1).
#[async_trait]
pub trait GrantValidator: Send + Sync {
    async fn validate(
        &self,
        grant: &ExecutionGrant,
        invocation: &ToolInvocation,
        now: SystemTime,
    ) -> Result<(), GrantError>;
}

#[cfg(test)]
mod cf12_debug_redaction {
    use super::*;

    // A valid ULID for constructing a RunId in tests.
    const RUN_ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    #[test]
    fn approval_request_debug_redacts_effect_and_arguments() {
        let request = ApprovalRequest {
            run_id: RUN_ULID.parse().unwrap(),
            tool_id: "message.send".parse().unwrap(),
            exact_effect: "Email carol@example.com: transfer authorized".to_owned(),
            proposed_arguments: CanonicalValue::obj([(
                "body",
                CanonicalValue::str("secret-body-text"),
            )]),
            risk: RiskLevel::R2,
            reversible: false,
            egress: DataEgress::External,
        };
        let rendered = format!("{request:?}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(
            !rendered.contains("carol@example.com"),
            "leaked effect: {rendered}"
        );
        assert!(
            !rendered.contains("secret-body-text"),
            "leaked args: {rendered}"
        );
        assert!(rendered.contains("R2"), "risk kept for correlation");
    }

    #[test]
    fn approval_outcome_debug_redacts_approved_arguments() {
        let approved = ApprovalOutcome::Approved {
            arguments: CanonicalValue::obj([("body", CanonicalValue::str("secret-body-text"))]),
        };
        let rendered = format!("{approved:?}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(!rendered.contains("secret-body-text"), "leaked: {rendered}");
        assert_eq!(format!("{:?}", ApprovalOutcome::Denied), "Denied");
    }
}
