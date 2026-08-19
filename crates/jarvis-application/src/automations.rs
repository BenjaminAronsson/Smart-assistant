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
    ///
    /// Takes the sweep's cancellation token (invariant 4). Without it the
    /// daemon had no way to interrupt a firing: one unresponsive tool blocked
    /// every later automation indefinitely, and shutdown could only give up and
    /// detach the task.
    async fn execute(
        &self,
        proposal: &ToolProposal,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), String>;
}

/// How long one automation may take before its executor gives up on it.
///
/// A firing is unattended background work: nobody is waiting on it, and the
/// cost of letting one hang is that every *later* automation stops too. Two
/// minutes is far past any legitimate tool and far short of "until the daemon
/// restarts".
///
/// Declared here so the rule is stated with the thing it governs, but **applied
/// by the adapter** — enforcing it needs a timer, and this crate takes no tokio
/// dependency beyond async traits (invariant 3).
pub const FIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

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

/// Minutes since UTC midnight.
///
/// Computed straight off the epoch rather than through a calendar crate: this
/// crate takes no such dependency (invariant 3), and UTC is already what the
/// daemon's sweep works in, so the two agree by construction rather than by
/// coincidence.
fn minutes_of_day(at: std::time::SystemTime) -> u16 {
    let secs = at
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    ((secs % 86_400) / 60) as u16
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
        cancel: tokio_util::sync::CancellationToken,
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
        self.fire_all(due, now, cancel).await
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

    /// Stamp that the daemon is alive now (M8b). See
    /// [`crate::ports::AutomationStore::record_heartbeat`].
    pub async fn record_heartbeat(
        &self,
        at: std::time::SystemTime,
    ) -> Result<(), crate::ports::RepositoryError> {
        self.store.record_heartbeat(at).await
    }

    /// When the daemon was last known to be running, if ever.
    pub async fn last_heartbeat(
        &self,
    ) -> Result<Option<std::time::SystemTime>, crate::ports::RepositoryError> {
        self.store.last_heartbeat().await
    }

    /// The restart report, from two instants rather than two times of day
    /// (M8b, closes the M8b gate's D2).
    ///
    /// [`Self::missed_since`] takes minutes since midnight, which is the right
    /// shape for a sweep window but cannot express "down for three days". Given
    /// only a time of day, a daemon that had been off all weekend would report a
    /// plausible-looking partial window and silently omit everything else —
    /// which is the same "cannot tell broken from off" failure the restart
    /// report exists to prevent, wearing a more convincing disguise.
    ///
    /// So: past a day of downtime, **every** enabled daily automation was
    /// missed, and this says so rather than doing arithmetic on a wrapped clock.
    pub async fn missed_between(
        &self,
        down_since: std::time::SystemTime,
        now: std::time::SystemTime,
    ) -> Result<Vec<MissedAutomation>, crate::ports::RepositoryError> {
        const DAY: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

        let downtime = now.duration_since(down_since).unwrap_or_default();
        if downtime >= DAY {
            return Ok(self
                .store
                .list_enabled()
                .await?
                .into_iter()
                .filter(|a| {
                    matches!(
                        a.trigger(),
                        jarvis_domain::automations::Trigger::DailyAt { .. }
                    )
                })
                .map(|a| MissedAutomation {
                    automation_id: a.id().clone(),
                    name: a.name().as_str().to_owned(),
                })
                .collect());
        }

        self.missed_since(minutes_of_day(down_since), minutes_of_day(now))
            .await
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
        cancel: tokio_util::sync::CancellationToken,
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
        self.fire_all(due, now, cancel).await
    }

    async fn fire_all(
        &self,
        due: Vec<Automation>,
        now: std::time::SystemTime,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Vec<FiredAutomation>, crate::ports::RepositoryError> {
        let mut fired = Vec::new();
        for automation in due {
            // Shutdown stops the sweep between firings rather than mid-effect:
            // the automations not yet reached are simply not fired, which is the
            // same outcome as the daemon having stopped a minute earlier.
            if cancel.is_cancelled() {
                break;
            }
            let outcome =
                match decide_at_fire_time(&automation, &self.registry, self.authority.as_ref())
                    .await
                {
                    // The executor bounds this by `FIRE_TIMEOUT` and observes
                    // `cancel`; a timeout comes back as an ordinary failure,
                    // because it happened, it did not work, and the history has
                    // to say so.
                    Ok(proposal) => match self.executor.execute(&proposal, cancel.clone()).await {
                        Ok(()) => ExecutionOutcome::Executed,
                        Err(reason) => ExecutionOutcome::Failed { reason },
                    },
                    Err(refusal) => refusal,
                };

            // Recorded whatever happened, and *before* it is reported: the
            // record is what stops a flapping trigger re-firing, so a firing we
            // could not write down must not count as having happened.
            //
            // The audit row travels with it (invariant 6). An automation is the
            // one thing here that acts with nobody watching, so "why did the
            // lights come on at 6am" has to be answerable from the append-only
            // trail — not only from a table the automation module owns and
            // could in principle rewrite.
            //
            // The actor is the **creator**, not `system`: the firing ran on that
            // device's authority, and an audit row naming the daemon would hide
            // exactly the fact that matters when a device is later revoked.
            let audit = jarvis_domain::audit::AuditEvent {
                occurred_at: now,
                actor: format!("device:{}", automation.created_by()),
                event_type: match &outcome {
                    ExecutionOutcome::Executed => "automation.executed",
                    ExecutionOutcome::NeedsApproval { .. } => "automation.needs_approval",
                    ExecutionOutcome::Denied { .. } => "automation.denied",
                    ExecutionOutcome::Failed { .. } => "automation.failed",
                }
                .to_owned(),
                target: format!("automation:{}", automation.id()),
                correlation_id: None,
                // Closed vocabulary only — never the automation's name or the
                // refusal reason, both of which are human text with no business
                // in a hashed audit payload.
                payload_json: format!(
                    r#"{{"toolId":"{}","outcome":"{}"}}"#,
                    automation.action().tool_id.as_str(),
                    outcome.as_str()
                ),
            };

            self.store
                .record_execution(
                    &jarvis_domain::automations::AutomationExecution {
                        automation_id: automation.id().clone(),
                        occurred_at: now,
                        outcome: outcome.clone(),
                    },
                    &audit,
                )
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
        audited: std::sync::Mutex<Vec<jarvis_domain::audit::AuditEvent>>,
        heartbeat: std::sync::Mutex<Option<std::time::SystemTime>>,
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
            _audit: &jarvis_domain::audit::AuditEvent,
        ) -> Result<(), crate::ports::RepositoryError> {
            Ok(())
        }
        async fn delete(
            &self,
            _id: &jarvis_domain::ids::AutomationId,
            _audit: &jarvis_domain::audit::AuditEvent,
        ) -> Result<(), crate::ports::RepositoryError> {
            Ok(())
        }
        async fn record_execution(
            &self,
            execution: &jarvis_domain::automations::AutomationExecution,
            audit: &jarvis_domain::audit::AuditEvent,
        ) -> Result<(), crate::ports::RepositoryError> {
            self.recorded.lock().expect("lock").push(execution.clone());
            self.audited.lock().expect("lock").push(audit.clone());
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
        async fn record_heartbeat(
            &self,
            at: std::time::SystemTime,
        ) -> Result<(), crate::ports::RepositoryError> {
            *self.heartbeat.lock().expect("lock") = Some(at);
            Ok(())
        }
        async fn last_heartbeat(
            &self,
        ) -> Result<Option<std::time::SystemTime>, crate::ports::RepositoryError> {
            Ok(*self.heartbeat.lock().expect("lock"))
        }
    }

    /// Counts what actually reached the world.
    #[derive(Default)]
    struct CountingExecutor(std::sync::Mutex<usize>);

    #[async_trait::async_trait]
    impl AutomationExecutor for CountingExecutor {
        async fn execute(
            &self,
            _proposal: &ToolProposal,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<(), String> {
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
            .sweep_clock(
                419,
                420,
                at(1_000),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("sweeps");
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].outcome, ExecutionOutcome::Executed);
        assert_eq!(*executor.0.lock().expect("lock"), 1);

        // The same window again must not re-fire it: the trigger already passed.
        let again = service
            .sweep_clock(
                420,
                425,
                at(1_100),
                tokio_util::sync::CancellationToken::new(),
            )
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
                .sweep_clock(
                    419,
                    420,
                    at(1_000),
                    tokio_util::sync::CancellationToken::new()
                )
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
            .sweep_clock(
                419,
                420,
                at(1_000),
                tokio_util::sync::CancellationToken::new(),
            )
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
                .sweep_state(
                    "person.guest",
                    "home",
                    at(1_000),
                    tokio_util::sync::CancellationToken::new()
                )
                .await
                .expect("sweeps")
                .is_empty(),
            "another entity's transition is not this automation's"
        );
        assert!(
            service
                .sweep_state(
                    "person.owner",
                    "away",
                    at(1_000),
                    tokio_util::sync::CancellationToken::new()
                )
                .await
                .expect("sweeps")
                .is_empty(),
            "the wrong state is not a trigger either"
        );
        let fired = service
            .sweep_state(
                "person.owner",
                "home",
                at(1_000),
                tokio_util::sync::CancellationToken::new(),
            )
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

    /// A cancelled sweep stops between firings rather than mid-effect.
    ///
    /// Before this the sweep took no token at all: `RegistryExecutor` built a
    /// fresh `CancellationToken::new()` per firing that nothing held the other
    /// end of, so shutdown could not interrupt a sweep and `main.rs` could only
    /// let the drain deadline expire and detach the task (invariant 4).
    #[tokio::test]
    async fn a_cancelled_sweep_fires_nothing_further() {
        let tool = ToolId::home_set_light();
        let store = std::sync::Arc::new(MemStore::default());
        // Two automations due in the same window.
        store
            .automations
            .lock()
            .expect("lock")
            .push(automation_for(tool.clone()));
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

        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();

        let fired = service
            .sweep_clock(419, 420, at(1_000), cancel)
            .await
            .expect("sweeps");

        assert!(fired.is_empty(), "a cancelled sweep fires nothing");
        assert_eq!(
            *executor.0.lock().expect("lock"),
            0,
            "and nothing reaches the world"
        );
    }

    /// The heartbeat round-trips, and a first start reports nothing.
    ///
    /// This is the seam that made M8b's restart report inert in production
    /// (gate D2): the sweep was correct and tested, and the daemon had nowhere
    /// to read "when was I last running" from, so it passed `None` forever.
    #[tokio::test]
    async fn the_last_seen_stamp_round_trips_and_starts_empty() {
        let store = std::sync::Arc::new(MemStore::default());
        let service = service(
            store.clone(),
            registry_with(&ToolId::home_set_light(), RiskLevel::R1, &["home:control"]),
            Authority(Some(scopes(&["home:control"]))),
            std::sync::Arc::new(CountingExecutor::default()),
        );

        assert_eq!(
            service.last_heartbeat().await.expect("reads"),
            None,
            "a first start has no downtime to report, only an uptime to begin"
        );

        let stamp = at(90_000);
        service.record_heartbeat(stamp).await.expect("stamps");
        assert_eq!(service.last_heartbeat().await.expect("reads"), Some(stamp));
    }

    /// Downtime is a duration, and past a day a time-of-day window cannot
    /// describe it.
    ///
    /// A daemon off all weekend that reasoned in minutes-since-midnight would
    /// report a plausible-looking partial window and silently omit the rest —
    /// the same "cannot tell broken from off" failure the restart report exists
    /// to prevent, in a more convincing disguise. Past 24 hours, every daily
    /// automation was missed and it says so.
    #[tokio::test]
    async fn downtime_longer_than_a_day_reports_every_daily_automation() {
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

        // Down three days, and back at the *same* time of day — so the naive
        // time-of-day window is empty and would report nothing at all.
        let now = at(3 * 86_400 + 43_200);
        let down_since = at(43_200);

        let missed = service
            .missed_between(down_since, now)
            .await
            .expect("reports");

        assert_eq!(missed.len(), 1, "three days of downtime missed it");
        assert_eq!(missed[0].name, "evening lights");
        assert_eq!(
            *executor.0.lock().expect("lock"),
            0,
            "still reported, still not run late"
        );
    }

    /// Under a day, the report is the ordinary time-of-day window.
    #[tokio::test]
    async fn downtime_within_a_day_uses_the_time_of_day_window() {
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

        // 06:00 → 11:00 on one day: the 07:00 trigger fell inside.
        assert_eq!(
            service
                .missed_between(at(6 * 3_600), at(11 * 3_600))
                .await
                .expect("reports")
                .len(),
            1
        );
        // 08:00 → 09:00: already past.
        assert!(
            service
                .missed_between(at(8 * 3_600), at(9 * 3_600))
                .await
                .expect("reports")
                .is_empty()
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
