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

/// Runs the tool an automation proposed.
///
/// A separate port from the decision so that "decided" and "did" cannot be
/// confused: `decide_at_fire_time` returns a proposal and cannot execute, and
/// this cannot decide.
#[async_trait::async_trait]
pub trait AutomationExecutor: Send + Sync {
    /// Execute an authorized proposal. `Err` carries a neutral reason — never
    /// raw adapter text (docs/06 §5).
    async fn execute(&self, proposal: &ToolProposal) -> Result<(), String>;
}

/// Sweeps automations and fires the ones whose moment has come (FR-17, F8.7).
pub struct AutomationService {
    store: std::sync::Arc<dyn crate::ports::AutomationStore>,
    registry: std::sync::Arc<crate::policy::ToolRegistry>,
    authority: std::sync::Arc<dyn DeviceAuthority>,
    executor: std::sync::Arc<dyn AutomationExecutor>,
}

/// An automation whose moment passed while the daemon was down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissedAutomation {
    pub automation_id: jarvis_domain::ids::AutomationId,
    /// Carried so the announcement can name it — "the morning lights did not
    /// run" is actionable; "an automation did not run" is not.
    pub name: String,
}

/// What one sweep did, for the daemon's log and the tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiredAutomation {
    pub automation_id: jarvis_domain::ids::AutomationId,
    pub outcome: ExecutionOutcome,
}

impl AutomationService {
    pub fn new(
        store: std::sync::Arc<dyn crate::ports::AutomationStore>,
        registry: std::sync::Arc<crate::policy::ToolRegistry>,
        authority: std::sync::Arc<dyn DeviceAuthority>,
        executor: std::sync::Arc<dyn AutomationExecutor>,
    ) -> Self {
        Self {
            store,
            registry,
            authority,
            executor,
        }
    }

    /// Fire every enabled automation whose trigger fell in `(previous, now]`.
    ///
    /// `now_minutes`/`previous_minutes` are wall-clock minutes since midnight,
    /// passed in rather than read here: the clock enters as data, exactly as it
    /// does for timers, so a sweep is reproducible in a test.
    ///
    /// Every firing is **recorded**, including refusals. A sweep that decided
    /// not to act and did not say so is indistinguishable from one that never
    /// ran.
    pub async fn sweep_clock(
        &self,
        previous_minutes: u16,
        now_minutes: u16,
        now: std::time::SystemTime,
    ) -> Result<Vec<FiredAutomation>, crate::ports::RepositoryError> {
        let due: Vec<_> = self
            .store
            .list_enabled()
            .await?
            .into_iter()
            .filter(|a| {
                a.trigger().fires_in_window(previous_minutes, now_minutes) && a.may_fire_at(now)
            })
            .collect();
        self.fire_all(due, now).await
    }

    /// Report automations whose moment passed while the daemon was down
    /// (M8b exit evidence, the same shape timers already use).
    ///
    /// **Reported, not fired.** A timer that was missed still has to ring —
    /// the owner asked for a noise at a time and the noise is the whole point.
    /// An automation that was missed is a different thing: firing "turn on the
    /// lights at 07:00" at 11:00 because the daemon was off all morning is
    /// worse than not firing it, because the *reason* the owner wanted it has
    /// passed. So the honest behaviour is to say so and skip, rather than to
    /// act late or to say nothing at all.
    ///
    /// Skipping silently is the option this rules out: an owner who comes back
    /// to a house that did nothing cannot tell "the automation is broken" from
    /// "the daemon was off", and those need very different responses.
    pub async fn missed_since(
        &self,
        down_since_minutes: u16,
        now_minutes: u16,
    ) -> Result<Vec<MissedAutomation>, crate::ports::RepositoryError> {
        Ok(self
            .store
            .list_enabled()
            .await?
            .into_iter()
            .filter(|a| a.trigger().fires_in_window(down_since_minutes, now_minutes))
            .map(|a| MissedAutomation {
                automation_id: a.id().clone(),
                name: a.name().as_str().to_owned(),
            })
            .collect())
    }

