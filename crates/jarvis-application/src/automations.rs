//! Firing automations (FR-17, F8.6).
//!
//! The domain type refuses to store authority; this is the half that refuses to
//! *assume* it. One function carries the whole security property:
//! [`decide_at_fire_time`] resolves the creator's authority **now**, builds a
//! [`PolicyContext`] from it, and runs the same `policy::evaluate` every other
//! tool call goes through.
//!
//! Three things it must never do, each of which would be a plausible shortcut:
//!
//! 1. **Reuse a decision from creation time.** An automation is a stored
//!    intention; authority is not part of it.
//! 2. **Run as `system`.** The tempting shape — "the daemon fires it, so the
//!    daemon's authority applies" — would make every automation a superuser and
//!    turn `POST /automations` into privilege escalation for any device that
//!    can reach it.
//! 3. **Treat a missing creator as permission.** A revoked or deleted device
//!    resolves to *no* scopes, which denies. The failure mode of the opposite
//!    choice is an automation that keeps acting for a device the owner
//!    deliberately switched off.

use std::collections::BTreeSet;

use jarvis_domain::automations::{Automation, ExecutionOutcome};
use jarvis_domain::ids::{DeviceId, UserId};
use jarvis_domain::policy::Scope;
use jarvis_domain::tools::ToolProposal;

use crate::policy::{self, PolicyContext, PolicyDecision, ToolRegistry};

/// What the daemon knows about a device *right now*.
///
/// A trait rather than a struct so the runner cannot be handed a stale
/// snapshot: the implementation reads the live device row, and a device that
/// has been revoked resolves to `None`.
#[async_trait::async_trait]
pub trait DeviceAuthority: Send + Sync {
    /// The scopes this device holds at this moment, or `None` if it no longer
    /// exists or has been revoked.
    async fn scopes_of(&self, device: &DeviceId) -> Option<(UserId, BTreeSet<Scope>)>;
}

