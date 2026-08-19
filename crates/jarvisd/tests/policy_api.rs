//! F10.5: the rendered policy must match what the engine actually decides.
//!
//! The feature's acceptance, verbatim: "the rendered policy matches
//! `policy::evaluate`'s actual decisions for the same inputs — a UI that
//! *describes* different rules than the engine enforces is worse than none."
//!
//! `policy_view` is built so it cannot describe different rules: it obtains each
//! outcome by calling `evaluate` rather than re-deriving one from the policy
//! fields. That removes the drift risk but not the *input* risk — a projection
//! that asks the engine the wrong question is just as wrong, and looks just as
//! confident. These tests pin the inputs.

use std::collections::BTreeSet;
use std::time::Duration;

use std::sync::Arc;

use jarvis_application::policy::{
    PolicyContext, PolicyDecision, ToolDescriptor, ToolExecutor, ToolRegistry, evaluate,
};
use jarvis_contracts::policy::PolicyOutcomeDto;
use jarvis_domain::identity::DeviceClass;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{CanonicalValue, ToolProposal};
use jarvisd::policy_view::project;

/// Never executed — this feature only ever *reads* policy. A tool must have an
/// executor to be registrable, so the view is tested against a registry built
/// exactly as the real one is, rather than a hand-made policy map.
struct NeverRuns;

#[async_trait::async_trait]
impl ToolExecutor for NeverRuns {
    async fn execute(
        &self,
        _invocation: jarvis_domain::tools::ToolInvocation,
        _grant: Option<jarvis_domain::grants::ExecutionGrant>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<jarvis_domain::tools::ToolResult, jarvis_domain::tools::ToolError> {
        unreachable!("the policy view never executes anything")
    }
}

fn scope(s: &str) -> Scope {
    Scope::new(s).expect("valid scope")
}

fn policy(risk: RiskLevel, scopes: &[&str]) -> ToolPolicy {
    ToolPolicy {
        risk,
        is_reversible: true,
        requires_user_presence: false,
        timeout: Duration::from_secs(5),
        required_scopes: scopes.iter().map(|s| scope(s)).collect(),
        egress: DataEgress::Local,
    }
}

/// A registry spanning every branch `evaluate` can take: auto, approval,
/// prohibited, and missing-scope.
fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for (id, risk, scopes) in [
        ("example.read", RiskLevel::R0, &["files:read"][..]),
        ("example.light", RiskLevel::R1, &["home:control"][..]),
        ("example.send", RiskLevel::R2, &["message:send"][..]),
        ("example.patch", RiskLevel::R3, &["coding:patch"][..]),
        // Requires a scope **nobody holds** — not even the owner, whose
        // `OWNER_TOOL_SCOPES` has `files:read` but no `files:write`. Kept
        // deliberately: an owner reading this view should learn that such a
        // tool is unreachable for everyone, which is exactly the kind of thing
        // config-only policy hid.
        ("example.wipe", RiskLevel::R3, &["files:write"][..]),
        ("example.forbidden", RiskLevel::R4, &[][..]),
    ] {
        registry
            .register(ToolDescriptor {
                id: id.parse().expect("tool id"),
                version: jarvis_domain::tools::ToolVersion::new(1, 0, 0),
                policy: Some(policy(risk, scopes)),
                executor: Arc::new(NeverRuns),
            })
            .expect("registers");
    }
    registry
}

/// **The acceptance, stated as a test.** For every tool and every device class,
/// what the view shows is what the engine says — compared against a *fresh*
/// call to `evaluate`, not against the value the view computed.
#[test]
fn every_rendered_outcome_matches_what_the_engine_decides() {
    let registry = registry();
    let view = project(&registry);

    assert!(!view.tools.is_empty(), "the fixture registry is not empty");

    for tool in &view.tools {
        for shown in &tool.outcomes {
            let class = DeviceClass::ALL
                .iter()
                .find(|c| c.as_str() == shown.device_class)
                .unwrap_or_else(|| panic!("unknown class {}", shown.device_class));

            let granted: BTreeSet<Scope> = class.tool_scopes().into_iter().collect();
            let truth = evaluate(
                &ToolProposal {
                    tool_id: tool.tool_id.parse().expect("tool id"),
                    arguments: CanonicalValue::obj([]),
                },
                &registry,
                &PolicyContext {
                    user_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("ulid"),
                    device_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("ulid"),
                    granted_scopes: granted,
                },
            );

            let expected = match truth {
                PolicyDecision::Auto => PolicyOutcomeDto::Auto,
                PolicyDecision::NeedsApproval { .. } => PolicyOutcomeDto::NeedsApproval,
                PolicyDecision::Reject { reason } => PolicyOutcomeDto::Denied {
                    reason: match reason {
                        jarvis_application::policy::DenyReason::UnknownTool => {
                            "unknown_tool".to_owned()
                        }
                        jarvis_application::policy::DenyReason::Prohibited => {
                            "prohibited".to_owned()
                        }
                        jarvis_application::policy::DenyReason::MissingScope(s) => {
                            format!("missing_scope:{s}")
                        }
                    },
                },
            };

            assert_eq!(
                shown.outcome, expected,
                "the view shows {:?} for {} on {}, the engine decides {expected:?}",
                shown.outcome, tool.tool_id, shown.device_class
            );
        }
    }
}