    /// Fire every enabled automation watching `entity_id` entering `state`.
    ///
    /// Edge-triggered by the caller: this is invoked when the entity *changes*
    /// into the state, not repeatedly while it sits there.
    pub async fn sweep_state(
        &self,
        entity_id: &str,
        state: &str,
        now: std::time::SystemTime,
    ) -> Result<Vec<FiredAutomation>, crate::ports::RepositoryError> {
        let due: Vec<_> = self
            .store
            .list_enabled()
            .await?
            .into_iter()
            .filter(|a| {
                matches!(
                    a.trigger(),
                    jarvis_domain::automations::Trigger::HomeAssistantState { entity_id: e, state: s }
                        if e == entity_id && s == state
                ) && a.may_fire_at(now)
            })
            .collect();
        self.fire_all(due, now).await
    }

    async fn fire_all(
        &self,
        due: Vec<Automation>,
        now: std::time::SystemTime,
    ) -> Result<Vec<FiredAutomation>, crate::ports::RepositoryError> {
        let mut fired = Vec::new();
        for automation in due {
            let outcome =
                match decide_at_fire_time(&automation, &self.registry, self.authority.as_ref())
                    .await
                {
                    Ok(proposal) => match self.executor.execute(&proposal).await {
                        Ok(()) => ExecutionOutcome::Executed,
                        Err(reason) => ExecutionOutcome::Failed { reason },
                    },
                    Err(refusal) => refusal,
                };

            // Recorded whatever happened, and *before* it is reported: the
            // record is what stops a flapping trigger re-firing, so a firing we
            // could not write down must not count as having happened.
            self.store
                .record_execution(&jarvis_domain::automations::AutomationExecution {
                    automation_id: automation.id().clone(),
                    occurred_at: now,
                    outcome: outcome.clone(),
                })
                .await?;
            fired.push(FiredAutomation {
                automation_id: automation.id().clone(),
                outcome,
            });
        }
        Ok(fired)
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

    // ---- the sweep (F8.7) --------------------------------------------------

    /// Records everything, refuses nothing — the store under test.
    #[derive(Default)]
    struct MemStore {
        automations: std::sync::Mutex<Vec<Automation>>,
        recorded: std::sync::Mutex<Vec<jarvis_domain::automations::AutomationExecution>>,
    }

    #[async_trait::async_trait]
    impl crate::ports::AutomationStore for MemStore {
        async fn create(
            &self,
            _a: &Automation,
            _audit: &jarvis_domain::audit::AuditEvent,
        ) -> Result<(), crate::ports::RepositoryError> {
            Ok(())
        }
        async fn list_enabled(&self) -> Result<Vec<Automation>, crate::ports::RepositoryError> {
            Ok(self
                .automations
                .lock()
                .expect("lock")
                .iter()
                .filter(|a| a.is_enabled())
                .cloned()
                .collect())
        }
        async fn list_all(&self) -> Result<Vec<Automation>, crate::ports::RepositoryError> {
            Ok(self.automations.lock().expect("lock").clone())
        }
        async fn set_enabled(
            &self,
            _id: &jarvis_domain::ids::AutomationId,
            _enabled: bool,
        ) -> Result<(), crate::ports::RepositoryError> {
            Ok(())
        }
        async fn delete(
            &self,
            _id: &jarvis_domain::ids::AutomationId,
        ) -> Result<(), crate::ports::RepositoryError> {
            Ok(())
        }
        async fn record_execution(
            &self,
            execution: &jarvis_domain::automations::AutomationExecution,
        ) -> Result<(), crate::ports::RepositoryError> {
            self.recorded.lock().expect("lock").push(execution.clone());
            Ok(())
        }
        async fn history(
            &self,
            _id: &jarvis_domain::ids::AutomationId,
            _limit: i64,
        ) -> Result<
            Vec<jarvis_domain::automations::AutomationExecution>,
            crate::ports::RepositoryError,
        > {
            Ok(Vec::new())
        }
    }

    /// Counts what actually reached the world.
    #[derive(Default)]
    struct CountingExecutor(std::sync::Mutex<usize>);

    #[async_trait::async_trait]
    impl AutomationExecutor for CountingExecutor {
        async fn execute(&self, _proposal: &ToolProposal) -> Result<(), String> {
            *self.0.lock().expect("lock") += 1;
            Ok(())
        }
    }

    fn service(
        store: std::sync::Arc<MemStore>,
        registry: ToolRegistry,
        authority: Authority,
        executor: std::sync::Arc<CountingExecutor>,
    ) -> AutomationService {
        AutomationService::new(
            store,
            std::sync::Arc::new(registry),
            std::sync::Arc::new(authority),
            executor,
        )
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    /// A trigger fires once and only once — the feature list's first named test.
    #[tokio::test]
    async fn a_trigger_fires_once_and_only_once() {
        let tool = ToolId::home_set_light();
        let store = std::sync::Arc::new(MemStore::default());
        store
            .automations
            .lock()
            .expect("lock")
            .push(automation_for(tool.clone()));
        let executor = std::sync::Arc::new(CountingExecutor::default());
        let service = service(
            store.clone(),
            registry_with(&tool, RiskLevel::R1, &["home:control"]),
            Authority(Some(scopes(&["home:control"]))),
            executor.clone(),
        );

        // 07:00 falls in this window.
        let fired = service
            .sweep_clock(419, 420, at(1_000))
            .await
            .expect("sweeps");
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].outcome, ExecutionOutcome::Executed);
        assert_eq!(*executor.0.lock().expect("lock"), 1);

        // The same window again must not re-fire it: the trigger already passed.
        let again = service
            .sweep_clock(420, 425, at(1_100))
            .await
            .expect("sweeps");
        assert!(again.is_empty(), "the moment does not come round twice");
        assert_eq!(*executor.0.lock().expect("lock"), 1);
    }

