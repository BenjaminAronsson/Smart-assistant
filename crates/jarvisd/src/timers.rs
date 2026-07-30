//! Timer surface (F3b.7, FR-33, docs/02 §11e, ADR-023): the REST entry points,
//! the wire projection, and the scheduler that actually makes the noise.
//!
//! Three pieces:
//!
//! * **[`TimerApi`]** — `GET /api/v1/timers`, `POST /api/v1/timers`,
//!   `POST /api/v1/timers/{id}/action`. Owner-driven and authenticated, the same
//!   shape as the media command surface: this is a human pressing a button on
//!   their own paired device, not a model proposal. There is deliberately **no
//!   registered timer tool** in this feature — nothing reaches these endpoints
//!   from model output, so invariant 1 is untouched by them.
//! * **[`TimerEncoder`]** — projects a fired timer into the persisted
//!   `timer.fired` outbox payload. It lives here because jarvisd owns
//!   `jarvis-contracts`; the application layer names the capability and never
//!   sees a wire type (invariant 3).
//! * **[`run_scheduler`]** — the one resident task. It is **event-driven, not
//!   polling** (docs/09 §5): it sleeps exactly until the next fire time and is
//!   woken early by [`TimerApi`] when a timer is set or snoozed. With nothing
//!   armed it parks on the notify and burns nothing at all.
//!
//! The scheduler's first act on startup is a sweep, which is the entire
//! missed-alarm mechanism (ADR-023): a timer whose moment passed while jarvisd
//! was stopped is simply a very late timer, fired through the same code path and
//! flagged `missed` so the human is told rather than misled.
//!
//! **Boundary with FR-17 automations.** Everything here is "make a noise at T".
//! There is no policy evaluation on the fire path and nowhere to put a tool
//! call: anything that needs either at fire time is an automation and belongs to
//! that module.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::{Extension, Json};
use jarvis_application::ports::{DomainEventRecord, TimerEventEncoder};
use jarvis_application::timers::{NewTimer, TimerService, TimerServiceError, TimerWhen};
use jarvis_contracts::errors::ErrorCode;
use jarvis_contracts::timers::{
    CreateTimerRequest, TimerActionRequest, TimerActionResponse, TimerDto, TimerKindDto,
    TimerListResponse,
};
use jarvis_domain::ids::TimerId;
use jarvis_domain::timers::{
    MAX_SNOOZE, Timer, TimerAction, TimerKind, TimerNote, TimerScheduleError, TimerTransitionError,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::auth::DeviceContext;
use crate::problem::problem;

/// Never sleep for less than this between scheduler passes. Belt-and-braces
/// against a hot loop: if a timer ever reads "due" but cannot be fired, the
/// scheduler must degrade to a slow retry, not spin a core (docs/09 §5).
const MIN_SCHEDULER_SLEEP: Duration = Duration::from_millis(100);

/// Project a domain timer onto the wire.
///
/// `now` is the instant the countdown is measured from — the caller passes its
/// clock reading so every timer in one response shares one `now` and the card's
/// arithmetic is self-consistent.
pub fn to_timer_dto(timer: &Timer, now: SystemTime) -> TimerDto {
    let (duration_secs, note) = match timer.kind() {
        TimerKind::Countdown { duration } => (Some(duration.as_secs()), None),
        TimerKind::Alarm => (None, None),
        TimerKind::Reminder { note } => (None, Some(note.as_str().to_owned())),
    };
    TimerDto {
        id: timer.id().clone(),
        name: timer.name().as_str().to_owned(),
        kind: TimerKindDto::from(timer.kind()),
        state: timer.state().into(),
        fire_at: rfc3339(timer.fire_at()),
        duration_secs,
        note,
        remaining_secs: timer.remaining_at(now).map(|d| d.as_secs()),
    }
}

fn rfc3339(t: SystemTime) -> String {
    OffsetDateTime::from(t)
        .format(&Rfc3339)
        .expect("UTC timestamp formats")
}

/// Encodes `timer.fired` for the transactional outbox (docs/05 §3).
///
/// The payload is the wire event MINUS its `type` discriminator — the envelope
/// carries that — matching the run/approval outbox convention that
/// `jarvisd::runs::domain_event` folds back on resync.
pub struct TimerEncoder;

impl TimerEventEncoder for TimerEncoder {
    fn fired(&self, timer: &Timer, missed: bool) -> DomainEventRecord {
        // A fired timer is not armed, so it has no remaining countdown; the
        // instant passed here only anchors the projection.
        let dto = to_timer_dto(timer, timer.fire_at());
        DomainEventRecord {
            event_type: "timer.fired".to_owned(),
            payload_json: serde_json::json!({ "timer": dto, "missed": missed }).to_string(),
        }
    }
}

/// State for the timer routes. Cloneable so it can be axum route state.
#[derive(Clone)]
pub struct TimerApi {
    service: Arc<TimerService>,
    /// Poked whenever the armed set changes, so the scheduler recomputes its
    /// sleep instead of waiting out a stale one.
    wake: Arc<Notify>,
}

impl TimerApi {
    pub fn new(service: Arc<TimerService>, wake: Arc<Notify>) -> Self {
        Self { service, wake }
    }
}

/// `GET /api/v1/timers` — everything outstanding, earliest first.
pub async fn list(State(api): State<TimerApi>) -> Result<Json<TimerListResponse>, Response> {
    let cancel = CancellationToken::new();
    let now = api.service.now();
    let timers = api.service.list(&cancel).await.map_err(service_problem)?;
    Ok(Json(TimerListResponse {
        timers: timers.iter().map(|t| to_timer_dto(t, now)).collect(),
        now: rfc3339(now),
    }))
}

/// `POST /api/v1/timers` — set a timer, alarm, or reminder.
pub async fn create(
    State(api): State<TimerApi>,
    Extension(device): Extension<DeviceContext>,
    Json(req): Json<CreateTimerRequest>,
) -> Result<(StatusCode, Json<TimerDto>), Response> {
    let cancel = CancellationToken::new();
    let request = new_timer(&req).map_err(fault_response)?;
    // The id is minted here (the host owns randomness; the domain only
    // validates) — ULID, so the list is naturally creation-ordered too.
    let id: TimerId = crate::auth::fresh_id();
    let timer = api
        .service
        .set(
            id,
            request,
            &format!("device:{}", device.device_id),
            &cancel,
        )
        .await
        .map_err(service_problem)?;
    // The armed set changed: recompute the sleep rather than wait one out.
    api.wake.notify_one();
    let now = api.service.now();
    Ok((StatusCode::CREATED, Json(to_timer_dto(&timer, now))))
}

/// `POST /api/v1/timers/{id}/action` — cancel, dismiss, or snooze.
pub async fn act(
    State(api): State<TimerApi>,
    Path(id): Path<String>,
    Extension(device): Extension<DeviceContext>,
    Json(req): Json<TimerActionRequest>,
) -> Result<Json<TimerActionResponse>, Response> {
    let cancel = CancellationToken::new();
    let id: TimerId = id
        .parse()
        .map_err(|_| bad_request("timer id is not a ULID"))?;
    // `fire` has no wire spelling, so this rejects it along with any other verb
    // the client invented — firing is the clock's job, never a request's.
    let action = TimerAction::parse(&req.action).map_err(|e| bad_request(&e.to_string()))?;
    let snooze_by = match req.snooze_secs {
        Some(secs) if secs == 0 || Duration::from_secs(secs) > MAX_SNOOZE => {
            return Err(bad_request(
                &TimerScheduleError::SnoozeOutOfRange.to_string(),
            ));
        }
        Some(secs) => Some(Duration::from_secs(secs)),
        None => None,
    };
    let timer = api
        .service
        .act(
            &id,
            action,
            snooze_by,
            &format!("device:{}", device.device_id),
            &cancel,
        )
        .await
        .map_err(service_problem)?;
    api.wake.notify_one();
    let now = api.service.now();
    Ok(Json(TimerActionResponse {
        timer: to_timer_dto(&timer, now),
    }))
}

/// Validate a create request into the application's request type.
///
/// The rules are structural, so they are enforced here rather than left to a
/// schema comment: exactly one of `durationSecs`/`fireAt`, matched to the kind,
/// and a reminder must carry its note (there is nothing to announce otherwise).
fn new_timer(req: &CreateTimerRequest) -> Result<NewTimer, TimerFault> {
    let kind = match req.kind {
        TimerKindDto::Countdown => TimerKind::Countdown {
            duration: Duration::from_secs(
                req.duration_secs
                    .ok_or(TimerFault::CountdownNeedsDuration)?,
            ),
        },
        TimerKindDto::Alarm => TimerKind::Alarm,
        TimerKindDto::Reminder => TimerKind::Reminder {
            note: TimerNote::new(req.note.as_deref().ok_or(TimerFault::ReminderNeedsNote)?)
                .map_err(|_| TimerFault::ReminderNeedsNote)?,
        },
    };
    let when = match (req.duration_secs, req.fire_at.as_deref()) {
        (Some(secs), None) => TimerWhen::In(Duration::from_secs(secs)),
        (None, Some(raw)) => TimerWhen::At(
            OffsetDateTime::parse(raw, &Rfc3339)
                .map_err(|_| TimerFault::UnparseableTime)?
                .into(),
        ),
        (Some(_), Some(_)) => return Err(TimerFault::AmbiguousTime),
        (None, None) => return Err(TimerFault::NoTime),
    };
    Ok(NewTimer {
        name: req.name.clone(),
        kind,
        when,
    })
}

/// Why a create request could not be turned into a timer. A small enum rather
/// than a prebuilt `Response`: an axum `Response` is large, and returning one in
/// every helper's `Err` makes those results enormous (clippy `result_large_err`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerFault {
    CountdownNeedsDuration,
    ReminderNeedsNote,
    UnparseableTime,
    AmbiguousTime,
    NoTime,
}

