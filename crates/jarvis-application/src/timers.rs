//! Timer use cases and scheduling decisions (FR-33, docs/02 §11e, ADR-023).
//!
//! This module is the whole of "what should happen to timers, and when". It is
//! **entirely deterministic**: it holds no model provider, no policy engine,
//! and no network — setting, querying, cancelling, snoozing,
//! dismissing and firing a timer are arithmetic over a stored row plus two
//! output ports (a tone, a spoken line). That is ADR-023's whole point: the most
//! used assistant feature must work offline, in degraded mode, and with the
//! model quota exhausted. A test asserts this structurally rather than trusting
//! the comment ([`tests::the_timer_path_never_reaches_a_model`]).
//!
//! **Boundary with FR-17 automations** (ADR-023): "make a noise at T" is a
//! timer and lives here. Anything that needs policy re-evaluation or model
//! reasoning *at fire time* is an automation and does not — a timer's fire path
//! deliberately has nowhere to put a tool call.
//!
//! Time enters through the injected [`Clock`]; the domain itself never reads a
//! clock, so every decision below is reproducible at an instant of the test's
//! choosing.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::TimerId;
use jarvis_domain::timers::{
    MISSED_GRACE, Timer, TimerAction, TimerKind, TimerName, TimerScheduleError, TimerSnoozeError,
    TimerTransitionError,
};
use tokio_util::sync::CancellationToken;

use crate::orchestrator::Clock;
use crate::ports::{
    AlertPlayer, AnnouncementOutcome, Announcer, RepositoryError, TimerEventEncoder, TimerStore,
};

/// When a new timer should go off, as the caller expressed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerWhen {
    /// "…in ten minutes" — relative to the service's clock at set time.
    In(Duration),
    /// "…at seven" — an absolute instant the caller already resolved.
    At(SystemTime),
}

/// A request to schedule a timer. `name` is raw human text; the service
/// sanitizes it (or substitutes the kind's fallback label) so no caller has to
/// remember to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTimer {
    pub name: Option<String>,
    pub kind: TimerKind,
    pub when: TimerWhen,
}

