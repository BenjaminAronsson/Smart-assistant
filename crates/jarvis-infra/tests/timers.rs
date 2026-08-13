//! F3b.7 acceptance — timer persistence against live Postgres (FR-33, ADR-023,
//! migration 0011, invariant #6).
//!
//! The behaviour this file exists to prove is the one ADR-023 calls out: **a
//! timer whose moment passes while jarvisd is stopped is not lost**. Everything
//! else here guards the same guarantee from another side — the fire is a
//! compare-and-set so it happens exactly once, its audit row and its
//! `timer.fired` outbox event commit with it or not at all, and the database
//! itself refuses to resurrect a finished timer.

use std::time::{Duration, SystemTime};

use jarvis_application::ports::{DomainEventRecord, RepositoryError, TimerStore};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::{DeviceId, TimerId};
use jarvis_domain::timers::{MISSED_GRACE, Timer, TimerKind, TimerName, TimerNote, TimerState};
use jarvis_infra::audit::verify_chain;
use jarvis_infra::timers::PgTimerStore;
use sqlx::PgPool;

const PASTA: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const BREAD: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
const MOM: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB2";

fn id(raw: &str) -> TimerId {
    raw.parse().expect("valid test ulid")
}

/// A fixed "now" well clear of the epoch so backdating stays representable.
fn t0() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn countdown(raw_id: &str, name: &str, secs_ahead: i64) -> Timer {
    let now = t0();
    let fire_at = if secs_ahead >= 0 {
        now + Duration::from_secs(secs_ahead.unsigned_abs())
    } else {
        now - Duration::from_secs(secs_ahead.unsigned_abs())
    };
    Timer::from_parts(
        id(raw_id),
        TimerName::new(name).unwrap(),
        TimerKind::Countdown {
            duration: Duration::from_secs(600),
        },
        TimerState::Pending,
        fire_at,
        now - Duration::from_secs(600),
        // Unattributed: these fixtures predate room attribution (F8.5).
        None,
    )
}

fn audit_for(timer: &Timer, event_type: &str) -> AuditEvent {
    AuditEvent {
        occurred_at: t0(),
        actor: "device:01ARZ3NDEKTSV4RRFFQ69G5FB3".to_owned(),
        event_type: event_type.to_owned(),
        target: format!("timer:{}", timer.id()),
        correlation_id: None,
        payload_json: format!(
            r#"{{"kind":"{}","state":"{}","missed":false}}"#,
            timer.kind().as_str(),
            timer.state().as_str()
        ),
    }
}

fn fired_event(timer: &Timer, missed: bool) -> DomainEventRecord {
    DomainEventRecord {
        event_type: "timer.fired".to_owned(),
        payload_json: serde_json::json!({
            "timer": { "id": timer.id().as_str(), "name": timer.name().as_str() },
            "missed": missed,
        })
        .to_string(),
    }
}