fn fault_response(fault: TimerFault) -> Response {
    bad_request(match fault {
        TimerFault::CountdownNeedsDuration => "a countdown needs durationSecs",
        TimerFault::ReminderNeedsNote => "a reminder needs a non-empty note",
        TimerFault::UnparseableTime => "fireAt must be an RFC 3339 timestamp",
        TimerFault::AmbiguousTime => "give durationSecs or fireAt, not both",
        TimerFault::NoTime => "give durationSecs or fireAt",
    })
}

fn bad_request(detail: &str) -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        ErrorCode::ValidationFailed,
        detail,
        None,
    )
}

fn service_problem(error: TimerServiceError) -> Response {
    match error {
        TimerServiceError::NotFound => problem(
            StatusCode::NOT_FOUND,
            ErrorCode::ResourceNotFound,
            "no such timer",
            None,
        ),
        TimerServiceError::Schedule(e) => bad_request(&e.to_string()),
        TimerServiceError::EmptyName => bad_request("a timer name must not be empty"),
        // The verb is illegal for the timer's state: well-formed, so not a 400,
        // and not fixed by retrying it unchanged.
        TimerServiceError::Transition(e) => {
            let title = match e {
                TimerTransitionError::Terminal(_) => "that timer is already finished",
                TimerTransitionError::NotFired => "that timer has not gone off yet",
                TimerTransitionError::AlreadyFired => {
                    "that timer already went off; dismiss it instead"
                }
            };
            problem(
                StatusCode::CONFLICT,
                ErrorCode::TimerInvalidTransition,
                title,
                None,
            )
        }
        // Lost the compare-and-set: retryable after a refresh, which is exactly
        // what makes it a different code from the transition error above.
        TimerServiceError::RaceLost => problem(
            StatusCode::CONFLICT,
            ErrorCode::TimerStale,
            "that timer changed while the request was in flight; refresh and retry",
            None,
        ),
        TimerServiceError::Storage(e) => {
            tracing::error!(error = %e, "timer storage failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "storage unavailable",
                None,
            )
        }
        TimerServiceError::Cancelled => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::ProviderUnavailable,
            "the request was cancelled",
            None,
        ),
    }
}

