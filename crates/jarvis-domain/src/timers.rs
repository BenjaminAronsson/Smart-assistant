//! Timers, alarms and one-shot reminders (FR-33, docs/02 §11e, ADR-023).
//!
//! The cheapest machinery that covers the most-used assistant category. Every
//! rule that decides *what happens and when* lives here as pure functions:
//!
//! * **Time is a parameter, never a reading.** Nothing in this module calls
//!   `SystemTime::now()`. "Is this due?", "how long is left?", "was this missed
//!   while we were down?" all take the instant as an argument, so the scheduler,
//!   the restart sweep, and the tests observe identical logic at instants of
//!   their choosing — a timer test that has to sleep is a timer test that lies.
//! * **The state table is the whole lifecycle.** [`TimerState::apply`] is the
//!   single place a timer may change state; its `match` is exhaustive over
//!   (state, action) with no `_` arm, so a new state or verb cannot be added
//!   without deciding every pairing (the same discipline as `RunState`).
//! * **A timer never reasons.** Firing is "make a noise at T" — no policy
//!   re-evaluation, no model call. Anything that needs either at fire time is an
//!   FR-17 automation and does not belong in this module (ADR-023 boundary).
//! * **Names and notes are untrusted text.** A timer name reaches an audit row,
//!   a card, and (via [`Timer::announcement`]) a spoken line. It is sanitized and
//!   length-capped at construction, so no control/bidi smuggling survives into
//!   any of those (invariant 1 — it is data, never instructions).

use std::fmt;
use std::time::{Duration, SystemTime};

use crate::ids::{DeviceId, TimerId};
use crate::tools::sanitize_result_content;

/// Longest accepted timer name, in bytes. Names are spoken back ("the pasta
/// timer"), so anything longer is padding, not a name.
pub const MAX_TIMER_NAME_BYTES: usize = 64;

/// Longest accepted reminder note, in bytes. A reminder is one line ("call
/// Mom"), not a document — that is what artifacts are for.
pub const MAX_TIMER_NOTE_BYTES: usize = 512;

/// Furthest ahead a timer may be scheduled. A month covers every honest use
/// ("remind me next Tuesday") while keeping a mis-parsed date ("in 3000 years")
/// out of the table, where it would sit forever occupying the scheduler's
/// wakeup calculation.
pub const MAX_TIMER_HORIZON: Duration = Duration::from_secs(31 * 24 * 60 * 60);

/// How far in the past a fire time may be at creation. A timer set for a moment
/// that just passed is a clock-skew artifact, not an error; anything older than
/// this is a caller bug and is refused rather than fired instantly.
pub const MAX_BACKDATE: Duration = Duration::from_secs(60);

/// Default snooze. Nine minutes is the bedside-clock convention, and the human
/// asked for "a bit longer", not for a number.
pub const DEFAULT_SNOOZE: Duration = Duration::from_secs(9 * 60);

/// Longest accepted snooze — beyond this, set a new timer.
pub const MAX_SNOOZE: Duration = Duration::from_secs(24 * 60 * 60);

/// How late a fire may be before it is reported as **missed** rather than
/// merely fired. A scheduler wakeup is accurate to well under a second; a fire
/// this far behind its time means the daemon was not running when it came due
/// (ADR-023: "missed alarms announced on restart with a notice"), which the
/// human is told about rather than left to infer.
pub const MISSED_GRACE: Duration = Duration::from_secs(30);

/// A human-facing timer name — **untrusted text** (it is typed or spoken by a
/// person, and reaches an audit row, a card, and a spoken line).
///
/// Validated once at construction: control/bidi/zero-width characters stripped,
/// newlines flattened, length capped, non-empty after trimming. Downstream code
/// can therefore treat the name as safe display data without re-checking.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimerName(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimerNameError {
    #[error("a timer name must not be empty")]
    Empty,
}

impl TimerName {
    /// Validate and construct. Over-long names are **truncated**, not rejected:
    /// the name is a label, and a person who dictated a long one still wants
    /// their timer. Names that are empty once the smuggling characters are gone
    /// are rejected — a blank label is not a name.
    pub fn new(raw: &str) -> Result<Self, TimerNameError> {
        let cleaned = single_line(raw, MAX_TIMER_NAME_BYTES);
        if cleaned.is_empty() {
            return Err(TimerNameError::Empty);
        }
        Ok(Self(cleaned))
    }

