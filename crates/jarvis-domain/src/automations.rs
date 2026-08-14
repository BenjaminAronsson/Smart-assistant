//! Automations (FR-17, docs/02 §11, docs/04 §3).
//!
//! The requirement `docs/05 §1` has advertised routes for since M0, parked
//! twice, and finally scheduled as M8b.
//!
//! # The one idea this module exists to enforce
//!
//! **An automation is a stored *intention*, not a stored *authorization*.**
//!
//! Everything else here follows from that sentence. It is why there is no
//! `scopes` field, no cached `PolicyDecision`, and no "approved" flag: an
//! automation records *what the owner wants to happen*, and the authority to
//! actually do it is resolved **at fire time, every time**, from the creator's
//! authority as it stands at that moment.
//!
//! Consider the alternative. If an automation cached the decision made when it
//! was created, then revoking the kitchen tablet would leave behind a stored
//! object that still turns on the heating at 6am with the tablet's authority,
//! forever, and the only way to stop it would be to remember it exists. Worse,
//! an automation created while a device briefly held a scope would keep that
//! scope after it was taken away — a durable privilege escalation with a
//! friendly name and a nice card in the UI.
//!
//! So: [`Automation`] stores **who** asked (`created_by`), never **what they
//! were allowed**. Resolving a device to its current scopes is the caller's
//! job, and the caller must do it fresh (see `jarvis_application::automations`).
//!
//! The boundary with timers (ADR-023) is the same one from the other side: a
//! timer means "make a noise at T" and needs no policy at all; anything that
//! needs policy re-evaluated or a model consulted at fire time is an automation
//! and lives here.

use std::fmt;
use std::time::{Duration, SystemTime};

use crate::ids::{AutomationId, DeviceId};
use crate::tools::{CanonicalValue, ToolId};

/// How often an automation may fire, at most. A trigger that could fire on
/// every evaluation would let one automation saturate the tool executor.
pub const MIN_REFIRE_INTERVAL: Duration = Duration::from_secs(30);

/// Human label, sanitized and bounded like every other display string here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationName(String);

/// Longest label we store. Matches `TimerName`'s bound for the same reason: it
/// is rendered on a card next to one.
pub const MAX_NAME_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AutomationNameError {
    #[error("automation name is empty")]
    Empty,
    #[error("automation name is too long")]
    TooLong,
}

impl AutomationName {
    pub fn new(raw: &str) -> Result<Self, AutomationNameError> {
        // Control characters are stripped rather than rejected: the name comes
        // from a human typing into a box, and a stray tab is not an attack.
        let cleaned: String = raw
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .trim()
            .to_owned();
        if cleaned.is_empty() {
            return Err(AutomationNameError::Empty);
        }
        if cleaned.len() > MAX_NAME_BYTES {
            return Err(AutomationNameError::TooLong);
        }
        Ok(Self(cleaned))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AutomationName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What makes an automation fire.
///
/// Deliberately a closed set. A trigger is evaluated by the daemon on a
/// schedule, so an open-ended predicate — anything a model could author — would
/// be a code path from model output to a tool call, which invariant 1 forbids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// Every day at a wall-clock time, in the daemon's local zone.
    ///
    /// Minutes since midnight rather than a `SystemTime`, because "07:00" means
    /// seven in the morning tomorrow as well as today — a stored instant would
    /// fire once and never again.
    DailyAt { minutes_since_midnight: u16 },
    /// A Home Assistant entity entering a state (FR-17: presence and zone).
    ///
    /// Edge-triggered: it fires on the *transition* into `state`, not for every
    /// evaluation while the entity sits there. Otherwise "when I get home" would
    /// fire every thirty seconds all evening.
    HomeAssistantState { entity_id: String, state: String },
}

impl Trigger {
    /// Whether a daily trigger's moment falls in the half-open window
    /// `(previous, now]`.
    ///
    /// A window rather than an equality test: the scheduler wakes when it wakes,
    /// and an automation must not be skipped because nothing asked at exactly
    /// 07:00:00.
    pub fn fires_in_window(&self, previous_minutes: u16, now_minutes: u16) -> bool {
        let Self::DailyAt {
            minutes_since_midnight,
        } = self
        else {
            return false;
        };
        let target = *minutes_since_midnight;
        if previous_minutes <= now_minutes {
            target > previous_minutes && target <= now_minutes
        } else {
            // The window crossed midnight; it is two ranges, not one.
            target > previous_minutes || target <= now_minutes
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::DailyAt { .. } => "daily_at",
            Self::HomeAssistantState { .. } => "ha_state",
        }
    }
}

/// The tool call an automation proposes when it fires.
///
/// A *proposal*, in the same sense as a model's: it is what the automation
/// would like to happen, and it goes through `policy::evaluate` exactly like
/// anything else. Text never grants authority (invariant 1), and neither does
/// a row in a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationAction {
    pub tool_id: ToolId,
    pub arguments: CanonicalValue,
}

