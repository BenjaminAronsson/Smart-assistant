//! F3b.7: the timer surface through the production router (FR-33, ADR-023).
//!
//! An in-memory `TimerStore` plus recording alert/announcer doubles drive the
//! full middleware path. Covered: set → list → dismiss end to end, the audible
//! alert firing with **no voice pipeline at all**, missed alarms swept on
//! startup, `fire` being unreachable as a request verb, illegal transitions and
//! stale decisions mapping to their own codes, and auth required.

mod identity_fixture;
use identity_fixture::InMemoryIdentityStore;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use jarvis_application::orchestrator::Clock;
use jarvis_application::ports::{
    AlertError, AlertPlayer, AnnouncementOutcome, Announcer, DomainEventRecord, RepositoryError,
    TimerStore,
};
use jarvis_application::timers::TimerService;
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::TimerId;
use jarvis_domain::timers::{Timer, TimerKind, TimerName, TimerState};
use jarvisd::api::{AppState, Wiring, router_with};
use jarvisd::auth::AuthState;
use jarvisd::timers::{TimerApi, TimerEncoder};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const T0: u64 = 1_700_000_000;

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

// --- fakes --------------------------------------------------------------

#[derive(Default)]
struct FakeTimerStore {
    rows: Mutex<Vec<Timer>>,
    events: Mutex<Vec<DomainEventRecord>>,
}

impl FakeTimerStore {
    fn seeded(timers: Vec<Timer>) -> Arc<Self> {
        Arc::new(Self {
            rows: Mutex::new(timers),
            events: Mutex::default(),
        })
    }
}

#[async_trait::async_trait]
impl TimerStore for FakeTimerStore {
    async fn create(&self, timer: &Timer, _audit: &AuditEvent) -> Result<(), RepositoryError> {
        self.rows.lock().unwrap().push(timer.clone());
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
        _audit: &AuditEvent,
        event: Option<&DomainEventRecord>,
    ) -> Result<bool, RepositoryError> {
        let mut rows = self.rows.lock().unwrap();
        let Some(row) = rows.iter_mut().find(|t| t.id() == next.id()) else {
            return Ok(false);
        };
        if row.state() != expected {
            return Ok(false);
        }
        *row = next.clone();
        if let Some(event) = event {
            self.events.lock().unwrap().push(event.clone());
        }
        Ok(true)
    }
}

#[derive(Default)]
struct RecordingAlert {
    plays: Mutex<u32>,
}

#[async_trait::async_trait]
impl AlertPlayer for RecordingAlert {
    async fn play(
        &self,
        _timer: &jarvis_domain::timers::Timer,
        _cancel: CancellationToken,
    ) -> Result<(), AlertError> {
        *self.plays.lock().unwrap() += 1;
        Ok(())
    }
}