    /// The name to use when the human named nothing ("set a timer for ten
    /// minutes"). Kept here rather than at each caller so the REST surface, the
    /// grammar, and the restart sweep all spell an unnamed timer identically.
    pub fn fallback_for(kind: &TimerKind) -> Self {
        Self(
            match kind {
                TimerKind::Countdown { .. } => "Timer",
                TimerKind::Alarm => "Alarm",
                TimerKind::Reminder { .. } => "Reminder",
            }
            .to_owned(),
        )
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TimerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The text a reminder announces ("call Mom") — untrusted, sanitized exactly
/// like a name. A separate newtype because it has its own (longer) bound and
/// because a reminder without a note is not a reminder.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimerNote(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimerNoteError {
    #[error("a reminder note must not be empty")]
    Empty,
}

impl TimerNote {
    pub fn new(raw: &str) -> Result<Self, TimerNoteError> {
        let cleaned = single_line(raw, MAX_TIMER_NOTE_BYTES);
        if cleaned.is_empty() {
            return Err(TimerNoteError::Empty);
        }
        Ok(Self(cleaned))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TimerNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Strip smuggling characters, flatten to one line, cap, trim. Shared by the
/// name and note newtypes so both boundaries behave identically.
fn single_line(raw: &str, max_bytes: usize) -> String {
    let cleaned = sanitize_result_content(raw, max_bytes).text;
    let flattened: String = cleaned
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .collect();
    flattened.trim().to_owned()
}

/// What kind of thing is due, which decides only how it is *announced* — every
/// kind fires the same way (ADR-023: they are one mechanism, not three).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerKind {
    /// "Set a ten minute timer." The duration is kept so the card can show the
    /// original span, and so a snoozed countdown can say what it was.
    Countdown { duration: Duration },
    /// "Wake me at seven." A wall-clock instant; nothing else distinguishes it.
    Alarm,
    /// "Remind me to call Mom at six." Fires like an alarm and speaks the note.
    Reminder { note: TimerNote },
}

impl TimerKind {
    /// The stable wire/storage spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Countdown { .. } => "countdown",
            Self::Alarm => "alarm",
            Self::Reminder { .. } => "reminder",
        }
    }
}

/// Where a timer is in its life. Closed set; the transitions between them are
/// [`TimerState::apply`] and nothing else.
///
/// `Fired` is deliberately **not** terminal: a ringing timer is waiting for a
/// human to dismiss or snooze it, and one that is never answered stays visible
/// (and is re-announced after a restart) rather than quietly disappearing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TimerState {
    /// Scheduled, not yet due.
    Pending,
    /// Came due and was announced; awaiting dismiss or snooze.
    Fired,
    /// Fired, then pushed out to a new time. Behaves like `Pending` for
    /// scheduling; kept distinct so the card can say "snoozed" and the restart
    /// sweep can tell a first fire from a repeat.
    Snoozed,
    /// The human acknowledged it. Terminal.
    Dismissed,
    /// The human called it off before it fired. Terminal.
    Cancelled,
}