// ---------------------------------------------------------------------------
// THE test
// ---------------------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_timer_that_came_due_while_the_daemon_was_down_is_still_there_on_restart(pool: PgPool) {
    // Set a timer, then simulate the process dying: nothing else happens. A new
    // PgTimerStore (a "restarted" daemon) must find it still armed and overdue —
    // this is the whole reason timers are persisted (ADR-023, NFR-05).
    let before_crash = PgTimerStore::new(pool.clone());
    let pasta = countdown(PASTA, "pasta timer", -3_600); // an hour overdue
    let bread = countdown(BREAD, "bread timer", 900); // still to come
    before_crash
        .create(&pasta, &audit_for(&pasta, "timer.set"))
        .await
        .unwrap();
    before_crash
        .create(&bread, &audit_for(&bread, "timer.set"))
        .await
        .unwrap();
    drop(before_crash);

    let after_restart = PgTimerStore::new(pool.clone());
    let live = after_restart.list_live().await.unwrap();
    assert_eq!(live.len(), 2, "both timers survived the restart");
    assert_eq!(
        live[0].id(),
        pasta.id(),
        "the overdue one sorts first — the oldest missed alarm is announced first"
    );

    let recovered = &live[0];
    assert_eq!(recovered.state(), TimerState::Pending);
    assert!(
        recovered.is_due_at(t0()),
        "a timer whose moment passed while we were down is still due"
    );
    assert!(
        recovered.is_missed_at(t0(), MISSED_GRACE),
        "and is flagged MISSED, not presented as if it had just rung"
    );
    // The whole aggregate round-tripped, not just the id.
    assert_eq!(recovered, &pasta);

    // Firing it on restart writes the state, the durable event and the audit row
    // together, and it is then no longer due.
    let fired = recovered.fire().unwrap();
    assert!(
        after_restart
            .apply(
                &fired,
                TimerState::Pending,
                &audit_for(&fired, "timer.fired"),
                Some(&fired_event(&fired, true)),
            )
            .await
            .unwrap()
    );

    let live = after_restart.list_live().await.unwrap();
    let rung = live.iter().find(|t| t.id() == pasta.id()).unwrap();
    assert_eq!(rung.state(), TimerState::Fired);
    assert!(
        !rung.is_due_at(t0()),
        "a timer that rang is not due again — no repeat alarm"
    );
    assert!(
        rung.state().is_live(),
        "but it stays listed until the human answers it"
    );

    let events = outbox_types(&pool).await;
    assert_eq!(events, vec!["timer.fired"]);
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        verify_chain(&mut conn).await.unwrap(),
        3,
        "two sets + one fire, all in the hash chain"
    );
}

// ---------------------------------------------------------------------------
// exactly-once firing
// ---------------------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_lost_compare_and_set_writes_absolutely_nothing(pool: PgPool) {
    // Two sweeps overlap (a restart sweep and the first scheduler wakeup). The
    // second one must not ring, must not audit, and must not emit an event —
    // otherwise the HUD replays a second alarm that never sounded.
    let store = PgTimerStore::new(pool.clone());
    let pasta = countdown(PASTA, "pasta timer", -3_600);
    store
        .create(&pasta, &audit_for(&pasta, "timer.set"))
        .await
        .unwrap();

    let fired = pasta.fire().unwrap();
    assert!(
        store
            .apply(
                &fired,
                TimerState::Pending,
                &audit_for(&fired, "timer.fired"),
                Some(&fired_event(&fired, true)),
            )
            .await
            .unwrap(),
        "the first sweep wins"
    );
    assert!(
        !store
            .apply(
                &fired,
                TimerState::Pending,
                &audit_for(&fired, "timer.fired"),
                Some(&fired_event(&fired, true)),
            )
            .await
            .unwrap(),
        "the second sweep loses the race and reports it"
    );

    assert_eq!(
        outbox_types(&pool).await,
        vec!["timer.fired"],
        "exactly one durable fire event"
    );
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        verify_chain(&mut conn).await.unwrap(),
        2,
        "one set + one fire — the losing sweep left no audit row at all"
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn an_unknown_timer_is_a_lost_cas_not_an_error(pool: PgPool) {
    let store = PgTimerStore::new(pool);
    let ghost = countdown(PASTA, "ghost", -60).fire().unwrap();
    assert!(
        !store
            .apply(
                &ghost,
                TimerState::Pending,
                &audit_for(&ghost, "timer.fired"),
                None
            )
            .await
            .unwrap()
    );
}

