//! `GET /api/v1/policy` — what each tool may do (F10.5, FR-05, docs/12).
//!
//! # The whole design in one sentence
//!
//! This module **asks `policy::evaluate`** for every (tool, device class) pair
//! and reports what it said; it does not re-derive the answer from the tool's
//! policy fields.
//!
//! That is the difference between this feature being useful and being harmful.
//! F10.5's own acceptance is that "the rendered policy matches
//! `policy::evaluate`'s actual decisions for the same inputs", because a UI
//! describing different rules than the engine enforces is worse than none: it
//! turns "I am not sure what this will do" into confident, wrong belief. A
//! projection that re-implemented the tier/scope/prohibition logic would be a
//! second copy of the rules, free to drift on the next policy change, and the
//! drift would be invisible precisely where it matters.
//!
//! By construction there is nothing here that *can* disagree. The remaining risk
//! is narrower and testable: that the projection feeds `evaluate` the wrong
//! inputs. `crates/jarvisd/tests/policy_api.rs` pins that.
//!
//! # Read-only
//!
//! No write surface, deliberately (see `jarvis_contracts::policy`). Changing
//! risk tiers from a web page is a much bigger authority question than changing
//! a wake word; it belongs in an ADR before it belongs in a route.

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::State;
use axum::response::Response;
use jarvis_application::policy::{PolicyContext, PolicyDecision, ToolRegistry, evaluate};
use jarvis_contracts::policy::{ClassOutcomeDto, PolicyOutcomeDto, PolicyViewDto, ToolPolicyDto};
use jarvis_domain::identity::DeviceClass;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, SpeechSensitivity};
use jarvis_domain::tools::{CanonicalValue, ToolProposal};

/// Project the live registry into the read-only view.
pub fn project(registry: &ToolRegistry) -> PolicyViewDto {
    let tools = registry
        .tool_ids()
        .filter_map(|id| {
            let policy = registry.policy_of(id)?;
            Some(ToolPolicyDto {
                tool_id: id.to_string(),
                risk: risk_name(policy.risk).to_owned(),
                reversible: policy.is_reversible,
                requires_user_presence: policy.requires_user_presence,
                egress: egress_name(policy.egress).to_owned(),
                speech_sensitivity: speech_sensitivity_name(policy.speech_sensitivity).to_owned(),
                required_scopes: policy
                    .required_scopes
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                outcomes: DeviceClass::ALL
                    .iter()
                    .map(|class| ClassOutcomeDto {
                        device_class: class.as_str().to_owned(),
                        outcome: outcome_for(registry, id, *class),
                    })
                    .collect(),
            })
        })
        .collect();
    PolicyViewDto { tools }
}

/// Ask the engine. This is the only place the answer comes from.
fn outcome_for(
    registry: &ToolRegistry,
    tool_id: &jarvis_domain::tools::ToolId,
    class: DeviceClass,
) -> PolicyOutcomeDto {
    // Empty arguments are faithful, not a shortcut: `evaluate` reaches no
    // decision from a proposal's arguments — it consults the registry's policy,
    // the prohibition tier and the caller's scopes, and nothing else. (That is a
    // known property of this engine, and the reason the volume cap needed two
    // tools rather than one tool with an argument check.) A test asserts it, so
    // the day `evaluate` does start reading arguments, this stops compiling a
    // lie.
    let proposal = ToolProposal {
        tool_id: tool_id.clone(),
        arguments: CanonicalValue::obj([]),
    };
    let ctx = PolicyContext {
        user_id: view_user(),
        device_id: view_device(),
        granted_scopes: scopes_of(class),
    };
    match evaluate(&proposal, registry, &ctx) {
        PolicyDecision::Auto => PolicyOutcomeDto::Auto,
        PolicyDecision::NeedsApproval { .. } => PolicyOutcomeDto::NeedsApproval,
        PolicyDecision::Reject { reason } => PolicyOutcomeDto::Denied {
            reason: deny_reason(&reason),
        },
    }
}

/// The tool scopes a device class actually holds (`DeviceClass::tool_scopes`),
/// so the view answers for the real classes rather than a hypothetical caller.
fn scopes_of(class: DeviceClass) -> BTreeSet<Scope> {
    class.tool_scopes().into_iter().collect()
}

fn deny_reason(reason: &jarvis_application::policy::DenyReason) -> String {
    use jarvis_application::policy::DenyReason as R;
    match reason {
        R::UnknownTool => "unknown_tool".to_owned(),
        R::Prohibited => "prohibited".to_owned(),
        R::MissingScope(scope) => format!("missing_scope:{scope}"),
    }
}

fn risk_name(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::R0 => "R0",
        RiskLevel::R1 => "R1",
        RiskLevel::R2 => "R2",
        RiskLevel::R3 => "R3",
        RiskLevel::R4 => "R4",
    }
}

fn egress_name(egress: DataEgress) -> &'static str {
    match egress {
        DataEgress::None => "none",
        DataEgress::Local => "local",
        DataEgress::External => "external",
    }
}

fn speech_sensitivity_name(sensitivity: SpeechSensitivity) -> &'static str {
    match sensitivity {
        SpeechSensitivity::Normal => "normal",
        SpeechSensitivity::Sensitive => "sensitive",
    }
}

/// Placeholder identities for the evaluation.
///
/// `evaluate` reads neither field — authority comes from `granted_scopes` — and
/// nothing here is persisted or audited, because this route decides nothing. If
/// that ever changes, these become real and this comment becomes a bug report.
fn view_user() -> jarvis_domain::ids::UserId {
    "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("static ulid")
}

fn view_device() -> jarvis_domain::ids::DeviceId {
    "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("static ulid")
}

/// The route. `ui`-scoped by the router's own guard, like every other settings
/// surface — a satellite has no business enumerating the household's policy.
pub async fn get_policy(
    State(view): State<PolicyViewState>,
) -> Result<Json<PolicyViewDto>, Response> {
    Ok(Json(view.snapshot()))
}

/// Handle onto the live registry.
#[derive(Clone)]
pub struct PolicyViewState {
    registry: std::sync::Arc<ToolRegistry>,
}

impl PolicyViewState {
    pub fn new(registry: std::sync::Arc<ToolRegistry>) -> Self {
        Self { registry }
    }

    /// Projected on demand rather than cached at startup: the answer must be
    /// what the engine would say *now*, and a cache is one more thing that can
    /// be stale in exactly the way this feature exists to prevent.
    pub fn snapshot(&self) -> PolicyViewDto {
        project(&self.registry)
    }
}