/// A stored automation.
///
/// Note what is **absent**, and why:
///
/// * no `scopes` — see the module docs; caching authority is the bug this whole
///   design is shaped to prevent;
/// * no cached `PolicyDecision` — the decision is made at fire time;
/// * no `approved` flag — an approval is a grant, grants expire and are bound
///   to a run (ADR-005/F2.4), and an automation is none of those things.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Automation {
    id: AutomationId,
    name: AutomationName,
    trigger: Trigger,
    action: AutomationAction,
    enabled: bool,
    /// The device whose authority this automation borrows **at fire time**.
    ///
    /// Not "the device that is allowed to run it" — the device whose *current*
    /// authority is consulted. If it has been revoked, the automation is denied
    /// and says so, which is the correct outcome and the one an owner expects
    /// after revoking a device.
    created_by: DeviceId,
    created_at: SystemTime,
    last_fired_at: Option<SystemTime>,
}

impl Automation {
    pub fn create(
        id: AutomationId,
        name: AutomationName,
        trigger: Trigger,
        action: AutomationAction,
        created_by: DeviceId,
        now: SystemTime,
    ) -> Self {
        Self {
            id,
            name,
            trigger,
            action,
            enabled: true,
            created_by,
            created_at: now,
            last_fired_at: None,
        }
    }

    /// Rehydrate a stored row. The repository is the only caller.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        id: AutomationId,
        name: AutomationName,
        trigger: Trigger,
        action: AutomationAction,
        enabled: bool,
        created_by: DeviceId,
        created_at: SystemTime,
        last_fired_at: Option<SystemTime>,
    ) -> Self {
        Self {
            id,
            name,
            trigger,
            action,
            enabled,
            created_by,
            created_at,
            last_fired_at,
        }
    }

    pub fn id(&self) -> &AutomationId {
        &self.id
    }
    pub fn name(&self) -> &AutomationName {
        &self.name
    }
    pub fn trigger(&self) -> &Trigger {
        &self.trigger
    }
    pub fn action(&self) -> &AutomationAction {
        &self.action
    }
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
    pub fn created_by(&self) -> &DeviceId {
        &self.created_by
    }
    pub fn created_at(&self) -> SystemTime {
        self.created_at
    }
    pub fn last_fired_at(&self) -> Option<SystemTime> {
        self.last_fired_at
    }

    /// Enable or disable. The single mutation an owner can make to an
    /// automation's behaviour without recreating it.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Whether this automation may fire at `now`, ignoring its trigger.
    ///
    /// Two guards, and both are about the *automation*, not the trigger:
    /// disabled means never, and a refire inside [`MIN_REFIRE_INTERVAL`] means
    /// not yet. A trigger that goes true twice in a second — a flapping
    /// presence sensor is the ordinary case — must not turn into two tool
    /// calls.
    pub fn may_fire_at(&self, now: SystemTime) -> bool {
        if !self.enabled {
            return false;
        }
        match self.last_fired_at {
            Some(last) => now
                .duration_since(last)
                .is_ok_and(|since| since >= MIN_REFIRE_INTERVAL),
            None => true,
        }
    }

    /// Record that it fired. Only the runner calls this, and only after the
    /// policy decision has been made and persisted.
    pub fn mark_fired(&mut self, now: SystemTime) {
        self.last_fired_at = Some(now);
    }
}

/// What happened on one firing — the execution history FR-17 asks for.
///
/// The policy decision is recorded whatever it was: a *denial* is the most
/// important row in this table, because "the automation ran and nothing
/// happened" and "the automation was refused" look identical from the sofa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationExecution {
    pub automation_id: AutomationId,
    pub occurred_at: SystemTime,
    pub outcome: ExecutionOutcome,
}

/// The result of one firing, in the vocabulary the history renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionOutcome {
    /// Policy said `Auto` and the tool ran.
    Executed,
    /// Policy said `NeedsApproval`. An automation firing at 6am has nobody to
    /// ask, so this is a refusal with a reason, not a pending prompt — it is
    /// recorded and visible rather than silently queued forever.
    NeedsApproval { exact_effect: String },
    /// Policy rejected it — including the revoked-creator case.
    Denied { reason: String },
    /// Policy allowed it and the tool itself failed.
    Failed { reason: String },
}