// ---------------------------------------------------------------------------
// lifecycle round-trips
// ---------------------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn every_kind_round_trips_including_its_kind_specific_payload(pool: PgPool) {
    let store = PgTimerStore::new(pool);
    let alarm = Timer::from_parts(
        id(BREAD),
        TimerName::new("wake up").unwrap(),
        TimerKind::Alarm,
        TimerState::Pending,
        t0() + Duration::from_secs(3_600),
        t0(),
        // Unattributed: these fixtures predate room attribution (F8.5).
        None,
    );
    let reminder = Timer::from_parts(
        id(MOM),
        TimerName::new("Mom").unwrap(),
        TimerKind::Reminder {
            note: TimerNote::new("call Mom").unwrap(),
        },
        TimerState::Pending,
        t0() + Duration::from_secs(7_200),
        t0(),
        // Unattributed: these fixtures predate room attribution (F8.5).
        None,
    );
    let pasta = countdown(PASTA, "pasta timer", 600);
    for t in [&alarm, &reminder, &pasta] {
        store.create(t, &audit_for(t, "timer.set")).await.unwrap();
    }

    assert_eq!(store.get(alarm.id()).await.unwrap().as_ref(), Some(&alarm));
    assert_eq!(
        store.get(reminder.id()).await.unwrap().as_ref(),
        Some(&reminder),
        "the reminder's note survived — it is what gets announced"
    );
    assert_eq!(store.get(pasta.id()).await.unwrap().as_ref(), Some(&pasta));
    assert_eq!(
        store.get(&id("01ARZ3NDEKTSV4RRFFQ69G5FB9")).await.unwrap(),
        None
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_duplicate_id_is_a_conflict(pool: PgPool) {
    let store = PgTimerStore::new(pool);
    let pasta = countdown(PASTA, "pasta timer", 600);
    store
        .create(&pasta, &audit_for(&pasta, "timer.set"))
        .await
        .unwrap();
    assert!(matches!(
        store.create(&pasta, &audit_for(&pasta, "timer.set")).await,
        Err(RepositoryError::Conflict(_))
    ));
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_snooze_moves_the_fire_time_and_the_timer_rings_again(pool: PgPool) {
    let store = PgTimerStore::new(pool);
    let pasta = countdown(PASTA, "pasta timer", -30);
    store
        .create(&pasta, &audit_for(&pasta, "timer.set"))
        .await
        .unwrap();

    let fired = pasta.fire().unwrap();
    store
        .apply(
            &fired,
            TimerState::Pending,
            &audit_for(&fired, "timer.fired"),
            None,
        )
        .await
        .unwrap();
    let snoozed = fired.snooze(t0(), Duration::from_secs(300)).unwrap();
    assert!(
        store
            .apply(
                &snoozed,
                TimerState::Fired,
                &audit_for(&snoozed, "timer.snooze"),
                None,
            )
            .await
            .unwrap()
    );

    let stored = store.get(pasta.id()).await.unwrap().unwrap();
    assert_eq!(stored.state(), TimerState::Snoozed);
    assert_eq!(stored.fire_at(), t0() + Duration::from_secs(300));
    assert!(!stored.is_due_at(t0()));
    assert!(stored.is_due_at(t0() + Duration::from_secs(300)));
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn terminal_timers_leave_the_live_list_and_never_come_back(pool: PgPool) {
    let store = PgTimerStore::new(pool);
    let pasta = countdown(PASTA, "pasta timer", 600);
    store
        .create(&pasta, &audit_for(&pasta, "timer.set"))
        .await
        .unwrap();

    let cancelled = pasta.cancel().unwrap();
    assert!(
        store
            .apply(
                &cancelled,
                TimerState::Pending,
                &audit_for(&cancelled, "timer.cancel"),
                None,
            )
            .await
            .unwrap()
    );
    assert!(
        store.list_live().await.unwrap().is_empty(),
        "a cancelled timer is not outstanding work — and a restart will not ring it"
    );
    // It is still readable by id: "what did I have set?" is a real question.
    assert_eq!(
        store.get(pasta.id()).await.unwrap().unwrap().state(),
        TimerState::Cancelled
    );
}

// ---------------------------------------------------------------------------
// the database's own guards (defence in depth, migration 0011)
// ---------------------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn the_database_refuses_to_resurrect_a_finished_timer(pool: PgPool) {
    // Even with the domain transition table bypassed entirely — a raw UPDATE,
    // as an application bug or a hand-run query would do — a dismissed timer
    // cannot be set back to a state that fires.
    let store = PgTimerStore::new(pool.clone());
    let pasta = countdown(PASTA, "pasta timer", -30);
    store
        .create(&pasta, &audit_for(&pasta, "timer.set"))
        .await
        .unwrap();
    let fired = pasta.fire().unwrap();
    store
        .apply(
            &fired,
            TimerState::Pending,
            &audit_for(&fired, "timer.fired"),
            None,
        )
        .await
        .unwrap();
    let dismissed = fired.dismiss().unwrap();
    store
        .apply(
            &dismissed,
            TimerState::Fired,
            &audit_for(&dismissed, "timer.dismiss"),
            None,
        )
        .await
        .unwrap();

    let resurrect = sqlx::query("UPDATE timers.timers SET state = 'pending' WHERE id = $1")
        .bind(PASTA)
        .execute(&pool)
        .await;
    assert!(
        resurrect.is_err(),
        "the DB must refuse to re-arm a dismissed timer"
    );

    let rename = sqlx::query("UPDATE timers.timers SET name = 'other' WHERE id = $1")
        .bind(PASTA)
        .execute(&pool)
        .await;
    assert!(rename.is_err(), "identity columns are frozen");
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn the_database_refuses_a_kind_without_its_payload(pool: PgPool) {
    // The CHECK constraints, not just the writer: a countdown with no duration
    // (or an alarm carrying a note) would read back as a different timer.
    let bad_countdown = sqlx::query(
        "INSERT INTO timers.timers
           (id, name, kind, duration_secs, note, state, fire_at, created_at, updated_at)
         VALUES ($1, 'x', 'countdown', NULL, NULL, 'pending', now(), now(), now())",
    )
    .bind(PASTA)
    .execute(&pool)
    .await;
    assert!(bad_countdown.is_err());

    let alarm_with_note = sqlx::query(
        "INSERT INTO timers.timers
           (id, name, kind, duration_secs, note, state, fire_at, created_at, updated_at)
         VALUES ($1, 'x', 'alarm', NULL, 'surprise', 'pending', now(), now(), now())",
    )
    .bind(BREAD)
    .execute(&pool)
    .await;
    assert!(alarm_with_note.is_err());

    let unknown_state = sqlx::query(
        "INSERT INTO timers.timers
           (id, name, kind, duration_secs, note, state, fire_at, created_at, updated_at)
         VALUES ($1, 'x', 'alarm', NULL, NULL, 'ringing', now(), now(), now())",
    )
    .bind(MOM)
    .execute(&pool)
    .await;
    assert!(unknown_state.is_err(), "the state vocabulary is closed");
}

// ---------------------------------------------------------------------------

async fn outbox_types(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT event_type FROM outbox.outbox_events ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// F8.5's named acceptance: "a timer set on one node rings on it **after a
/// restart**". That is a claim about the *row*, not about memory — the origin
/// has to come back out of Postgres.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_timers_room_survives_a_restart(pool: PgPool) {
    let store = PgTimerStore::new(pool.clone());
    let kitchen: DeviceId = "01ARZ3NDEKTSV4RRFFQ69G5FB2".parse().expect("device id");

    let timer = Timer::from_parts(
        id(BREAD),
        TimerName::new("pasta timer").unwrap(),
        TimerKind::Countdown {
            duration: Duration::from_secs(600),
        },
        TimerState::Pending,
        t0() + Duration::from_secs(600),
        t0(),
        Some(kitchen.clone()),
    );
    store
        .create(&timer, &audit_for(&timer, "timer.set"))
        .await
        .expect("creates");

    // A new store over the same pool is the restart: nothing in memory carries
    // over, so anything that comes back came out of the row.
    let after_restart = PgTimerStore::new(pool);
    let live = after_restart.list_live().await.expect("lists");
    assert_eq!(live.len(), 1);
    assert_eq!(
        live[0].origin_device(),
        Some(&kitchen),
        "the room a timer was set in must survive a restart"
    );
}

/// An unattributed timer round-trips as unattributed — the absence is stored,
/// not defaulted into somebody's room.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_timer_with_no_room_reads_back_with_no_room(pool: PgPool) {
    let store = PgTimerStore::new(pool);
    let timer = Timer::from_parts(
        id(BREAD),
        TimerName::new("pasta timer").unwrap(),
        TimerKind::Countdown {
            duration: Duration::from_secs(600),
        },
        TimerState::Pending,
        t0() + Duration::from_secs(600),
        t0(),
        None,
    );
    store
        .create(&timer, &audit_for(&timer, "timer.set"))
        .await
        .expect("creates");

    let live = store.list_live().await.expect("lists");
    assert_eq!(live[0].origin_device(), None);
}