impl TimerState {
    /// The stable wire/storage spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Fired => "fired",
            Self::Snoozed => "snoozed",
            Self::Dismissed => "dismissed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse the stored spelling. Unknown values are an error, never a default —
    /// a row we cannot interpret must not silently read as `Pending` and fire.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "fired" => Some(Self::Fired),
            "snoozed" => Some(Self::Snoozed),
            "dismissed" => Some(Self::Dismissed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Nothing further will ever happen to this timer.
    pub fn is_terminal(self) -> bool {
        match self {
            Self::Dismissed | Self::Cancelled => true,
            Self::Pending | Self::Fired | Self::Snoozed => false,
        }
    }

    /// Waiting for a fire time to arrive — the scheduler's worklist.
    pub fn is_armed(self) -> bool {
        match self {
            Self::Pending | Self::Snoozed => true,
            Self::Fired | Self::Dismissed | Self::Cancelled => false,
        }
    }

    /// Still of interest after a restart: either armed, or ringing unanswered.
    /// This is exactly the set the restart sweep reloads (ADR-023 missed alarms).
    pub fn is_live(self) -> bool {
        !self.is_terminal()
    }

    /// **The** transition table. Exhaustive over every (state, action) pair with
    /// no `_` arm: adding a state or a verb forces a decision on every pairing
    /// rather than inheriting a silent default.
    pub fn apply(self, action: TimerAction) -> Result<Self, TimerTransitionError> {
        use TimerAction as A;
        use TimerState as S;
        let next = match (self, action) {
            // Arming → ringing.
            (S::Pending, A::Fire) | (S::Snoozed, A::Fire) => S::Fired,
            // The human calls it off before it rings.
            (S::Pending, A::Cancel) | (S::Snoozed, A::Cancel) => S::Cancelled,
            // The human answers a ringing timer.
            (S::Fired, A::Dismiss) => S::Dismissed,
            (S::Fired, A::Snooze) => S::Snoozed,
            // Dismissing something that is merely snoozed is "I'm done with it"
            // — accepted, and recorded as dismissed rather than cancelled
            // because it *did* fire once.
            (S::Snoozed, A::Dismiss) => S::Dismissed,
            // A ringing timer cannot be "cancelled": it already happened. The
            // human dismisses it. Refused rather than silently aliased so the
            // audit trail distinguishes "never fired" from "fired and answered".
            //
            // Nor may it fire again — this is the guard that makes a double
            // wakeup, a duplicated restart sweep, or two schedulers racing
            // announce a timer exactly once (the store's compare-and-set is the
            // other half of that guarantee).
            (S::Fired, A::Cancel) | (S::Fired, A::Fire) => {
                return Err(TimerTransitionError::AlreadyFired);
            }
            // Snoozing something that is not currently ringing would move its
            // time without the human having heard it — that is a reschedule,
            // not a snooze, and this module does not offer one. Dismissing a
            // timer that never rang is likewise a cancel, and is named as such.
            (S::Pending, A::Snooze) | (S::Pending, A::Dismiss) | (S::Snoozed, A::Snooze) => {
                return Err(TimerTransitionError::NotFired);
            }
            // Terminal is terminal: a replayed decision (double-tap, retried
            // request) must change nothing.
            (S::Dismissed | S::Cancelled, A::Fire | A::Cancel | A::Dismiss | A::Snooze) => {
                return Err(TimerTransitionError::Terminal(self));
            }
        };
        Ok(next)
    }
}

impl fmt::Display for TimerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The verbs that move a timer. Closed set — a novel verb is rejected at parse
/// time rather than reaching the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerAction {
    /// The scheduler observed the fire time pass.
    Fire,
    Cancel,
    Dismiss,
    Snooze,
}