impl ExecutionOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Executed => "executed",
            Self::NeedsApproval { .. } => "needs_approval",
            Self::Denied { .. } => "denied",
            Self::Failed { .. } => "failed",
        }
    }

    /// Whether anything actually happened in the world.
    pub fn took_effect(&self) -> bool {
        matches!(self, Self::Executed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> DeviceId {
        "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("device id")
    }

    fn automation(trigger: Trigger, now: SystemTime) -> Automation {
        Automation::create(
            "01ARZ3NDEKTSV4RRFFQ69G5FB1".parse().expect("id"),
            AutomationName::new("evening lights").expect("name"),
            trigger,
            AutomationAction {
                tool_id: ToolId::home_set_light(),
                arguments: CanonicalValue::Null,
            },
            device(),
            now,
        )
    }

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// The property the whole module is shaped around: an automation stores
    /// *who*, never *what they were allowed*. If this ever gains a scopes
    /// field, a revoked device's automations keep working.
    #[test]
    fn an_automation_stores_its_creator_but_never_their_authority() {
        let a = automation(
            Trigger::DailyAt {
                minutes_since_midnight: 420,
            },
            t(0),
        );
        assert_eq!(a.created_by(), &device());
        // Nothing on this type can answer "what was allowed at creation" —
        // asserted by construction: `Automation` has no such accessor, so the
        // runner is forced to resolve authority fresh.
    }

    #[test]
    fn a_daily_trigger_fires_once_inside_the_window_that_contains_it() {
        let trigger = Trigger::DailyAt {
            minutes_since_midnight: 420, // 07:00
        };
        assert!(trigger.fires_in_window(419, 420), "the minute it arrives");
        assert!(
            trigger.fires_in_window(415, 425),
            "a wider sweep still sees it"
        );
        assert!(!trigger.fires_in_window(420, 425), "already fired at 420");
        assert!(!trigger.fires_in_window(300, 400), "not yet");
        assert!(!trigger.fires_in_window(421, 430), "missed, not re-fired");
    }

    /// A scheduler sweep that crosses midnight is two ranges, not one. Without
    /// this, every automation between the last evening tick and the first
    /// morning one would be skipped every single night.
    #[test]
    fn a_window_that_crosses_midnight_still_fires() {
        let just_after_midnight = Trigger::DailyAt {
            minutes_since_midnight: 5,
        };
        assert!(just_after_midnight.fires_in_window(1435, 10));
        let late_evening = Trigger::DailyAt {
            minutes_since_midnight: 1439,
        };
        assert!(late_evening.fires_in_window(1435, 10));
        // And something in the middle of the day is not swept up by it.
        let midday = Trigger::DailyAt {
            minutes_since_midnight: 720,
        };
        assert!(!midday.fires_in_window(1435, 10));
    }

    #[test]
    fn a_state_trigger_never_fires_on_the_clock() {
        let trigger = Trigger::HomeAssistantState {
            entity_id: "person.owner".into(),
            state: "home".into(),
        };
        assert!(!trigger.fires_in_window(0, 1440));
    }

    #[test]
    fn a_disabled_automation_never_fires() {
        let mut a = automation(
            Trigger::DailyAt {
                minutes_since_midnight: 420,
            },
            t(0),
        );
        assert!(a.may_fire_at(t(100)));
        a.set_enabled(false);
        assert!(!a.may_fire_at(t(100)));
        a.set_enabled(true);
        assert!(a.may_fire_at(t(100)));
    }

    /// A flapping presence sensor is the ordinary case, not the exotic one.
    #[test]
    fn a_trigger_that_goes_true_twice_in_a_second_fires_once() {
        let mut a = automation(
            Trigger::HomeAssistantState {
                entity_id: "person.owner".into(),
                state: "home".into(),
            },
            t(0),
        );
        assert!(a.may_fire_at(t(100)));
        a.mark_fired(t(100));

        assert!(
            !a.may_fire_at(t(101)),
            "a second later is not a second firing"
        );
        assert!(!a.may_fire_at(t(100 + MIN_REFIRE_INTERVAL.as_secs() - 1)));
        assert!(a.may_fire_at(t(100 + MIN_REFIRE_INTERVAL.as_secs())));
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_unlock_a_refire() {
        let mut a = automation(
            Trigger::DailyAt {
                minutes_since_midnight: 420,
            },
            t(1_000),
        );
        a.mark_fired(t(1_000));
        // `duration_since` errs when now precedes last; that must read as
        // "not yet", never as "no constraint".
        assert!(!a.may_fire_at(t(900)));
    }

    #[test]
    fn a_denial_is_recorded_as_distinctly_as_a_success() {
        assert_eq!(ExecutionOutcome::Executed.as_str(), "executed");
        assert!(ExecutionOutcome::Executed.took_effect());
        for outcome in [
            ExecutionOutcome::Denied {
                reason: "missing scope".into(),
            },
            ExecutionOutcome::NeedsApproval {
                exact_effect: "home.control {}".into(),
            },
            ExecutionOutcome::Failed {
                reason: "unreachable".into(),
            },
        ] {
            assert!(
                !outcome.took_effect(),
                "{} must not read as having happened",
                outcome.as_str()
            );
        }
    }

    #[test]
    fn names_are_bounded_and_stripped_of_control_characters() {
        assert_eq!(
            AutomationName::new("  evening\tlights  ")
                .expect("name")
                .as_str(),
            "eveninglights"
        );
        assert!(AutomationName::new("").is_err());
        assert!(AutomationName::new("   ").is_err());
        assert!(AutomationName::new(&"x".repeat(MAX_NAME_BYTES + 1)).is_err());
    }
}
