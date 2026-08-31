use super::shared::RepositoryError;
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::{DeviceId, TimerId};
use jarvis_domain::timers::{Timer, TimerState};

/// A persisted domain event bound for the transactional outbox (docs/05 §3,
/// skill `sqlx-data` §5) — written in the SAME transaction as the state change
/// it describes, then published to the WS hub by the dispatcher.
///
/// The payload is carried as **already-serialized JSON text**, for the same
/// reason [`AuditEvent`] carries its payload that way: the *wire* shape belongs
/// to `jarvis-contracts`, which neither this crate nor `jarvis-infra` may depend
/// on (invariant 3, enforced by arch-test). The host encodes; the store writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainEventRecord {
    /// Dotted envelope discriminator, e.g. `timer.fired`.
    pub event_type: String,
    /// The event payload MINUS the `type` discriminator (the envelope carries
    /// it) — matching the run/approval outbox convention.
    pub payload_json: String,
}

/// Timer/alarm/reminder persistence (FR-33, ADR-023, invariant 6). Timers must
/// survive a restart (NFR-05): one that came due while the daemon was down is
/// still in this store, still armed, and the sweep announces it as missed rather
/// than swallowing it.
///
/// Two properties belong to the contract rather than the implementation:
///
/// * **Every write co-transacts its audit row** (invariant 6). A timer that
///   cannot be audited is not stored, and a fire that cannot be audited did not
///   happen.
/// * **State changes are compare-and-set.** [`Self::apply`] moves a timer only
///   if it is still in `expected`; a lost race returns `Ok(false)`, never an
///   error and never a second write. That is what makes "a timer rings exactly
///   once" hold when the scheduler wakeup and the restart sweep overlap, or when
///   a human dismisses a timer in the instant it fires.
#[async_trait::async_trait]
pub trait TimerStore: Send + Sync {
    /// Persist a newly scheduled timer and its audit event atomically. A
    /// repeated [`TimerId`] is a [`RepositoryError::Conflict`].
    async fn create(&self, timer: &Timer, audit: &AuditEvent) -> Result<(), RepositoryError>;

    /// One timer by id. Unknown => `Ok(None)`.
    async fn get(&self, id: &TimerId) -> Result<Option<Timer>, RepositoryError>;

    /// Every timer that is not terminal — armed *or* ringing-unanswered —
    /// earliest fire time first. This is both the scheduler's worklist and the
    /// restart sweep's input, so the two can never disagree about what is
    /// outstanding.
    async fn list_live(&self) -> Result<Vec<Timer>, RepositoryError>;

    /// Compare-and-set `next`'s row from `expected` to `next.state()`, writing
    /// `audit` and (when given) `event` in the SAME transaction.
    ///
    /// `Ok(true)` = this caller made the change. `Ok(false)` = the row had
    /// already moved on, and **nothing was written** — no audit row, no event.
    async fn apply(
        &self,
        next: &Timer,
        expected: TimerState,
        audit: &AuditEvent,
        event: Option<&DomainEventRecord>,
    ) -> Result<bool, RepositoryError>;
}

/// Encodes a fired timer into its persisted wire event (FR-33). Implemented by
/// the host, which owns `jarvis-contracts`; named here because the timer use
/// case is what needs it. Kept to the one event this feature *produces* — a
/// module that emits an event it does not own would be contract drift waiting to
/// happen.
pub trait TimerEventEncoder: Send + Sync {
    /// The `timer.fired` outbox record for a timer that just went off.
    fn fired(&self, timer: &Timer, missed: bool) -> DomainEventRecord;
}

/// Why an audible alert could not be played. Deliberately content-free: no
/// device name and no player stderr reaches this type (it is logged and, in
/// `Failed`, already reduced to a short diagnostic).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AlertError {
    /// No audio path is configured or the player is missing. A normal state on a
    /// headless box — the timer still fires, it just does so silently.
    #[error("no audible alert path is available")]
    Unavailable,
    #[error("the alert was cancelled")]
    Cancelled,
    #[error("the alert failed: {0}")]
    Failed(String),
}

/// The **audible** half of a timer going off (ADR-023): a short tone on a
/// playback path that is *independent of the TTS pipeline*, so an alarm sounds
/// even when voice services are down or absent entirely.
///
/// This is deliberately not "speak this text": speaking is [`Announcer`], and
/// the two are separate ports precisely so one can be missing while the other
/// works. A failed alert never fails the fire — the timer is still marked fired,
/// still carded, and still audited.
#[async_trait::async_trait]
pub trait AlertPlayer: Send + Sync {
    /// Sound the alert for `timer`, in the room it was set in (F8.5).
    ///
    /// The whole timer rather than just its room, because an implementation
    /// that routes has to tell the room *which* timer is going off, and
    /// threading that through a side channel would make the port lie about
    /// what it needs. `timer.origin_device()` is `None` when it was set from
    /// the shell or by an automation — then the implementation falls back to
    /// whatever it considers "somewhere sensible", which for the daemon is its
    /// own host. A room that is no longer reachable (a revoked or unplugged
    /// node) is the same case: the timer still rings, it just does not ring
    /// there (ADR-023 — an alarm must sound).
    async fn play(
        &self,
        timer: &jarvis_domain::timers::Timer,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), AlertError>;
}

/// What happened to a spoken announcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnouncementOutcome {
    Spoken,
    /// No voice pipeline (the default before M5) — reported honestly rather than
    /// pretended, because it decides whether the HUD card is the *only* notice
    /// the human gets.
    Unavailable,
}

/// The **spoken** half of a timer going off ("reminder — call Mom").
///
/// **M5 boundary.** Voice is M5 (docs/08); until then the wired implementation
/// is `jarvis_adapters::timer_alert::SilentAnnouncer`, which always answers
/// [`AnnouncementOutcome::Unavailable`]. M5 replaces that one binding with the
/// Wyoming TTS adapter and nothing else in this feature changes — that is the
/// entire seam. The audible alert above is NOT part of that seam and must keep
/// working with no voice pipeline at all.
#[async_trait::async_trait]
pub trait Announcer: Send + Sync {
    /// Speak `text`, in `target`'s room when there is one (F8.5). Same
    /// fallback rule as [`AlertPlayer::play`].
    async fn announce(
        &self,
        text: &str,
        target: Option<&DeviceId>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> AnnouncementOutcome;
}