    /// A disabled automation does not fire — the second named test.
    #[tokio::test]
    async fn a_disabled_automation_does_not_fire() {
        let tool = ToolId::home_set_light();
        let store = std::sync::Arc::new(MemStore::default());
        let mut disabled = automation_for(tool.clone());
        disabled.set_enabled(false);
        store.automations.lock().expect("lock").push(disabled);
        let executor = std::sync::Arc::new(CountingExecutor::default());
        let service = service(
            store.clone(),
            registry_with(&tool, RiskLevel::R1, &["home:control"]),
            Authority(Some(scopes(&["home:control"]))),
            executor.clone(),
        );

        assert!(
            service
                .sweep_clock(419, 420, at(1_000))
                .await
                .expect("sweeps")
                .is_empty()
        );
        assert_eq!(*executor.0.lock().expect("lock"), 0);
        assert!(store.recorded.lock().expect("lock").is_empty());
    }

    /// **Policy denial at fire time is recorded and visible** — the third named
    /// test, and the one that makes a refusal answerable from the sofa.
    #[tokio::test]
    async fn a_denial_at_fire_time_is_recorded_and_nothing_executes() {
        let tool = ToolId::home_set_light();
        let store = std::sync::Arc::new(MemStore::default());
        store
            .automations
            .lock()
            .expect("lock")
            .push(automation_for(tool.clone()));
        let executor = std::sync::Arc::new(CountingExecutor::default());
        let service = service(
            store.clone(),
            registry_with(&tool, RiskLevel::R1, &["home:control"]),
            // The creator has been revoked since it was created.
            Authority(None),
            executor.clone(),
        );

        let fired = service
            .sweep_clock(419, 420, at(1_000))
            .await
            .expect("sweeps");
        assert_eq!(fired.len(), 1);
        assert!(matches!(fired[0].outcome, ExecutionOutcome::Denied { .. }));
        assert_eq!(
            *executor.0.lock().expect("lock"),
            0,
            "a denied automation must not reach the world"
        );
        let recorded = store.recorded.lock().expect("lock");
        assert_eq!(
            recorded.len(),
            1,
            "the refusal must be recorded, not swallowed"
        );
        assert!(matches!(
            recorded[0].outcome,
            ExecutionOutcome::Denied { .. }
        ));
    }

