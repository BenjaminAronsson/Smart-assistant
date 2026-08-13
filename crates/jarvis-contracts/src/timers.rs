//! Timer/alarm/reminder wire DTOs (FR-33, docs/02 §11e, ADR-023).
//!
//! Three surfaces:
//!
//! * [`TimerDto`] — one timer, as the HUD card renders it and as the persisted
//!   `timer.fired` domain event carries it. **Persisted, not transient**
//!   (docs/05 §3): a timer going off is a fact about the world at a moment, so a
//!   client that was disconnected when the kitchen timer rang must still learn
//!   about it on resync. This is the opposite call from `media.state`, which is
//!   a current-value readout with no history worth replaying.
//! * [`CreateTimerRequest`] / [`TimerActionRequest`] — `POST /api/v1/timers` and
//!   `POST /api/v1/timers/{id}/action`, the owner-driven set and
//!   cancel/dismiss/snooze paths.
//! * [`TimerListResponse`] — `GET /api/v1/timers`, the enumerable list ADR-023
//!   requires ("how long left?", "cancel the pasta timer").
//!
//! `name` and `note` are human text, sanitized by `jarvis_domain::timers` before
//! they are projected here; the client renders them as text only, never markup.
//!
//! Note what is **absent**: there is no schedule, no recurrence, no condition,
//! and no action field. Those belong to FR-17 automations. The ADR-023 boundary
//! is "make a noise at T" — anything needing policy re-evaluation or model
//! reasoning at fire time is not a timer, and the wire shape says so.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Wire mirror of `jarvis_domain::timers::TimerKind`, flattened: the
/// kind-specific payload rides in [`TimerDto`]'s optional `durationSecs`/`note`
/// so a client can switch on one string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TimerKindDto {
    /// "Ten minutes from now."
    Countdown,
    /// "At seven."
    Alarm,
    /// "At six, to call Mom."
    Reminder,
}

impl From<&jarvis_domain::timers::TimerKind> for TimerKindDto {
    fn from(kind: &jarvis_domain::timers::TimerKind) -> Self {
        use jarvis_domain::timers::TimerKind as K;
        match kind {
            K::Countdown { .. } => Self::Countdown,
            K::Alarm => Self::Alarm,
            K::Reminder { .. } => Self::Reminder,
        }
    }
}

/// Wire mirror of `jarvis_domain::timers::TimerState`. Total projection (no `_`
/// arm) so a new state forces a decision here rather than silently mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TimerStateDto {
    Pending,
    /// Ringing, awaiting dismiss or snooze — **not** terminal.
    Fired,
    Snoozed,
    Dismissed,
    Cancelled,
}

impl From<jarvis_domain::timers::TimerState> for TimerStateDto {
    fn from(state: jarvis_domain::timers::TimerState) -> Self {
        use jarvis_domain::timers::TimerState as S;
        match state {
            S::Pending => Self::Pending,
            S::Fired => Self::Fired,
            S::Snoozed => Self::Snoozed,
            S::Dismissed => Self::Dismissed,
            S::Cancelled => Self::Cancelled,
        }
    }
}

/// One timer as the HUD renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TimerDto {
    #[schemars(with = "crate::schema::UlidString")]
    pub id: jarvis_domain::ids::TimerId,
    /// Human label ("pasta timer"), sanitized. Rendered as text.
    pub name: String,
    pub kind: TimerKindDto,
    pub state: TimerStateDto,
    /// When it goes (or went) off, RFC 3339 UTC.
    pub fire_at: String,
    /// The countdown's original span — present only for `countdown`, so the card
    /// can show "10 min" alongside the live remainder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    /// The reminder's spoken line ("call Mom") — present only for `reminder`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Seconds left **at the moment this DTO was produced**. Absent when the
    /// timer is not armed (ringing or finished). The card ticks its own display
    /// down from here rather than polling the server — one value, then local
    /// arithmetic (docs/09 §5: no polling loop for something a clock can do).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_secs: Option<u64>,
    /// The device this timer was set on — the room it must ring in (F8.5,
    /// FR-33). Absent when it was set from nowhere in particular (the shell, or
    /// an automation), which is the case that falls back to the daemon's own
    /// speaker.
    ///
    /// Same name and same meaning as the display directives' `targetDeviceId`
    /// (F7.5): the fan-out addresses on this field, so an alert reaches exactly
    /// the room that set it and no other.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

/// `POST /api/v1/timers`. Exactly one of `durationSecs` (a countdown) or
/// `fireAt` (an alarm/reminder) must be given; `note` is required by — and only
/// meaningful for — `reminder`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateTimerRequest {
    /// Optional label. Omitted ⇒ the server uses the kind's default ("Timer",
    /// "Alarm", "Reminder") so every timer is enumerable and addressable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kind: TimerKindDto,
    /// "…in ten minutes", relative to the server's clock at set time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    /// "…at seven", RFC 3339. Absolute so the server never has to guess a
    /// timezone or a "next Tuesday".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fire_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `GET /api/v1/timers` — everything outstanding, earliest first. Terminal
/// timers are not listed: "what have I got set?" means the live ones.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TimerListResponse {
    pub timers: Vec<TimerDto>,
    /// Server time when the list was taken, RFC 3339. The card uses it to
    /// correct for clock skew between the browser and the daemon instead of
    /// trusting `Date.now()`.
    pub now: String,
}

/// `POST /api/v1/timers/{id}/action`.
///
/// `action` is one of `cancel`, `dismiss`, `snooze`. There is deliberately **no
/// `fire`**: firing is something the clock does, never something a request asks
/// for — an unknown verb is a 400 and never reaches the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TimerActionRequest {
    pub action: String,
    /// How long to push a ringing timer out by. Honoured by `snooze` only;
    /// omitted ⇒ the nine-minute default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snooze_secs: Option<u64>,
}

/// The timer after the action was applied and audited, so the card re-renders
/// without waiting for an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TimerActionResponse {
    pub timer: TimerDto,
}