/// The timer scheduler: sweep, sleep, repeat (FR-33, ADR-023).
///
/// **The first pass is the restart sweep.** Anything that came due while the
/// process was not running is fired immediately and flagged `missed` — the
/// single most important behaviour in this feature, and the reason timers are
/// persisted at all.
///
/// After that the task is idle-by-default: it sleeps exactly as long as the next
/// fire time requires, wakes early only when [`TimerApi`] signals that the armed
/// set changed, and parks indefinitely when nothing is armed. No polling loop
/// (docs/09 §5), and everything is bounded by `shutdown` (invariant 4) — a
/// cancelled token ends the pass in flight, mid-sweep if necessary.
pub async fn run_scheduler(
    service: Arc<TimerService>,
    wake: Arc<Notify>,
    shutdown: CancellationToken,
) {
    tracing::info!("timer scheduler started; sweeping for missed alarms");
    while !shutdown.is_cancelled() {
        match service.fire_due(&shutdown).await {
            Ok(fired) => {
                for f in &fired {
                    tracing::info!(
                        timer_id = %f.timer.id(),
                        missed = f.missed,
                        alerted = f.alerted,
                        announced = f.announced,
                        "timer fired"
                    );
                    if !f.alerted {
                        // Worth a warning rather than a debug: the human may be
                        // relying on a sound they did not get.
                        tracing::warn!(
                            timer_id = %f.timer.id(),
                            "timer fired but no audible alert was played"
                        );
                    }
                }
            }
            Err(TimerServiceError::Cancelled) => break,
            Err(error) => {
                // A transient storage failure must not kill the scheduler — the
                // whole point of this task is that it is still here when the
                // alarm comes due. Back off and try again.
                tracing::error!(%error, "timer sweep failed; retrying");
                if sleep_or_stop(TIMER_RETRY_BACKOFF, &wake, &shutdown).await {
                    break;
                }
                continue;
            }
        }

        let sleep = match service.next_wakeup(&shutdown).await {
            Ok(Some(d)) => Some(d.max(MIN_SCHEDULER_SLEEP)),
            // Nothing armed: park on the notify until a timer is set.
            Ok(None) => None,
            Err(TimerServiceError::Cancelled) => break,
            Err(error) => {
                tracing::error!(%error, "timer wakeup calculation failed; retrying");
                Some(TIMER_RETRY_BACKOFF)
            }
        };

        let stop = match sleep {
            Some(d) => sleep_or_stop(d, &wake, &shutdown).await,
            None => {
                tokio::select! {
                    () = shutdown.cancelled() => true,
                    () = wake.notified() => false,
                }
            }
        };
        if stop {
            break;
        }
    }
    tracing::info!("timer scheduler stopped");
}

/// Backoff after a storage failure. Short enough that an alarm is at most this
/// late once the database recovers.
const TIMER_RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// Sleep for `d`, returning `true` if shutdown fired (stop) — an early wake
/// signal simply returns `false` so the caller re-sweeps.
async fn sleep_or_stop(d: Duration, wake: &Notify, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        () = shutdown.cancelled() => true,
        () = wake.notified() => false,
        () = tokio::time::sleep(d) => false,
    }
}