    /// A state trigger fires on its entity and not on somebody else's.
    #[tokio::test]
    async fn a_state_trigger_fires_only_for_its_own_entity() {
        let tool = ToolId::home_set_light();
        let store = std::sync::Arc::new(MemStore::default());
        let mut watcher = automation_for(tool.clone());
        watcher = Automation::from_parts(
            watcher.id().clone(),
            AutomationName::new("arrive home").expect("name"),
            Trigger::HomeAssistantState {
                entity_id: "person.owner".into(),
                state: "home".into(),
            },
            watcher.action().clone(),
            true,
            watcher.created_by().clone(),
            watcher.created_at(),
            None,
        );
        store.automations.lock().expect("lock").push(watcher);
        let executor = std::sync::Arc::new(CountingExecutor::default());
        let service = service(
            store.clone(),
            registry_with(&tool, RiskLevel::R1, &["home:control"]),
            Authority(Some(scopes(&["home:control"]))),
            executor.clone(),
        );

        assert!(
            service
                .sweep_state("person.guest", "home", at(1_000))
                .await
                .expect("sweeps")
                .is_empty(),
            "another entity's transition is not this automation's"
        );
        assert!(
            service
                .sweep_state("person.owner", "away", at(1_000))
                .await
                .expect("sweeps")
                .is_empty(),
            "the wrong state is not a trigger either"
        );
        let fired = service
            .sweep_state("person.owner", "home", at(1_000))
            .await
            .expect("sweeps");
        assert_eq!(fired.len(), 1);
        assert_eq!(*executor.0.lock().expect("lock"), 1);
    }

    /// M8b's exit evidence: a run missed while the daemon was down is
    /// announced rather than silently skipped.
    #[tokio::test]
    async fn an_automation_missed_while_the_daemon_was_down_is_reported() {
        let tool = ToolId::home_set_light();
        let store = std::sync::Arc::new(MemStore::default());
        store
            .automations
            .lock()
            .expect("lock")
            .push(automation_for(tool.clone()));
        let executor = std::sync::Arc::new(CountingExecutor::default());
        let service = service(
            store.clone(),
            registry_with(&tool, RiskLevel::R1, &["home:control"]),
            Authority(Some(scopes(&["home:control"]))),
            executor.clone(),
        );

        // Down from 06:00 to 11:00; the 07:00 trigger fell inside that.
        let missed = service.missed_since(360, 660).await.expect("reports");

        assert_eq!(missed.len(), 1);
        // Named, so the announcement is actionable: "the evening lights did
        // not run" beats "an automation did not run".
        assert_eq!(missed[0].name, "evening lights");
        // Reported, NOT fired: acting on "turn the lights on at 07:00" at
        // 11:00 is worse than not acting, because the reason has passed.
        assert_eq!(
            *executor.0.lock().expect("lock"),
            0,
            "a missed automation must not be run late"
        );
        assert!(
            store.recorded.lock().expect("lock").is_empty(),
            "reporting is not firing; nothing is recorded as having happened"
        );
    }

    #[tokio::test]
    async fn an_automation_outside_the_downtime_is_not_reported_as_missed() {
        let tool = ToolId::home_set_light();
        let store = std::sync::Arc::new(MemStore::default());
        store
            .automations
            .lock()
            .expect("lock")
            .push(automation_for(tool.clone()));
        let service = service(
            store,
            registry_with(&tool, RiskLevel::R1, &["home:control"]),
            Authority(Some(scopes(&["home:control"]))),
            std::sync::Arc::new(CountingExecutor::default()),
        );

        // Down 08:00–09:00; the 07:00 trigger was already past.
        assert!(
            service
                .missed_since(480, 540)
                .await
                .expect("reports")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_disabled_automation_is_never_reported_as_missed() {
        let tool = ToolId::home_set_light();
        let store = std::sync::Arc::new(MemStore::default());
        let mut disabled = automation_for(tool.clone());
        disabled.set_enabled(false);
        store.automations.lock().expect("lock").push(disabled);
        let service = service(
            store,
            registry_with(&tool, RiskLevel::R1, &["home:control"]),
            Authority(Some(scopes(&["home:control"]))),
            std::sync::Arc::new(CountingExecutor::default()),
        );
        assert!(
            service
                .missed_since(360, 660)
                .await
                .expect("reports")
                .is_empty()
        );
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