/// A satellite must not be shown as able to run tools it cannot run.
///
/// The concrete regression this guards is the **fixture-vs-caller** bug class
/// that has bitten this project five times, most memorably at the M6 gate where
/// paired devices held only the `ui` scope and every approved action was denied
/// in production while the tests stayed green. Here the danger is the mirror
/// image: a view that invents a plausible scope set instead of reading the one
/// the class really has would show an owner that their kitchen node can send
/// messages. It cannot.
#[test]
fn a_node_is_shown_as_denied_because_it_holds_no_tool_scopes() {
    let view = project(&registry());

    for tool in &view.tools {
        for class in ["voice-node", "room-node", "display-node"] {
            let shown = tool
                .outcomes
                .iter()
                .find(|o| o.device_class == class)
                .unwrap_or_else(|| panic!("{class} missing from the view"));
            assert!(
                matches!(shown.outcome, PolicyOutcomeDto::Denied { .. }),
                "{class} holds no tool scopes, so {} must render as denied, not {:?}",
                tool.tool_id,
                shown.outcome
            );
        }
    }
}

/// The owner sees the tiers as tiers: R0/R1 auto, R2/R3 approval, R4 refused —
/// and a scope they do not hold as refused, whatever its tier.
///
/// That last case was found by writing this test: `OWNER_TOOL_SCOPES` grants
/// `files:read` and no `files:write`, so an R3 tool needing it is denied for
/// *everyone*. Showing it as "needs approval" would promise a dialog that can
/// never appear.
#[test]
fn the_owner_sees_each_tier_as_the_engine_treats_it() {
    let view = project(&registry());
    let owner_outcome = |id: &str| {
        view.tools
            .iter()
            .find(|t| t.tool_id == id)
            .unwrap_or_else(|| panic!("{id} missing"))
            .outcomes
            .iter()
            .find(|o| o.device_class == "owner-ui")
            .expect("owner-ui")
            .outcome
            .clone()
    };

    assert_eq!(owner_outcome("example.read"), PolicyOutcomeDto::Auto);
    assert_eq!(owner_outcome("example.light"), PolicyOutcomeDto::Auto);
    assert_eq!(
        owner_outcome("example.send"),
        PolicyOutcomeDto::NeedsApproval
    );
    assert_eq!(
        owner_outcome("example.patch"),
        PolicyOutcomeDto::NeedsApproval
    );
    assert_eq!(
        owner_outcome("example.wipe"),
        PolicyOutcomeDto::Denied {
            reason: "missing_scope:files:write".to_owned()
        },
        "the owner holds files:read but not files:write, so an R3 tool needing it \
         is refused outright rather than offered for approval — the view must show \
         that, not a hopeful 'needs approval'"
    );
    assert_eq!(
        owner_outcome("example.forbidden"),
        PolicyOutcomeDto::Denied {
            reason: "prohibited".to_owned()
        },
        "R4 is refused outright, never 'approvable' — there is no override through \
         conversation, and the UI must not suggest one"
    );
}

/// `policy_view` evaluates with empty arguments, which is only faithful while
/// `evaluate` ignores arguments. It does today. If that ever changes, the view
/// starts answering a different question than the engine, silently — so pin it
/// here rather than in a comment.
#[test]
fn evaluate_ignores_arguments_which_is_what_makes_the_view_faithful() {
    let registry = registry();
    let ctx = PolicyContext {
        user_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("ulid"),
        device_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("ulid"),
        granted_scopes: DeviceClass::OwnerUi.tool_scopes().into_iter().collect(),
    };
    let with = |arguments: CanonicalValue| {
        evaluate(
            &ToolProposal {
                tool_id: "example.light".parse().expect("tool id"),
                arguments,
            },
            &registry,
            &ctx,
        )
    };

    assert_eq!(
        with(CanonicalValue::obj([])),
        with(CanonicalValue::obj([
            ("entity_id", CanonicalValue::str("light.kitchen")),
            ("brightness", CanonicalValue::Int(255)),
        ])),
        "policy_view renders outcomes using empty arguments; that is only honest \
         while arguments cannot change the decision"
    );
}