/// The pre-M5 state of the world: nothing can speak.
#[derive(Default)]
struct VoicelessAnnouncer {
    lines: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Announcer for VoicelessAnnouncer {
    async fn announce(
        &self,
        text: &str,
        _target: Option<&jarvis_domain::ids::DeviceId>,
        _cancel: CancellationToken,
    ) -> AnnouncementOutcome {
        self.lines.lock().unwrap().push(text.to_owned());
        AnnouncementOutcome::Unavailable
    }
}

struct FixedClock(SystemTime);

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

// --- harness ------------------------------------------------------------

struct Harness {
    app: Router,
    token: String,
    store: Arc<FakeTimerStore>,
    service: Arc<TimerService>,
    alert: Arc<RecordingAlert>,
    announcer: Arc<VoicelessAnnouncer>,
}

impl Harness {
    async fn get(&self, path: &str) -> (StatusCode, serde_json::Value) {
        self.send(Request::get(path).body(Body::empty()).unwrap())
            .await
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        self.send(
            Request::post(path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    async fn send(&self, mut request: Request<Body>) -> (StatusCode, serde_json::Value) {
        request.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {}", self.token).parse().unwrap(),
        );
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }
}

async fn harness(seed: Vec<Timer>) -> Harness {
    let identity = Arc::new(InMemoryIdentityStore::default());
    let auth = AuthState::bootstrap(identity).await;
    let code = auth.current_pairing_code().unwrap();

    let store = FakeTimerStore::seeded(seed);
    let alert = Arc::new(RecordingAlert::default());
    let announcer = Arc::new(VoicelessAnnouncer::default());
    let service = Arc::new(TimerService::new(
        store.clone(),
        alert.clone(),
        announcer.clone(),
        Arc::new(TimerEncoder),
        Arc::new(FixedClock(at(T0))),
    ));
    let api = TimerApi::new(service.clone(), Arc::new(Notify::new()));

    let app = router_with(
        AppState::new().with_auth(auth),
        Wiring {
            timers: Some(api),
            ..Wiring::default()
        },
    );
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/pair")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"pairingCode":"{code}","deviceName":"laptop"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let token = body["deviceToken"].as_str().unwrap().to_owned();

    Harness {
        app,
        token,
        store,
        service,
        alert,
        announcer,
    }
}

fn overdue(id: &str, name: &str) -> Timer {
    Timer::from_parts(
        id.parse().unwrap(),
        TimerName::new(name).unwrap(),
        TimerKind::Countdown {
            duration: Duration::from_secs(600),
        },
        TimerState::Pending,
        at(T0 - 3_600),
        at(T0 - 4_200),
        // Unattributed: the restart-sweep fixture predates room attribution.
        None,
    )
}

// --- tests --------------------------------------------------------------

#[tokio::test]
async fn set_list_and_cancel_a_timer_end_to_end() {
    let h = harness(Vec::new()).await;

    let (status, created) = h
        .post(
            "/api/v1/timers",
            serde_json::json!({ "name": "pasta timer", "kind": "countdown", "durationSecs": 600 }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["name"], "pasta timer");
    assert_eq!(created["state"], "pending");
    assert_eq!(created["remainingSecs"], 600);
    assert_eq!(created["durationSecs"], 600);
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, listed) = h.get("/api/v1/timers").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["timers"].as_array().unwrap().len(), 1);
    assert!(listed["now"].as_str().unwrap().starts_with("20"));

    // The human changes their mind before it rings.
    let (status, acted) = h
        .post(
            &format!("/api/v1/timers/{id}/action"),
            serde_json::json!({ "action": "cancel" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{acted}");
    assert_eq!(acted["timer"]["state"], "cancelled");

    let (_, listed) = h.get("/api/v1/timers").await;
    assert!(
        listed["timers"].as_array().unwrap().is_empty(),
        "a cancelled timer leaves the outstanding list"
    );
    // …and it never rings, however long the scheduler runs.
    assert!(
        h.service
            .fire_due(&CancellationToken::new())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(*h.alert.plays.lock().unwrap(), 0);
}

#[tokio::test]
async fn a_ringing_timer_is_dismissed_from_the_card() {
    let h = harness(vec![overdue("01ARZ3NDEKTSV4RRFFQ69G5FAV", "pasta timer")]).await;
    h.service.fire_due(&CancellationToken::new()).await.unwrap();

    let (status, acted) = h
        .post(
            "/api/v1/timers/01ARZ3NDEKTSV4RRFFQ69G5FAV/action",
            serde_json::json!({ "action": "dismiss" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{acted}");
    assert_eq!(acted["timer"]["state"], "dismissed");

    let (_, listed) = h.get("/api/v1/timers").await;
    assert!(listed["timers"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_missed_alarm_is_swept_on_startup_and_sounds_without_any_voice_pipeline() {
    // THE feature behaviour (ADR-023), through the real service: a timer that
    // came due while the daemon was down fires on the first sweep, the tone
    // plays even though nothing can speak, and the persisted event says missed.
    let h = harness(vec![overdue("01ARZ3NDEKTSV4RRFFQ69G5FAV", "pasta timer")]).await;

    let fired = h.service.fire_due(&CancellationToken::new()).await.unwrap();
    assert_eq!(fired.len(), 1);
    assert!(fired[0].missed);
    assert!(fired[0].alerted, "the alarm sounded with no TTS available");
    assert!(!fired[0].announced);
    assert_eq!(*h.alert.plays.lock().unwrap(), 1);
    assert_eq!(
        h.announcer.lines.lock().unwrap().clone(),
        vec!["Missed while I was offline — pasta timer is up"]
    );

    // The durable event carries the whole card plus the missed notice.
    let events = h.store.events.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "timer.fired");
    let payload: serde_json::Value = serde_json::from_str(&events[0].payload_json).unwrap();
    assert_eq!(payload["missed"], true);
    assert_eq!(payload["timer"]["state"], "fired");
    assert_eq!(payload["timer"]["name"], "pasta timer");

    // …and the card is still listed until the human answers it.
    let (_, listed) = h.get("/api/v1/timers").await;
    assert_eq!(listed["timers"][0]["state"], "fired");
    assert!(
        listed["timers"][0].get("remainingSecs").is_none(),
        "a ringing timer shows no countdown"
    );
}

#[tokio::test]
async fn firing_is_not_a_verb_a_request_may_use() {
    let h = harness(vec![overdue("01ARZ3NDEKTSV4RRFFQ69G5FAV", "pasta timer")]).await;
    for verb in ["fire", "ring", "rm -rf /"] {
        let (status, body) = h
            .post(
                "/api/v1/timers/01ARZ3NDEKTSV4RRFFQ69G5FAV/action",
                serde_json::json!({ "action": verb }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{verb} must be refused");
        assert_eq!(body["code"], "validation.failed");
    }
    assert_eq!(
        *h.alert.plays.lock().unwrap(),
        0,
        "no request may make a noise"
    );
}

#[tokio::test]
async fn an_illegal_transition_is_its_own_code_and_never_a_silent_success() {
    let h = harness(vec![overdue("01ARZ3NDEKTSV4RRFFQ69G5FAV", "pasta timer")]).await;
    // Snoozing something that has not rung is a reschedule, not a snooze.
    let (status, body) = h
        .post(
            "/api/v1/timers/01ARZ3NDEKTSV4RRFFQ69G5FAV/action",
            serde_json::json!({ "action": "snooze" }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "timer.invalid_transition");

    // Once it has rung, cancelling is refused too — it is dismissed instead.
    h.service.fire_due(&CancellationToken::new()).await.unwrap();
    let (status, body) = h
        .post(
            "/api/v1/timers/01ARZ3NDEKTSV4RRFFQ69G5FAV/action",
            serde_json::json!({ "action": "cancel" }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "timer.invalid_transition");

    // And a snooze now works, with an explicit duration.
    let (status, body) = h
        .post(
            "/api/v1/timers/01ARZ3NDEKTSV4RRFFQ69G5FAV/action",
            serde_json::json!({ "action": "snooze", "snoozeSecs": 300 }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["timer"]["state"], "snoozed");
    assert_eq!(body["timer"]["remainingSecs"], 300);
}

#[tokio::test]
async fn malformed_create_requests_are_refused_before_anything_is_stored() {
    let h = harness(Vec::new()).await;
    for (label, body) in [
        (
            "countdown without a duration",
            serde_json::json!({ "kind": "countdown", "fireAt": "2030-01-01T00:00:00Z" }),
        ),
        (
            "reminder without a note",
            serde_json::json!({ "kind": "reminder", "durationSecs": 60 }),
        ),
        (
            "both a duration and a time",
            serde_json::json!({ "kind": "alarm", "durationSecs": 60, "fireAt": "2030-01-01T00:00:00Z" }),
        ),
        ("neither", serde_json::json!({ "kind": "alarm" })),
        (
            "unparseable time",
            serde_json::json!({ "kind": "alarm", "fireAt": "next tuesday" }),
        ),
        (
            "an empty name",
            serde_json::json!({ "kind": "alarm", "name": "  ", "fireAt": "2030-01-01T00:00:00Z" }),
        ),
    ] {
        let (status, response) = h.post("/api/v1/timers", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: {response}");
    }
    let (_, listed) = h.get("/api/v1/timers").await;
    assert!(listed["timers"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_timer_may_not_be_set_absurdly_far_out() {
    let h = harness(Vec::new()).await;
    let (status, body) = h
        .post(
            "/api/v1/timers",
            serde_json::json!({ "kind": "alarm", "fireAt": "3030-01-01T00:00:00Z" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn an_unknown_timer_is_a_404_and_a_bad_id_is_a_400() {
    let h = harness(Vec::new()).await;
    let (status, body) = h
        .post(
            "/api/v1/timers/01ARZ3NDEKTSV4RRFFQ69G5FAV/action",
            serde_json::json!({ "action": "cancel" }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "resource.not_found");

    let (status, _) = h
        .post(
            "/api/v1/timers/not-a-ulid/action",
            serde_json::json!({ "action": "cancel" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_reminder_carries_its_note_to_the_card_and_the_spoken_line() {
    let h = harness(Vec::new()).await;
    let (status, created) = h
        .post(
            "/api/v1/timers",
            serde_json::json!({
                "name": "Mom",
                "kind": "reminder",
                "durationSecs": 0,
                "note": "call Mom"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["note"], "call Mom");

    let fired = h.service.fire_due(&CancellationToken::new()).await.unwrap();
    assert_eq!(fired.len(), 1);
    assert_eq!(
        h.announcer.lines.lock().unwrap().clone(),
        vec!["Reminder — call Mom"]
    );
}

#[tokio::test]
async fn the_timer_surface_requires_authentication() {
    let h = harness(Vec::new()).await;
    let response = h
        .app
        .clone()
        .oneshot(Request::get("/api/v1/timers").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