impl TimerAction {
    /// Parse a wire verb. `Fire` is deliberately **not** parseable: firing is
    /// something the clock does, never something a request asks for.
    pub fn parse(verb: &str) -> Result<Self, TimerActionError> {
        match verb {
            "cancel" => Ok(Self::Cancel),
            "dismiss" => Ok(Self::Dismiss),
            "snooze" => Ok(Self::Snooze),
            other => Err(TimerActionError::Unknown(other.to_owned())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fire => "fire",
            Self::Cancel => "cancel",
            Self::Dismiss => "dismiss",
            Self::Snooze => "snooze",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimerActionError {
    #[error("unknown timer action `{0}`")]
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimerTransitionError {
    #[error("that timer is already {0} and cannot change")]
    Terminal(TimerState),
    #[error("that timer has not gone off yet")]
    NotFired,
    #[error("that timer already went off; dismiss it instead")]
    AlreadyFired,
}

/// Why a timer could not be scheduled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimerScheduleError {
    #[error("that time is too far in the past")]
    InPast,
    #[error("a timer cannot be set more than {} days ahead", MAX_TIMER_HORIZON.as_secs() / 86_400)]
    TooFarAhead,
    #[error("a snooze must be between 1 second and {} hours", MAX_SNOOZE.as_secs() / 3_600)]
    SnoozeOutOfRange,
}

/// One timer, alarm, or reminder.
///
/// Fields are private and mutation happens only through the lifecycle methods,
/// which route through [`TimerState::apply`] — so there is no way to reach a
/// state the table forbids, and no way to move `fire_at` except by snoozing.
/// [`Timer::from_parts`] exists for the repository to rehydrate a stored row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timer {
    id: TimerId,
    name: TimerName,
    kind: TimerKind,
    state: TimerState,
    fire_at: SystemTime,
    created_at: SystemTime,
    /// The device that set this timer — the room that spoke (F8.5, FR-33).
    ///
    /// `Option` because not every timer has a room: one set from the shell, or
    /// later by an automation, was set by nobody standing anywhere. That case
    /// is not an error and must still ring somewhere sensible, so the absence
    /// is modelled rather than defaulted.
    ///
    /// Provenance, so it is immutable for the same reason `created_at` is: a
    /// timer that could be re-homed after the fact could be made to ring in a
    /// room its setter never chose.
    origin_device: Option<DeviceId>,
}

impl Timer {
    /// Schedule a new timer, `Pending`, due at `fire_at`.
    ///
    /// `now` is the caller's clock reading — the only place a clock enters, and
    /// it enters as data. A fire time in the past by more than [`MAX_BACKDATE`]
    /// or further ahead than [`MAX_TIMER_HORIZON`] is refused.
    pub fn schedule(
        id: TimerId,
        name: TimerName,
        kind: TimerKind,
        fire_at: SystemTime,
        now: SystemTime,
    ) -> Result<Self, TimerScheduleError> {
        match fire_at.duration_since(now) {
            Ok(ahead) if ahead > MAX_TIMER_HORIZON => return Err(TimerScheduleError::TooFarAhead),
            Ok(_) => {}
            // `duration_since` errs when `fire_at` precedes `now`; the error
            // carries the gap, so a small clock skew is tolerated and a real
            // backdate is refused.
            Err(behind) if behind.duration() > MAX_BACKDATE => {
                return Err(TimerScheduleError::InPast);
            }
            Err(_) => {}
        }
        Ok(Self {
            id,
            name,
            kind,
            state: TimerState::Pending,
            fire_at,
            created_at: now,
            // Attribution is applied by the caller that knows the actor; a
            // timer is valid without one.
            origin_device: None,
        })
    }

    /// Attribute this timer to the device that set it (F8.5).
    ///
    /// A builder rather than a `schedule` parameter because attribution is
    /// genuinely optional and the scheduling rules — horizon, backdate — have
    /// nothing to do with it.
    #[must_use]
    pub fn with_origin(mut self, origin_device: Option<DeviceId>) -> Self {
        self.origin_device = origin_device;
        self
    }

    /// Rehydrate a stored row. The repository is the only caller; it has already
    /// validated the state spelling via [`TimerState::parse`].
    pub fn from_parts(
        id: TimerId,
        name: TimerName,
        kind: TimerKind,
        state: TimerState,
        fire_at: SystemTime,
        created_at: SystemTime,
        origin_device: Option<DeviceId>,
    ) -> Self {
        Self {
            id,
            name,
            kind,
            state,
            fire_at,
            created_at,
            origin_device,
        }
    }

    pub fn id(&self) -> &TimerId {
        &self.id
    }

    pub fn name(&self) -> &TimerName {
        &self.name
    }

    pub fn kind(&self) -> &TimerKind {
        &self.kind
    }

    pub fn state(&self) -> TimerState {
        self.state
    }

    pub fn fire_at(&self) -> SystemTime {
        self.fire_at
    }

    pub fn created_at(&self) -> SystemTime {
        self.created_at
    }

    /// The device this timer was set on, if it was set on one.
    ///
    /// The fire path uses this to ring in the room that spoke; `None` means
    /// "nobody's room", and the caller falls back to the host (F8.5).
    pub fn origin_device(&self) -> Option<&DeviceId> {
        self.origin_device.as_ref()
    }

    /// **The** due decision (docs/02 §11e): armed, and its moment has arrived.
    /// Everything that fires a timer asks this — the scheduler wakeup, the
    /// restart sweep, and the tests — so "due" cannot mean two things.
    pub fn is_due_at(&self, now: SystemTime) -> bool {
        self.state.is_armed() && self.fire_at <= now
    }

    /// How long until this timer goes off, or `Duration::ZERO` once it is due.
    /// `None` when the timer is not armed (already ringing, or finished) — the
    /// card shows no countdown rather than a frozen one.
    pub fn remaining_at(&self, now: SystemTime) -> Option<Duration> {
        if !self.state.is_armed() {
            return None;
        }
        Some(self.fire_at.duration_since(now).unwrap_or(Duration::ZERO))
    }

    /// How far past its fire time this timer is, `ZERO` if it is not yet due.
    pub fn lateness_at(&self, now: SystemTime) -> Duration {
        now.duration_since(self.fire_at).unwrap_or(Duration::ZERO)
    }

    /// True when this timer came due while nothing was watching — the fire is
    /// late by more than a scheduler wakeup could explain, so the human is told
    /// it was missed rather than being shown a timer that appears to have just
    /// gone off (ADR-023).
    pub fn is_missed_at(&self, now: SystemTime, grace: Duration) -> bool {
        self.is_due_at(now) && self.lateness_at(now) > grace
    }

    /// Move to `Fired`. Returns the updated timer; the caller persists it with a
    /// compare-and-set so a timer fires exactly once even with two schedulers.
    pub fn fire(&self) -> Result<Self, TimerTransitionError> {
        Ok(self.with_state(self.state.apply(TimerAction::Fire)?))
    }

    pub fn cancel(&self) -> Result<Self, TimerTransitionError> {
        Ok(self.with_state(self.state.apply(TimerAction::Cancel)?))
    }

    pub fn dismiss(&self) -> Result<Self, TimerTransitionError> {
        Ok(self.with_state(self.state.apply(TimerAction::Dismiss)?))
    }

    /// Push a ringing timer out by `by`, measured from `now` (not from the
    /// original fire time — "nine more minutes" means nine more minutes from
    /// when the human said so, however long it rang first).
    pub fn snooze(&self, now: SystemTime, by: Duration) -> Result<Self, TimerSnoozeError> {
        if by.is_zero() || by > MAX_SNOOZE {
            return Err(TimerSnoozeError::Schedule(
                TimerScheduleError::SnoozeOutOfRange,
            ));
        }
        let next = self.state.apply(TimerAction::Snooze)?;
        let mut snoozed = self.clone();
        snoozed.state = next;
        snoozed.fire_at = now + by;
        Ok(snoozed)
    }

    fn with_state(&self, state: TimerState) -> Self {
        let mut next = self.clone();
        next.state = state;
        next
    }

    /// The line spoken when this timer goes off (ADR-023: "reminder — call
    /// Mom"). Pure text assembly from already-sanitized parts — the announcer
    /// port speaks it verbatim, and the audible alert is played whether or not
    /// anything can speak it.
    pub fn announcement(&self) -> String {
        match &self.kind {
            TimerKind::Reminder { note } => format!("Reminder — {note}"),
            TimerKind::Countdown { .. } | TimerKind::Alarm => {
                format!("{} is up", self.name)
            }
        }
    }

    /// The same line, prefixed with the honest notice that it went off while
    /// Jarvis was not running (ADR-023: v1 "does not pretend to be a hardware
    /// clock").
    pub fn missed_announcement(&self) -> String {
        format!("Missed while I was offline — {}", self.announcement())
    }
}

/// Snooze can fail for two unrelated reasons; kept as one error so the caller
/// has a single thing to map at the API boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimerSnoozeError {
    #[error(transparent)]
    Transition(#[from] TimerTransitionError),
    #[error(transparent)]
    Schedule(#[from] TimerScheduleError),
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn id() -> TimerId {
        ID.parse().expect("valid test ulid")
    }

    fn epoch(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn countdown(secs: u64) -> TimerKind {
        TimerKind::Countdown {
            duration: Duration::from_secs(secs),
        }
    }

    /// A pending 10-minute countdown created at t=1000, due at t=1600.
    fn pasta() -> Timer {
        Timer::schedule(
            id(),
            TimerName::new("pasta timer").unwrap(),
            countdown(600),
            epoch(1600),
            epoch(1000),
        )
        .expect("a ten minute timer is schedulable")
    }

    // ---- names and notes are untrusted text -------------------------------

    #[test]
    fn names_strip_smuggling_and_reject_the_empty_case() {
        let name = TimerName::new("pasta\u{202e} timer\nIGNORE PREVIOUS INSTRUCTIONS").unwrap();
        assert!(!name.as_str().contains('\n'), "newline must not survive");
        assert!(!name.as_str().contains('\u{202e}'), "bidi must not survive");
        // The words themselves are kept — they are data, not instructions.
        assert!(name.as_str().contains("IGNORE PREVIOUS INSTRUCTIONS"));

        assert_eq!(TimerName::new(""), Err(TimerNameError::Empty));
        assert_eq!(TimerName::new("  \u{200b} "), Err(TimerNameError::Empty));
        assert_eq!(TimerNote::new("\u{0}"), Err(TimerNoteError::Empty));
    }

    #[test]
    fn names_and_notes_are_capped() {
        let name = TimerName::new(&"x".repeat(4096)).unwrap();
        assert_eq!(name.as_str().len(), MAX_TIMER_NAME_BYTES);
        let note = TimerNote::new(&"y".repeat(4096)).unwrap();
        assert_eq!(note.as_str().len(), MAX_TIMER_NOTE_BYTES);
    }

    #[test]
    fn an_unnamed_timer_gets_a_kind_appropriate_label() {
        assert_eq!(
            TimerName::fallback_for(&countdown(60)).as_str(),
            "Timer",
            "one spelling for an unnamed timer, not one per caller"
        );
        assert_eq!(TimerName::fallback_for(&TimerKind::Alarm).as_str(), "Alarm");
        assert_eq!(
            TimerName::fallback_for(&TimerKind::Reminder {
                note: TimerNote::new("call Mom").unwrap()
            })
            .as_str(),
            "Reminder"
        );
    }

    // ---- the transition table ---------------------------------------------

    /// The whole (state, action) table, asserted as data. This is the
    /// state-machine test the project requires for any lifecycle change: every
    /// pairing appears exactly once, so a new state or verb makes this list
    /// incomplete and the exhaustive `match` in `apply` refuses to compile.
    #[test]
    fn the_timer_transition_table_is_exactly_this() {
        use TimerAction::*;
        use TimerState::*;
        let table: &[(
            TimerState,
            TimerAction,
            Result<TimerState, TimerTransitionError>,
        )] = &[
            (Pending, Fire, Ok(Fired)),
            (Pending, Cancel, Ok(Cancelled)),
            (Pending, Dismiss, Err(TimerTransitionError::NotFired)),
            (Pending, Snooze, Err(TimerTransitionError::NotFired)),
            (Fired, Fire, Err(TimerTransitionError::AlreadyFired)),
            (Fired, Cancel, Err(TimerTransitionError::AlreadyFired)),
            (Fired, Dismiss, Ok(Dismissed)),
            (Fired, Snooze, Ok(Snoozed)),
            (Snoozed, Fire, Ok(Fired)),
            (Snoozed, Cancel, Ok(Cancelled)),
            (Snoozed, Dismiss, Ok(Dismissed)),
            (Snoozed, Snooze, Err(TimerTransitionError::NotFired)),
            (
                Dismissed,
                Fire,
                Err(TimerTransitionError::Terminal(Dismissed)),
            ),
            (
                Dismissed,
                Cancel,
                Err(TimerTransitionError::Terminal(Dismissed)),
            ),
            (
                Dismissed,
                Dismiss,
                Err(TimerTransitionError::Terminal(Dismissed)),
            ),
            (
                Dismissed,
                Snooze,
                Err(TimerTransitionError::Terminal(Dismissed)),
            ),
            (
                Cancelled,
                Fire,
                Err(TimerTransitionError::Terminal(Cancelled)),
            ),
            (
                Cancelled,
                Cancel,
                Err(TimerTransitionError::Terminal(Cancelled)),
            ),
            (
                Cancelled,
                Dismiss,
                Err(TimerTransitionError::Terminal(Cancelled)),
            ),
            (
                Cancelled,
                Snooze,
                Err(TimerTransitionError::Terminal(Cancelled)),
            ),
        ];
        for (state, action, expected) in table {
            assert_eq!(
                state.apply(*action),
                *expected,
                "{state:?} + {action:?} must be {expected:?}"
            );
        }
        // Every (state, action) pair is covered exactly once above.
        assert_eq!(table.len(), 5 * 4);
    }

    #[test]
    fn a_terminal_timer_never_moves_again() {
        let cancelled = pasta().cancel().unwrap();
        assert_eq!(cancelled.state(), TimerState::Cancelled);
        // A replayed cancel (double-tap, retried request) changes nothing.
        assert_eq!(
            cancelled.cancel(),
            Err(TimerTransitionError::Terminal(TimerState::Cancelled))
        );
        assert_eq!(
            cancelled.fire(),
            Err(TimerTransitionError::Terminal(TimerState::Cancelled)),
            "a cancelled timer must never ring"
        );
    }

    #[test]
    fn a_ringing_timer_is_dismissed_not_cancelled() {
        let fired = pasta().fire().unwrap();
        assert_eq!(
            fired.cancel(),
            Err(TimerTransitionError::AlreadyFired),
            "cancelling something that already rang would falsify the audit trail"
        );
        assert_eq!(fired.dismiss().unwrap().state(), TimerState::Dismissed);
    }

    #[test]
    fn only_the_clock_may_fire_a_timer() {
        // `fire` has no wire spelling: a request can never ask for one.
        assert_eq!(
            TimerAction::parse("fire"),
            Err(TimerActionError::Unknown("fire".to_owned()))
        );
        for verb in ["cancel", "dismiss", "snooze"] {
            assert_eq!(TimerAction::parse(verb).unwrap().as_str(), verb);
        }
        assert!(TimerAction::parse("rm -rf /").is_err());
    }

    // ---- what is due at instant T -----------------------------------------

    #[test]
    fn due_is_armed_plus_arrived() {
        let t = pasta();
        assert!(!t.is_due_at(epoch(1599)), "one second early is not due");
        assert!(t.is_due_at(epoch(1600)), "the fire instant is due");
        assert!(t.is_due_at(epoch(9999)));

        // Ringing and finished timers are not "due" — they must not re-fire.
        assert!(!t.fire().unwrap().is_due_at(epoch(9999)));
        assert!(!t.cancel().unwrap().is_due_at(epoch(9999)));
        assert!(!t.fire().unwrap().dismiss().unwrap().is_due_at(epoch(9999)));
    }

    #[test]
    fn remaining_counts_down_and_floors_at_zero() {
        let t = pasta();
        assert_eq!(t.remaining_at(epoch(1000)), Some(Duration::from_secs(600)));
        assert_eq!(t.remaining_at(epoch(1599)), Some(Duration::from_secs(1)));
        assert_eq!(
            t.remaining_at(epoch(2000)),
            Some(Duration::ZERO),
            "an overdue timer reads zero, never a negative or a wrapped value"
        );
        assert_eq!(
            t.fire().unwrap().remaining_at(epoch(1000)),
            None,
            "a ringing timer has no countdown to show"
        );
    }

    #[test]
    fn a_timer_that_came_due_while_we_were_down_reads_as_missed() {
        // THE feature test (ADR-023): the daemon was stopped at t=1500 and comes
        // back at t=5000. The 1600 timer is still Pending in the database, and
        // must be both due AND flagged missed — never silently swallowed, and
        // never presented as if it had just gone off.
        let t = pasta();
        let restart = epoch(5000);
        assert!(
            t.is_due_at(restart),
            "a missed timer is still due on restart"
        );
        assert!(t.is_missed_at(restart, MISSED_GRACE));
        assert_eq!(t.lateness_at(restart), Duration::from_secs(3400));
        assert!(
            t.missed_announcement()
                .starts_with("Missed while I was offline"),
            "the human is told it was missed: {}",
            t.missed_announcement()
        );

        // A fire within the grace window is a normal, on-time fire.
        let prompt = epoch(1600) + MISSED_GRACE;
        assert!(!t.is_missed_at(prompt, MISSED_GRACE));
        assert!(t.is_missed_at(prompt + Duration::from_secs(1), MISSED_GRACE));

        // A cancelled timer is NOT resurrected by a restart.
        assert!(!t.cancel().unwrap().is_missed_at(restart, MISSED_GRACE));
        // Nor is one that already rang and was dismissed.
        assert!(
            !t.fire()
                .unwrap()
                .dismiss()
                .unwrap()
                .is_missed_at(restart, MISSED_GRACE)
        );
    }

    #[test]
    fn a_fired_but_unanswered_timer_survives_a_restart_as_live() {
        // It stopped ringing when the process died; it is not armed (it must not
        // fire twice) but it is still live, so the restart sweep reloads it and
        // the human still sees the card.
        let fired = pasta().fire().unwrap();
        assert!(!fired.state().is_armed());
        assert!(fired.state().is_live());
        assert!(!fired.state().is_terminal());
    }

    // ---- snooze ------------------------------------------------------------

    #[test]
    fn snooze_measures_from_now_not_from_the_original_time() {
        let fired = pasta().fire().unwrap();
        // It rang at 1600 and the human snoozed at 1700: the new time is
        // 1700 + 9min, not 1600 + 9min.
        let snoozed = fired.snooze(epoch(1700), DEFAULT_SNOOZE).unwrap();
        assert_eq!(snoozed.state(), TimerState::Snoozed);
        assert_eq!(snoozed.fire_at(), epoch(1700) + DEFAULT_SNOOZE);
        assert!(snoozed.state().is_armed(), "a snoozed timer rings again");
        assert!(!snoozed.is_due_at(epoch(1700)));
        assert!(snoozed.is_due_at(epoch(1700) + DEFAULT_SNOOZE));
        // And it can ring, be snoozed, and ring again.
        assert_eq!(snoozed.fire().unwrap().state(), TimerState::Fired);
    }

    #[test]
    fn snooze_is_bounded_and_only_applies_to_a_ringing_timer() {
        let fired = pasta().fire().unwrap();
        assert_eq!(
            fired.snooze(epoch(1700), Duration::ZERO),
            Err(TimerSnoozeError::Schedule(
                TimerScheduleError::SnoozeOutOfRange
            ))
        );
        assert_eq!(
            fired.snooze(epoch(1700), MAX_SNOOZE + Duration::from_secs(1)),
            Err(TimerSnoozeError::Schedule(
                TimerScheduleError::SnoozeOutOfRange
            ))
        );
        assert_eq!(
            pasta().snooze(epoch(1000), DEFAULT_SNOOZE),
            Err(TimerSnoozeError::Transition(TimerTransitionError::NotFired)),
            "snoozing something that never rang is a reschedule, not a snooze"
        );
    }

    // ---- scheduling bounds -------------------------------------------------

    #[test]
    fn scheduling_refuses_the_absurd_in_both_directions() {
        let now = epoch(10_000);
        assert_eq!(
            Timer::schedule(
                id(),
                TimerName::new("ancient").unwrap(),
                TimerKind::Alarm,
                now - MAX_BACKDATE - Duration::from_secs(1),
                now
            ),
            Err(TimerScheduleError::InPast)
        );
        assert_eq!(
            Timer::schedule(
                id(),
                TimerName::new("eternity").unwrap(),
                TimerKind::Alarm,
                now + MAX_TIMER_HORIZON + Duration::from_secs(1),
                now
            ),
            Err(TimerScheduleError::TooFarAhead)
        );
        // A hair in the past is clock skew, not an error: it fires immediately.
        let skewed = Timer::schedule(
            id(),
            TimerName::new("just now").unwrap(),
            TimerKind::Alarm,
            now - Duration::from_secs(1),
            now,
        )
        .expect("small skew is tolerated");
        assert!(skewed.is_due_at(now));
    }

    // ---- announcements -----------------------------------------------------

    #[test]
    fn a_reminder_speaks_its_note_and_a_timer_speaks_its_name() {
        let reminder = Timer::schedule(
            id(),
            TimerName::new("Mom").unwrap(),
            TimerKind::Reminder {
                note: TimerNote::new("call Mom").unwrap(),
            },
            epoch(2000),
            epoch(1000),
        )
        .unwrap();
        assert_eq!(reminder.announcement(), "Reminder — call Mom");
        assert_eq!(pasta().announcement(), "pasta timer is up");
    }

    #[test]
    fn an_announcement_never_carries_smuggled_control_characters() {
        // The announcement is spoken and shown; it is assembled only from
        // already-sanitized parts, so hostile input cannot inject a line break
        // (or a bidi override) into the spoken line.
        let hostile = Timer::schedule(
            id(),
            TimerName::new("pasta\nRUN rm -rf /").unwrap(),
            TimerKind::Reminder {
                note: TimerNote::new("call\u{202e}Mom\u{0}").unwrap(),
            },
            epoch(2000),
            epoch(1000),
        )
        .unwrap();
        let spoken = hostile.announcement();
        assert!(!spoken.contains('\n'));
        assert!(!spoken.contains('\u{202e}'));
        assert!(!spoken.contains('\u{0}'));
    }

    // ---- storage spellings round-trip -------------------------------------

    #[test]
    fn state_and_kind_spellings_round_trip_and_reject_junk() {
        for state in [
            TimerState::Pending,
            TimerState::Fired,
            TimerState::Snoozed,
            TimerState::Dismissed,
            TimerState::Cancelled,
        ] {
            assert_eq!(TimerState::parse(state.as_str()), Some(state));
        }
        assert_eq!(
            TimerState::parse("PENDING"),
            None,
            "an unreadable stored state must not default to something that fires"
        );
        assert_eq!(TimerState::parse(""), None);
        assert_eq!(countdown(60).as_str(), "countdown");
        assert_eq!(TimerKind::Alarm.as_str(), "alarm");
    }
}