/// A timer that just went off, and how it went off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiredTimer {
    pub timer: Timer,
    /// It came due while nothing was running (ADR-023). The card and the spoken
    /// line say so — a missed alarm is never presented as a fresh one.
    pub missed: bool,
    /// The audible alert actually sounded. `false` means the box has no audio
    /// path; the fire still happened and is still recorded.
    pub alerted: bool,
    /// A voice pipeline spoke the announcement. `false` before M5 — the HUD card
    /// is then the only visual notice, which is why it is not optional.
    pub announced: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimerServiceError {
    #[error("no such timer")]
    NotFound,
    #[error(transparent)]
    Schedule(#[from] TimerScheduleError),
    #[error(transparent)]
    Transition(#[from] TimerTransitionError),
    #[error("a timer name must not be empty")]
    EmptyName,
    #[error("that timer changed while the request was in flight; try again")]
    RaceLost,
    #[error("timer storage failure: {0}")]
    Storage(String),
    #[error("the request was cancelled")]
    Cancelled,
}

impl From<RepositoryError> for TimerServiceError {
    fn from(e: RepositoryError) -> Self {
        Self::Storage(e.to_string())
    }
}

impl From<TimerSnoozeError> for TimerServiceError {
    fn from(e: TimerSnoozeError) -> Self {
        match e {
            TimerSnoozeError::Transition(t) => Self::Transition(t),
            TimerSnoozeError::Schedule(s) => Self::Schedule(s),
        }
    }
}

// ---------------------------------------------------------------------------
// Scheduling decisions — pure functions over the live set
// ---------------------------------------------------------------------------

/// Everything due at `now`, earliest first. The scheduler fires exactly this
/// list and nothing else.
pub fn due_at(timers: &[Timer], now: SystemTime) -> Vec<&Timer> {
    let mut due: Vec<&Timer> = timers.iter().filter(|t| t.is_due_at(now)).collect();
    due.sort_by_key(|t| t.fire_at());
    due
}

/// How long the scheduler should sleep before it next has work.
///
/// `Some(ZERO)` means "something is due right now" (including anything missed
/// while the process was down — a restart sweep is just this function answering
/// zero). `None` means nothing is armed at all: the scheduler parks on its
/// wakeup signal and burns nothing (docs/09 §5 — event-driven idle, never a
/// polling loop).
pub fn next_wakeup(timers: &[Timer], now: SystemTime) -> Option<Duration> {
    timers.iter().filter_map(|t| t.remaining_at(now)).min()
}

// ---------------------------------------------------------------------------
// The use cases
// ---------------------------------------------------------------------------

/// Set / query / cancel / snooze / dismiss / fire.
///
/// Everything is cancellable (invariant 4): each method takes a
/// [`CancellationToken`] and checks it before doing work, and the alert and
/// announcement — the two operations that can outlive a user's patience — carry
/// it into the adapter.
pub struct TimerService {
    store: Arc<dyn TimerStore>,
    alert: Arc<dyn AlertPlayer>,
    announcer: Arc<dyn Announcer>,
    encoder: Arc<dyn TimerEventEncoder>,
    clock: Arc<dyn Clock>,
}

impl TimerService {
    pub fn new(
        store: Arc<dyn TimerStore>,
        alert: Arc<dyn AlertPlayer>,
        announcer: Arc<dyn Announcer>,
        encoder: Arc<dyn TimerEventEncoder>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            alert,
            announcer,
            encoder,
            clock,
        }
    }

    pub fn now(&self) -> SystemTime {
        self.clock.now()
    }

    /// Schedule a timer. The id is minted by the caller (the host owns
    /// randomness); `actor` is the audit subject (`device:<ulid>`).
    pub async fn set(
        &self,
        id: TimerId,
        request: NewTimer,
        actor: &str,
        cancel: &CancellationToken,
    ) -> Result<Timer, TimerServiceError> {
        check(cancel)?;
        let now = self.clock.now();
        let fire_at = match request.when {
            TimerWhen::In(d) => now + d,
            TimerWhen::At(t) => t,
        };
        let name = match request.name.as_deref() {
            Some(raw) => TimerName::new(raw).map_err(|_| TimerServiceError::EmptyName)?,
            None => TimerName::fallback_for(&request.kind),
        };
        let timer = Timer::schedule(id, name, request.kind, fire_at, now)?;
        // Audit and row commit together (invariant 6): a timer that cannot be
        // recorded is not set at all.
        self.store
            .create(&timer, &audit(&timer, "timer.set", actor, now, false))
            .await?;
        Ok(timer)
    }

    /// Every outstanding timer — armed or ringing — earliest first. The list a
    /// human means by "what timers do I have?".
    pub async fn list(&self, cancel: &CancellationToken) -> Result<Vec<Timer>, TimerServiceError> {
        check(cancel)?;
        Ok(self.store.list_live().await?)
    }

    pub async fn get(
        &self,
        id: &TimerId,
        cancel: &CancellationToken,
    ) -> Result<Timer, TimerServiceError> {
        check(cancel)?;
        self.store.get(id).await?.ok_or(TimerServiceError::NotFound)
    }

    /// Apply a human verb — cancel, dismiss, or snooze — to one timer.
    ///
    /// `snooze_by` is honoured only by [`TimerAction::Snooze`]; `None` takes the
    /// bedside-clock default. [`TimerAction::Fire`] is rejected here: firing is
    /// something the clock does, never something a request asks for.
    pub async fn act(
        &self,
        id: &TimerId,
        action: TimerAction,
        snooze_by: Option<Duration>,
        actor: &str,
        cancel: &CancellationToken,
    ) -> Result<Timer, TimerServiceError> {
        check(cancel)?;
        let now = self.clock.now();
        let current = self
            .store
            .get(id)
            .await?
            .ok_or(TimerServiceError::NotFound)?;
        let next = match action {
            TimerAction::Cancel => current.cancel()?,
            TimerAction::Dismiss => current.dismiss()?,
            TimerAction::Snooze => current.snooze(
                now,
                snooze_by.unwrap_or(jarvis_domain::timers::DEFAULT_SNOOZE),
            )?,
            // Unreachable from any wire verb (`TimerAction::parse` has no
            // spelling for it); stated explicitly rather than with a `_` arm so
            // a new verb forces a decision here.
            TimerAction::Fire => return Err(TimerTransitionError::AlreadyFired.into()),
        };
        let audit = audit(
            &next,
            &format!("timer.{}", action.as_str()),
            actor,
            now,
            false,
        );
        // Compare-and-set: if the timer moved under us (it fired in the same
        // instant, or a second client answered first) nothing is written and the
        // caller is told to re-read rather than being handed a stale success.
        if !self
            .store
            .apply(&next, current.state(), &audit, None)
            .await?
        {
            return Err(TimerServiceError::RaceLost);
        }
        Ok(next)
    }

    /// Fire everything due at the clock's current reading, announcing each one.
    ///
    /// This is the scheduler's step **and** the restart sweep — deliberately the
    /// same code path, because a timer that came due while the daemon was down
    /// is not a special case, it is simply a timer that is very late. The only
    /// difference is the `missed` flag, which the human is told about.
    ///
    /// A timer that loses the compare-and-set (already fired by a concurrent
    /// sweep) is skipped silently: it rang once, which is the guarantee.
    pub async fn fire_due(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<FiredTimer>, TimerServiceError> {
        check(cancel)?;
        let now = self.clock.now();
        let live = self.store.list_live().await?;
        let mut fired = Vec::new();
        for timer in due_at(&live, now) {
            if cancel.is_cancelled() {
                break;
            }
            let missed = timer.is_missed_at(now, MISSED_GRACE);
            let next = match timer.fire() {
                Ok(next) => next,
                // Not firable (already ringing) — the table said so; leave it be.
                Err(_) => continue,
            };
            let event = self.encoder.fired(&next, missed);
            let audit = audit(&next, "timer.fired", "system", now, missed);
            // State change + `timer.fired` outbox row + audit row, one
            // transaction (invariant 6 + transactional outbox). Only after it
            // commits does anything become audible: a fire we could not record
            // must not be one the human hears and we cannot account for.
            if !self
                .store
                .apply(&next, timer.state(), &audit, Some(&event))
                .await?
            {
                // Another sweep got there first — it rang once, which is the
                // guarantee. Nothing was written; move on.
                continue;
            }
            fired.push(self.announce(next, missed, cancel).await);
        }
        Ok(fired)
    }

    /// How long until the next fire. `None` = nothing armed; the caller parks.
    pub async fn next_wakeup(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Option<Duration>, TimerServiceError> {
        check(cancel)?;
        let live = self.store.list_live().await?;
        Ok(next_wakeup(&live, self.clock.now()))
    }

    /// Make the noise, then (if anything can) speak the line.
    ///
    /// Order matters and is the ADR-023 requirement: the **tone is played first
    /// and independently**, so an alarm is audible on a box with no voice
    /// pipeline at all. Neither failing turns a fired timer into an unfired one
    /// — the durable state is already committed.
    async fn announce(&self, timer: Timer, missed: bool, cancel: &CancellationToken) -> FiredTimer {
        // A silent box is reported, never fatal. This layer has no `tracing`
        // dependency by design (it stays pure — arch-test), so the diagnostic is
        // carried out on [`FiredTimer::alerted`] and logged by the host.
        let alerted = self.alert.play(cancel.clone()).await.is_ok();
        let line = if missed {
            timer.missed_announcement()
        } else {
            timer.announcement()
        };
        let announced = matches!(
            self.announcer.announce(&line, cancel.clone()).await,
            AnnouncementOutcome::Spoken
        );
        FiredTimer {
            timer,
            missed,
            alerted,
            announced,
        }
    }
}

fn check(cancel: &CancellationToken) -> Result<(), TimerServiceError> {
    if cancel.is_cancelled() {
        return Err(TimerServiceError::Cancelled);
    }
    Ok(())
}

/// The audit row for a timer lifecycle event (invariant 6).
///
/// The payload carries **only closed-vocabulary values** — kind, resulting
/// state, and a boolean — never the timer's name or a reminder's note. Two
/// reasons: the audit chain is hashed from this JSON, so hand-assembling it with
/// free text would be an injection hazard; and a reminder note is personal
/// content that has no business being duplicated into the security log
/// (invariant 5's spirit). The row is joinable to the timer by its target id.
fn audit(
    timer: &Timer,
    event_type: &str,
    actor: &str,
    now: SystemTime,
    missed: bool,
) -> AuditEvent {
    AuditEvent {
        occurred_at: now,
        actor: actor.to_owned(),
        event_type: event_type.to_owned(),
        target: format!("timer:{}", timer.id()),
        correlation_id: None,
        payload_json: format!(
            r#"{{"kind":"{}","state":"{}","missed":{}}}"#,
            timer.kind().as_str(),
            timer.state().as_str(),
            missed
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{AlertError, DomainEventRecord};
    use crate::testing::ManualClock;
    use jarvis_domain::timers::TimerState;
    use std::sync::Mutex;

    const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const ID2: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
    const T0: u64 = 1_000_000;

    fn id(raw: &str) -> TimerId {
        raw.parse().expect("valid test ulid")
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    // ---- doubles -----------------------------------------------------------

    /// An in-memory [`TimerStore`] with the same compare-and-set semantics as
    /// the Postgres one, so a race here means a race there.
    #[derive(Default)]
    struct FakeStore {
        rows: Mutex<Vec<Timer>>,
        audits: Mutex<Vec<AuditEvent>>,
        events: Mutex<Vec<DomainEventRecord>>,
    }

    impl FakeStore {
        fn seeded(timers: Vec<Timer>) -> Arc<Self> {
            Arc::new(Self {
                rows: Mutex::new(timers),
                ..Self::default()
            })
        }
        fn audit_types(&self) -> Vec<String> {
            self.audits
                .lock()
                .unwrap()
                .iter()
                .map(|a| a.event_type.clone())
                .collect()
        }
        fn event_types(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|e| e.event_type.clone())
                .collect()
        }
        fn state_of(&self, id: &TimerId) -> Option<TimerState> {
            self.rows
                .lock()
                .unwrap()
                .iter()
                .find(|t| t.id() == id)
                .map(|t| t.state())
        }
    }

    #[async_trait::async_trait]
    impl TimerStore for FakeStore {
        async fn create(&self, timer: &Timer, audit: &AuditEvent) -> Result<(), RepositoryError> {
            let mut rows = self.rows.lock().unwrap();
            if rows.iter().any(|t| t.id() == timer.id()) {
                return Err(RepositoryError::Conflict("duplicate timer".to_owned()));
            }
            rows.push(timer.clone());
            self.audits.lock().unwrap().push(audit.clone());
            Ok(())
        }

        async fn get(&self, id: &TimerId) -> Result<Option<Timer>, RepositoryError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .find(|t| t.id() == id)
                .cloned())
        }

        async fn list_live(&self) -> Result<Vec<Timer>, RepositoryError> {
            let mut live: Vec<Timer> = self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.state().is_live())
                .cloned()
                .collect();
            live.sort_by_key(|t| t.fire_at());
            Ok(live)
        }

        async fn apply(
            &self,
            next: &Timer,
            expected: TimerState,
            audit: &AuditEvent,
            event: Option<&DomainEventRecord>,
        ) -> Result<bool, RepositoryError> {
            let mut rows = self.rows.lock().unwrap();
            let Some(row) = rows.iter_mut().find(|t| t.id() == next.id()) else {
                return Ok(false);
            };
            if row.state() != expected {
                // Lost the CAS: nothing is written at all.
                return Ok(false);
            }
            *row = next.clone();
            self.audits.lock().unwrap().push(audit.clone());
            if let Some(event) = event {
                self.events.lock().unwrap().push(event.clone());
            }
            Ok(true)
        }
    }

    /// Records every tone played, and can be made to fail like a box with no
    /// audio device.
    #[derive(Default)]
    struct FakeAlert {
        plays: Mutex<u32>,
        unavailable: bool,
    }

    impl FakeAlert {
        fn silent_box() -> Arc<Self> {
            Arc::new(Self {
                unavailable: true,
                ..Self::default()
            })
        }
        fn count(&self) -> u32 {
            *self.plays.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl AlertPlayer for FakeAlert {
        async fn play(&self, _cancel: CancellationToken) -> Result<(), AlertError> {
            *self.plays.lock().unwrap() += 1;
            if self.unavailable {
                return Err(AlertError::Unavailable);
            }
            Ok(())
        }
    }

    /// Stands in for both the pre-M5 silent announcer and an M5 voice pipeline.
    struct FakeAnnouncer {
        spoken: Mutex<Vec<String>>,
        available: bool,
    }

    impl FakeAnnouncer {
        fn voiceless() -> Arc<Self> {
            Arc::new(Self {
                spoken: Mutex::default(),
                available: false,
            })
        }
        fn voiced() -> Arc<Self> {
            Arc::new(Self {
                spoken: Mutex::default(),
                available: true,
            })
        }
        fn lines(&self) -> Vec<String> {
            self.spoken.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Announcer for FakeAnnouncer {
        async fn announce(&self, text: &str, _cancel: CancellationToken) -> AnnouncementOutcome {
            self.spoken.lock().unwrap().push(text.to_owned());
            if self.available {
                AnnouncementOutcome::Spoken
            } else {
                AnnouncementOutcome::Unavailable
            }
        }
    }

    struct FakeEncoder;

    impl TimerEventEncoder for FakeEncoder {
        fn fired(&self, timer: &Timer, missed: bool) -> DomainEventRecord {
            DomainEventRecord {
                event_type: "timer.fired".to_owned(),
                payload_json: format!(r#"{{"timerId":"{}","missed":{}}}"#, timer.id(), missed),
            }
        }
    }

    struct Harness {
        service: TimerService,
        store: Arc<FakeStore>,
        alert: Arc<FakeAlert>,
        announcer: Arc<FakeAnnouncer>,
        clock: Arc<ManualClock>,
    }

    impl Harness {
        fn with(timers: Vec<Timer>, alert: Arc<FakeAlert>, announcer: Arc<FakeAnnouncer>) -> Self {
            let store = FakeStore::seeded(timers);
            let clock = Arc::new(ManualClock::at_unix(T0));
            let service = TimerService::new(
                store.clone(),
                alert.clone(),
                announcer.clone(),
                Arc::new(FakeEncoder),
                clock.clone(),
            );
            Self {
                service,
                store,
                alert,
                announcer,
                clock,
            }
        }

        fn empty() -> Self {
            Self::with(
                Vec::new(),
                Arc::new(FakeAlert::default()),
                FakeAnnouncer::voiceless(),
            )
        }
    }

    fn pending(raw_id: &str, name: &str, fire_at: u64, created: u64) -> Timer {
        Timer::schedule(
            id(raw_id),
            TimerName::new(name).unwrap(),
            TimerKind::Countdown {
                duration: Duration::from_secs(fire_at - created),
            },
            at(fire_at),
            at(created),
        )
        .expect("test timer is schedulable")
    }

    // ---- THE test: missed alarms survive a restart -------------------------

    #[tokio::test]
    async fn a_timer_that_came_due_while_the_daemon_was_down_is_announced_on_restart() {
        // The most valuable behaviour in the feature (ADR-023): jarvisd was
        // stopped before this timer's moment and started again long after. The
        // row is still Pending in Postgres. The restart sweep must fire it,
        // flag it missed, sound the alert, and record both the audit row and the
        // durable `timer.fired` event — never silently swallow it.
        let pasta = pending(ID, "pasta timer", T0 - 3_600, T0 - 4_200);
        let later = pending(ID2, "bread timer", T0 + 600, T0 - 60);
        let h = Harness::with(
            vec![pasta.clone(), later],
            Arc::new(FakeAlert::default()),
            FakeAnnouncer::voiced(),
        );

        let fired = h
            .service
            .fire_due(&CancellationToken::new())
            .await
            .expect("the sweep succeeds");

        assert_eq!(fired.len(), 1, "only the overdue timer fires");
        let f = &fired[0];
        assert_eq!(f.timer.id(), pasta.id());
        assert!(f.missed, "an hour late is missed, not freshly rung");
        assert!(f.alerted, "the alarm sounded");
        assert!(f.announced);
        assert_eq!(
            h.announcer.lines(),
            vec!["Missed while I was offline — pasta timer is up"],
            "the human is TOLD it was missed rather than left to infer it"
        );
        assert_eq!(h.store.state_of(pasta.id()), Some(TimerState::Fired));
        assert_eq!(h.store.event_types(), vec!["timer.fired"]);
        assert_eq!(h.store.audit_types(), vec!["timer.fired"]);
        // The not-yet-due timer is untouched and still armed.
        assert_eq!(h.store.state_of(&id(ID2)), Some(TimerState::Pending));
    }

    #[tokio::test]
    async fn a_missed_timer_fires_exactly_once_across_overlapping_sweeps() {
        // The restart sweep and the first scheduler wakeup can overlap. The
        // compare-and-set makes the second one a no-op: one tone, one event,
        // one audit row.
        let h = Harness::with(
            vec![pending(ID, "pasta timer", T0 - 3_600, T0 - 4_200)],
            Arc::new(FakeAlert::default()),
            FakeAnnouncer::voiceless(),
        );
        let cancel = CancellationToken::new();
        assert_eq!(h.service.fire_due(&cancel).await.unwrap().len(), 1);
        assert_eq!(
            h.service.fire_due(&cancel).await.unwrap().len(),
            0,
            "a fired timer is not due again"
        );
        assert_eq!(h.alert.count(), 1, "exactly one tone");
        assert_eq!(h.store.event_types().len(), 1);
    }

    #[tokio::test]
    async fn an_on_time_fire_is_not_reported_as_missed() {
        let h = Harness::with(
            vec![pending(ID, "pasta timer", T0, T0 - 600)],
            Arc::new(FakeAlert::default()),
            FakeAnnouncer::voiced(),
        );
        let fired = h.service.fire_due(&CancellationToken::new()).await.unwrap();
        assert_eq!(fired.len(), 1);
        assert!(!fired[0].missed);
        assert_eq!(h.announcer.lines(), vec!["pasta timer is up"]);
    }

    // ---- the alert is independent of the TTS pipeline ----------------------

    #[tokio::test]
    async fn an_alarm_sounds_even_with_no_voice_pipeline_at_all() {
        // ADR-023: "an alarm must sound even if voice services are down". The
        // announcer here is the pre-M5 silent one; the tone still plays and the
        // fire is still durable.
        let alert = Arc::new(FakeAlert::default());
        let h = Harness::with(
            vec![pending(ID, "pasta timer", T0, T0 - 600)],
            alert.clone(),
            FakeAnnouncer::voiceless(),
        );
        let fired = h.service.fire_due(&CancellationToken::new()).await.unwrap();
        assert_eq!(alert.count(), 1, "the tone does not depend on TTS");
        assert!(fired[0].alerted);
        assert!(!fired[0].announced, "honest about having no voice");
        assert_eq!(h.store.state_of(&id(ID)), Some(TimerState::Fired));
    }

    #[tokio::test]
    async fn a_box_with_no_audio_still_fires_records_and_cards_the_timer() {
        // The inverse: no audio device. The fire is not cancelled by it — the
        // durable state, the event and the audit row are all still written, so
        // the HUD card still appears.
        let alert = FakeAlert::silent_box();
        let h = Harness::with(
            vec![pending(ID, "pasta timer", T0, T0 - 600)],
            alert,
            FakeAnnouncer::voiceless(),
        );
        let fired = h.service.fire_due(&CancellationToken::new()).await.unwrap();
        assert_eq!(fired.len(), 1);
        assert!(!fired[0].alerted);
        assert_eq!(h.store.state_of(&id(ID)), Some(TimerState::Fired));
        assert_eq!(h.store.event_types(), vec!["timer.fired"]);
    }

    // ---- set / cancel / snooze / dismiss -----------------------------------

    #[tokio::test]
    async fn setting_a_relative_timer_uses_the_injected_clock() {
        let h = Harness::empty();
        let timer = h
            .service
            .set(
                id(ID),
                NewTimer {
                    name: Some("pasta timer".to_owned()),
                    kind: TimerKind::Countdown {
                        duration: Duration::from_secs(600),
                    },
                    when: TimerWhen::In(Duration::from_secs(600)),
                },
                "device:01ARZ3NDEKTSV4RRFFQ69G5FB2",
                &CancellationToken::new(),
            )
            .await
            .expect("set succeeds");
        assert_eq!(timer.fire_at(), at(T0 + 600));
        assert_eq!(timer.state(), TimerState::Pending);
        assert_eq!(h.store.audit_types(), vec!["timer.set"]);
        // Nothing is due yet, and the scheduler is told to sleep exactly 600s.
        assert_eq!(
            h.service
                .next_wakeup(&CancellationToken::new())
                .await
                .unwrap(),
            Some(Duration::from_secs(600))
        );
    }

    #[tokio::test]
    async fn an_unnamed_timer_is_still_enumerable() {
        let h = Harness::empty();
        let timer = h
            .service
            .set(
                id(ID),
                NewTimer {
                    name: None,
                    kind: TimerKind::Alarm,
                    when: TimerWhen::At(at(T0 + 3_600)),
                },
                "device:01ARZ3NDEKTSV4RRFFQ69G5FB2",
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(timer.name().as_str(), "Alarm");
        let listed = h.service.list(&CancellationToken::new()).await.unwrap();
        assert_eq!(listed.len(), 1);
    }

    #[tokio::test]
    async fn cancelling_before_it_rings_means_it_never_rings() {
        let h = Harness::with(
            vec![pending(ID, "pasta timer", T0 + 600, T0)],
            Arc::new(FakeAlert::default()),
            FakeAnnouncer::voiceless(),
        );
        let cancel = CancellationToken::new();
        let cancelled = h
            .service
            .act(&id(ID), TimerAction::Cancel, None, "device:x", &cancel)
            .await
            .unwrap();
        assert_eq!(cancelled.state(), TimerState::Cancelled);
        assert_eq!(h.store.audit_types(), vec!["timer.cancel"]);

        // Move past its would-be moment: it must not fire, and nothing is live.
        h.clock.advance(Duration::from_secs(1_200));
        assert!(h.service.fire_due(&cancel).await.unwrap().is_empty());
        assert_eq!(h.alert.count(), 0);
        assert!(h.service.list(&cancel).await.unwrap().is_empty());
        // A cancelled timer emits no `timer.fired` event, ever.
        assert!(h.store.event_types().is_empty());
    }

    #[tokio::test]
    async fn snoozing_rings_again_later_and_dismissing_ends_it() {
        let h = Harness::with(
            vec![pending(ID, "pasta timer", T0, T0 - 600)],
            Arc::new(FakeAlert::default()),
            FakeAnnouncer::voiceless(),
        );
        let cancel = CancellationToken::new();
        h.service.fire_due(&cancel).await.unwrap();

        // Human hits snooze 20 seconds into the ringing.
        h.clock.advance(Duration::from_secs(20));
        let snoozed = h
            .service
            .act(
                &id(ID),
                TimerAction::Snooze,
                Some(Duration::from_secs(300)),
                "device:x",
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(snoozed.state(), TimerState::Snoozed);
        assert_eq!(snoozed.fire_at(), at(T0 + 20 + 300));
        assert!(h.service.fire_due(&cancel).await.unwrap().is_empty());

        // It rings again at the new time…
        h.clock.advance(Duration::from_secs(300));
        let again = h.service.fire_due(&cancel).await.unwrap();
        assert_eq!(again.len(), 1);
        assert!(!again[0].missed, "a snooze arriving on time is not missed");
        assert_eq!(h.alert.count(), 2);

        // …and dismissing retires it for good.
        let dismissed = h
            .service
            .act(&id(ID), TimerAction::Dismiss, None, "device:x", &cancel)
            .await
            .unwrap();
        assert_eq!(dismissed.state(), TimerState::Dismissed);
        assert!(h.service.list(&cancel).await.unwrap().is_empty());
        assert_eq!(
            h.store.audit_types(),
            vec![
                "timer.fired",
                "timer.snooze",
                "timer.fired",
                "timer.dismiss"
            ],
            "every lifecycle step left an audit row"
        );
    }

    #[tokio::test]
    async fn a_request_that_lost_the_race_is_told_so_rather_than_given_a_stale_success() {
        // The human taps "cancel" in the same instant the timer fires. The CAS
        // fails, nothing is written, and the caller re-reads.
        let h = Harness::with(
            vec![pending(ID, "pasta timer", T0, T0 - 600)],
            Arc::new(FakeAlert::default()),
            FakeAnnouncer::voiceless(),
        );
        let cancel = CancellationToken::new();
        // Read the timer as Pending, then let it fire underneath.
        let current = h.service.get(&id(ID), &cancel).await.unwrap();
        assert_eq!(current.state(), TimerState::Pending);
        h.service.fire_due(&cancel).await.unwrap();
        // Now the cancel arrives: it is refused (a ringing timer is dismissed).
        assert_eq!(
            h.service
                .act(&id(ID), TimerAction::Cancel, None, "device:x", &cancel)
                .await,
            Err(TimerServiceError::Transition(
                TimerTransitionError::AlreadyFired
            ))
        );
        assert_eq!(h.store.state_of(&id(ID)), Some(TimerState::Fired));
    }

    #[tokio::test]
    async fn unknown_timers_and_empty_names_are_clean_errors() {
        let h = Harness::empty();
        let cancel = CancellationToken::new();
        assert_eq!(
            h.service.get(&id(ID), &cancel).await,
            Err(TimerServiceError::NotFound)
        );
        assert_eq!(
            h.service
                .act(&id(ID), TimerAction::Dismiss, None, "device:x", &cancel)
                .await,
            Err(TimerServiceError::NotFound)
        );
        assert_eq!(
            h.service
                .set(
                    id(ID),
                    NewTimer {
                        name: Some("  \u{200b} ".to_owned()),
                        kind: TimerKind::Alarm,
                        when: TimerWhen::In(Duration::from_secs(60)),
                    },
                    "device:x",
                    &cancel,
                )
                .await,
            Err(TimerServiceError::EmptyName)
        );
    }

    // ---- cancellation (invariant 4) ---------------------------------------

    #[tokio::test]
    async fn every_timer_operation_honours_cancellation() {
        let h = Harness::with(
            vec![pending(ID, "pasta timer", T0, T0 - 600)],
            Arc::new(FakeAlert::default()),
            FakeAnnouncer::voiceless(),
        );
        let cancel = CancellationToken::new();
        cancel.cancel();

        assert_eq!(
            h.service.list(&cancel).await,
            Err(TimerServiceError::Cancelled)
        );
        assert_eq!(
            h.service.get(&id(ID), &cancel).await,
            Err(TimerServiceError::Cancelled)
        );
        assert_eq!(
            h.service
                .act(&id(ID), TimerAction::Dismiss, None, "device:x", &cancel)
                .await,
            Err(TimerServiceError::Cancelled)
        );
        assert_eq!(
            h.service.next_wakeup(&cancel).await,
            Err(TimerServiceError::Cancelled)
        );
        assert_eq!(
            h.service.fire_due(&cancel).await,
            Err(TimerServiceError::Cancelled)
        );
        // A cancelled shutdown must not have rung anything on its way out.
        assert_eq!(h.alert.count(), 0);
        assert_eq!(h.store.state_of(&id(ID)), Some(TimerState::Pending));
    }

    // ---- scheduling decisions ---------------------------------------------

    #[test]
    fn next_wakeup_is_the_earliest_armed_timer_and_zero_when_overdue() {
        let now = at(T0);
        let timers = vec![
            pending(ID, "later", T0 + 900, T0),
            pending(ID2, "sooner", T0 + 60, T0),
        ];
        assert_eq!(next_wakeup(&timers, now), Some(Duration::from_secs(60)));
        assert!(due_at(&timers, now).is_empty());

        // Overdue ⇒ zero: the scheduler does not sleep on a timer that is late.
        let overdue = vec![pending(ID, "missed", T0 - 100, T0 - 200)];
        assert_eq!(next_wakeup(&overdue, now), Some(Duration::ZERO));
        assert_eq!(due_at(&overdue, now).len(), 1);

        // Nothing armed ⇒ no wakeup at all: the scheduler parks on its signal
        // instead of spinning (docs/09 §5 event-driven idle).
        assert_eq!(next_wakeup(&[], now), None);
        let ringing = vec![pending(ID, "ringing", T0 - 100, T0 - 200).fire().unwrap()];
        assert_eq!(
            next_wakeup(&ringing, now),
            None,
            "a timer waiting to be dismissed is not a wakeup"
        );
    }

    #[test]
    fn due_timers_come_out_oldest_first() {
        let now = at(T0);
        let timers = vec![
            pending(ID, "second", T0 - 10, T0 - 100),
            pending(ID2, "first", T0 - 50, T0 - 100),
        ];
        let due = due_at(&timers, now);
        assert_eq!(
            due.iter().map(|t| t.name().as_str()).collect::<Vec<_>>(),
            vec!["first", "second"],
            "the oldest missed alarm is announced first"
        );
    }

    // ---- the deterministic-grammar property --------------------------------

    #[test]
    fn the_timer_path_never_reaches_a_model() {
        // ADR-023's load-bearing property: timers are "entirely in the
        // deterministic grammar — zero LLM, works offline and in degraded
        // mode". A behavioural test cannot prove a negative about a dependency
        // that was never injected, so assert it structurally, the same way
        // docs/12 §9 asserts "no free-form model HTML": neither the timer use
        // cases nor the timer domain module may so much as name the model or
        // policy plane. A future edit that wires one in fails here.
        for (label, source) in [
            ("application/timers.rs", include_str!("timers.rs")),
            (
                "domain/timers.rs",
                include_str!("../../jarvis-domain/src/timers.rs"),
            ),
        ] {
            // Skip this test's own body, which necessarily mentions the words.
            let code = source
                .split("mod tests {")
                .next()
                .expect("every module has a pre-test section");
            for forbidden in [
                "ModelProvider",
                "model::",
                "crate::policy",
                "PolicyContext",
                "ExecutionGrant",
                "ToolRegistry",
            ] {
                assert!(
                    !code.contains(forbidden),
                    "{label} must stay in the deterministic grammar, but names {forbidden}"
                );
            }
        }
    }
}
