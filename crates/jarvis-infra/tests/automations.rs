//! Automation persistence against real Postgres (F8.6, migration 0018).
//!
//! The claims that matter here are structural: an automation stores no
//! authority, its history is append-only, and a firing stamps the rate limit in
//! the same transaction that records it.

use std::time::{Duration, SystemTime};

use jarvis_application::ports::AutomationStore;
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::automations::{
    Automation, AutomationAction, AutomationExecution, AutomationName, ExecutionOutcome, Trigger,
};
use jarvis_domain::ids::AutomationId;
use jarvis_domain::tools::{CanonicalValue, ToolId};
use jarvis_infra::automations::PgAutomationStore;
use sqlx::PgPool;

const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
const CREATOR: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn t0() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn id(raw: &str) -> AutomationId {
    raw.parse().expect("automation id")
}

fn audit_event() -> AuditEvent {
    AuditEvent {
        occurred_at: t0(),
        actor: format!("device:{CREATOR}"),
        event_type: "automation.created".into(),
        target: format!("automation:{ID}"),
        correlation_id: None,
        payload_json: serde_json::json!({"triggerKind": "daily_at"}).to_string(),
    }
}

fn automation(trigger: Trigger) -> Automation {
    Automation::create(
        id(ID),
        AutomationName::new("evening lights").expect("name"),
        trigger,
        AutomationAction {
            tool_id: ToolId::home_set_light(),
            arguments: CanonicalValue::obj([
                ("entity_id", CanonicalValue::str("light.kitchen")),
                ("state", CanonicalValue::str("on")),
            ]),
        },
        CREATOR.parse().expect("device id"),
        t0(),
    )
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn an_automation_round_trips_with_its_trigger_and_arguments(pool: PgPool) {
    let store = PgAutomationStore::new(pool);
    let original = automation(Trigger::DailyAt {
        minutes_since_midnight: 420,
    });
    store
        .create(&original, &audit_event())
        .await
        .expect("creates");

    let all = store.list_all().await.expect("lists");
    assert_eq!(all.len(), 1);
    assert_eq!(
        all[0], original,
        "the row must rebuild the exact automation"
    );
    // And crucially: the creator is stored, so authority can be resolved fresh.
    assert_eq!(all[0].created_by().as_str(), CREATOR);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_state_trigger_round_trips_too(pool: PgPool) {
    let store = PgAutomationStore::new(pool);
    let original = automation(Trigger::HomeAssistantState {
        entity_id: "person.owner".into(),
        state: "home".into(),
    });
    store
        .create(&original, &audit_event())
        .await
        .expect("creates");

    let all = store.list_all().await.expect("lists");
    assert_eq!(all[0].trigger(), original.trigger());
}

/// The scheduler sweeps only what is enabled — a disabled automation must not
/// even be a candidate.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_disabled_automation_leaves_the_scheduler_sweep(pool: PgPool) {
    let store = PgAutomationStore::new(pool);
    store
        .create(
            &automation(Trigger::DailyAt {
                minutes_since_midnight: 420,
            }),
            &audit_event(),
        )
        .await
        .expect("creates");

    assert_eq!(store.list_enabled().await.expect("lists").len(), 1);
    store.set_enabled(&id(ID), false).await.expect("disables");
    assert!(store.list_enabled().await.expect("lists").is_empty());
    // Still listed for the settings surface, though — disabling is not deleting.
    assert_eq!(store.list_all().await.expect("lists").len(), 1);

    store.set_enabled(&id(ID), true).await.expect("re-enables");
    assert_eq!(store.list_enabled().await.expect("lists").len(), 1);
}

/// A denial is the most important row in the history table: "it ran and
/// nothing happened" and "it was refused" look identical from the sofa.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_denial_is_recorded_with_its_reason_and_stamps_the_rate_limit(pool: PgPool) {
    let store = PgAutomationStore::new(pool);
    store
        .create(
            &automation(Trigger::DailyAt {
                minutes_since_midnight: 420,
            }),
            &audit_event(),
        )
        .await
        .expect("creates");

    store
        .record_execution(&AutomationExecution {
            automation_id: id(ID),
            occurred_at: t0() + Duration::from_secs(60),
            outcome: ExecutionOutcome::Denied {
                reason: "the device that created this automation no longer has authority".into(),
            },
        })
        .await
        .expect("records");

    let history = store.history(&id(ID), 10).await.expect("history");
    assert_eq!(history.len(), 1);
    match &history[0].outcome {
        ExecutionOutcome::Denied { reason } => {
            assert!(reason.contains("no longer has authority"), "{reason}")
        }
        other => panic!("expected a denial, got {other:?}"),
    }

    // The same transaction stamped the rate limit, so a flapping trigger cannot
    // fire again immediately.
    let stored = &store.list_all().await.expect("lists")[0];
    assert!(
        stored.last_fired_at().is_some(),
        "recording a firing must stamp the rate limit"
    );
    assert!(!stored.may_fire_at(t0() + Duration::from_secs(61)));
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn history_comes_back_newest_first_and_bounded(pool: PgPool) {
    let store = PgAutomationStore::new(pool);
    store
        .create(
            &automation(Trigger::DailyAt {
                minutes_since_midnight: 420,
            }),
            &audit_event(),
        )
        .await
        .expect("creates");

    for minute in 1..=5 {
        store
            .record_execution(&AutomationExecution {
                automation_id: id(ID),
                occurred_at: t0() + Duration::from_secs(60 * minute),
                outcome: ExecutionOutcome::Executed,
            })
            .await
            .expect("records");
    }

    let history = store.history(&id(ID), 3).await.expect("history");
    assert_eq!(history.len(), 3, "the limit is honoured");
    assert!(
        history[0].occurred_at > history[1].occurred_at,
        "newest first"
    );
}

/// An execution record that could be rewritten is not a record (invariant 6's
/// spirit). The database refuses, not just the application.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn execution_history_cannot_be_rewritten(pool: PgPool) {
    let store = PgAutomationStore::new(pool.clone());
    store
        .create(
            &automation(Trigger::DailyAt {
                minutes_since_midnight: 420,
            }),
            &audit_event(),
        )
        .await
        .expect("creates");
    store
        .record_execution(&AutomationExecution {
            automation_id: id(ID),
            occurred_at: t0() + Duration::from_secs(60),
            outcome: ExecutionOutcome::Denied {
                reason: "missing scope".into(),
            },
        })
        .await
        .expect("records");

    let rewritten = sqlx::query("UPDATE automations.executions SET outcome = 'executed'")
        .execute(&pool)
        .await;
    assert!(
        rewritten.is_err(),
        "a denial must not be editable into a success"
    );

    let deleted = sqlx::query("DELETE FROM automations.executions")
        .execute(&pool)
        .await;
    assert!(deleted.is_err(), "history must not be deletable");
}