/// The policy decision for one firing, made now, from current authority.
///
/// Returns the outcome to record rather than executing anything: recording is a
/// transaction the caller owns (invariant 6), and a function that both decides
/// and acts is one that can act without recording.
pub async fn decide_at_fire_time(
    automation: &Automation,
    registry: &ToolRegistry,
    authority: &dyn DeviceAuthority,
) -> Result<ToolProposal, ExecutionOutcome> {
    // Resolved now, never cached. This lookup *is* the feature.
    let Some((user_id, granted_scopes)) = authority.scopes_of(automation.created_by()).await else {
        return Err(ExecutionOutcome::Denied {
            // Named precisely: "the device that created this is gone" is the
            // answer an owner needs when they ask why the lights stopped.
            reason: "the device that created this automation no longer has authority".to_owned(),
        });
    };

    let proposal = ToolProposal {
        tool_id: automation.action().tool_id.clone(),
        arguments: automation.action().arguments.clone(),
    };
    let ctx = PolicyContext {
        user_id,
        device_id: automation.created_by().clone(),
        granted_scopes,
    };

    match policy::evaluate(&proposal, registry, &ctx) {
        PolicyDecision::Auto => Ok(proposal),
        // An automation firing at 6am has nobody to ask. Recording it as a
        // refusal with the exact effect beats queueing a prompt nobody will
        // ever see, and beats silently doing nothing.
        PolicyDecision::NeedsApproval { exact_effect } => {
            Err(ExecutionOutcome::NeedsApproval { exact_effect })
        }
        PolicyDecision::Reject { reason } => Err(ExecutionOutcome::Denied {
            reason: reason.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::automations::{AutomationAction, AutomationName, Trigger};
    use jarvis_domain::policy::{DataEgress, RiskLevel, ToolPolicy};
    use jarvis_domain::tools::{CanonicalValue, ToolId};
    use std::time::SystemTime;

    const CREATOR: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    /// Answers with whatever the test set up — including "this device is gone".
    struct Authority(Option<BTreeSet<Scope>>);

    #[async_trait::async_trait]
    impl DeviceAuthority for Authority {
        async fn scopes_of(&self, _device: &DeviceId) -> Option<(UserId, BTreeSet<Scope>)> {
            self.0.clone().map(|scopes| {
                (
                    "01ARZ3NDEKTSV4RRFFQ69G5FB9".parse().expect("user id"),
                    scopes,
                )
            })
        }
    }

    fn scopes(names: &[&str]) -> BTreeSet<Scope> {
        names
            .iter()
            .map(|n| Scope::new(*n).expect("scope"))
            .collect()
    }

    /// An executor that is never reached: every test here stops at the policy
    /// decision, which is the point — `decide_at_fire_time` decides, it does
    /// not act.
    struct Unreached;

    #[async_trait::async_trait]
    impl crate::policy::ToolExecutor for Unreached {
        async fn execute(
            &self,
            _invocation: jarvis_domain::tools::ToolInvocation,
            _grant: Option<jarvis_domain::grants::ExecutionGrant>,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<jarvis_domain::tools::ToolResult, jarvis_domain::tools::ToolError> {
            unreachable!("a policy decision must never execute anything")
        }
    }

    /// A registry holding one tool at `risk`, requiring `required`.
    fn registry_with(tool_id: &ToolId, risk: RiskLevel, required: &[&str]) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry
            .register(crate::policy::ToolDescriptor {
                id: tool_id.clone(),
                version: jarvis_domain::tools::ToolVersion::new(1, 0, 0),
                policy: Some(ToolPolicy {
                    risk,
                    is_reversible: true,
                    requires_user_presence: false,
                    timeout: std::time::Duration::from_secs(5),
                    required_scopes: required
                        .iter()
                        .map(|s| Scope::new(*s).expect("scope"))
                        .collect(),
                    egress: DataEgress::None,
                }),
                executor: std::sync::Arc::new(Unreached),
            })
            .expect("registers");
        registry
    }

    fn automation_for(tool_id: ToolId) -> Automation {
        Automation::create(
            "01ARZ3NDEKTSV4RRFFQ69G5FB1".parse().expect("id"),
            AutomationName::new("evening lights").expect("name"),
            Trigger::DailyAt {
                minutes_since_midnight: 420,
            },
            AutomationAction {
                tool_id,
                arguments: CanonicalValue::Null,
            },
            CREATOR.parse().expect("device id"),
            SystemTime::UNIX_EPOCH,
        )
    }

    /// **The** test: an automation cannot mint authority its creator does not
    /// have. The creator holds nothing, so the same automation that would be
    /// allowed for an owner is refused.
    #[tokio::test]
    async fn an_automation_cannot_mint_authority_its_creator_lacks() {
        let tool = ToolId::home_set_light();
        // Registered, and requiring an authority the creator does not hold.
        let registry = registry_with(&tool, RiskLevel::R1, &["home:control"]);
        let automation = automation_for(tool);

        let outcome = decide_at_fire_time(&automation, &registry, &Authority(Some(scopes(&[]))))
            .await
            .expect_err("must be refused");

        assert!(
            matches!(outcome, ExecutionOutcome::Denied { .. }),
            "expected a denial, got {outcome:?}"
        );
    }

    /// Revocation reaches automations. Without this, revoking the kitchen
    /// tablet leaves behind something that still acts with its authority.
    #[tokio::test]
    async fn a_revoked_creator_denies_the_firing() {
        let tool = ToolId::home_set_light();
        let registry = registry_with(&tool, RiskLevel::R1, &["home:control"]);
        let automation = automation_for(tool);

        // `None` — the device is gone.
        let outcome = decide_at_fire_time(&automation, &registry, &Authority(None))
            .await
            .expect_err("must be refused");

        match outcome {
            ExecutionOutcome::Denied { reason } => {
                assert!(
                    reason.contains("no longer has authority"),
                    "the reason must say why: {reason}"
                );
            }
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    /// The positive case, so the tests are not all refusals: a creator who
    /// genuinely holds the scope gets an executable proposal.
    #[tokio::test]
    async fn a_creator_who_holds_the_scope_gets_an_executable_proposal() {
        let tool = ToolId::home_set_light();
        let registry = registry_with(&tool, RiskLevel::R1, &["home:control"]);
        let automation = automation_for(tool.clone());

        let proposal = decide_at_fire_time(
            &automation,
            &registry,
            &Authority(Some(scopes(&["home:control"]))),
        )
        .await
        .expect("must be allowed");
        assert_eq!(proposal.tool_id, tool);
    }

    /// A denial is recorded, not swallowed — "it ran and did nothing" and "it
    /// was refused" look identical from the sofa otherwise.
    #[tokio::test]
    async fn an_unregistered_tool_is_denied_with_a_reason() {
        // Nothing registered at all: an unknown tool is refused, not assumed.
        let registry = ToolRegistry::new();
        let automation = automation_for(ToolId::home_set_light());

        let outcome = decide_at_fire_time(
            &automation,
            &registry,
            &Authority(Some(scopes(&["home:control"]))),
        )
        .await
        .expect_err("must be refused");

        assert!(matches!(outcome, ExecutionOutcome::Denied { .. }));
    }

    /// An automation firing at 6am has nobody to ask, so an approval-requiring
    /// action is a recorded refusal rather than a prompt queued forever.
    #[tokio::test]
    async fn an_action_needing_approval_is_refused_and_says_exactly_what_it_wanted() {
        let tool = ToolId::home_execute_scene();
        // R2: the creator holds the scope, but the action still needs a human.
        let registry = registry_with(&tool, RiskLevel::R2, &["home:control"]);
        let automation = automation_for(tool);

        let outcome = decide_at_fire_time(
            &automation,
            &registry,
            &Authority(Some(scopes(&["home:control"]))),
        )
        .await;

        match outcome {
            Err(ExecutionOutcome::NeedsApproval { exact_effect }) => {
                assert!(
                    exact_effect.contains("home."),
                    "the record must name the exact effect: {exact_effect}"
                );
            }
            other => panic!("an R2 action must not fire unattended: {other:?}"),
        }
    }
}
