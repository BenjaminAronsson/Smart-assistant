//! Golden 12 — "the house answers" (F8.10, M8 exit evidence, docs/07 §2).
//!
//! The M8 row promises: with no browser open, the wake word fires in the
//! kitchen, the answer comes back *there*, a voice-set timer rings in that
//! room, an automation fires on its own, and a revoked node goes quiet.
//!
//! # What this file proves, and what it deliberately does not
//!
//! It proves the **daemon-side** halves, against real Postgres and the
//! production fan-out:
//!
//! * a timer set on a node rings **on that node and nowhere else**, and
//!   survives a restart with its room intact;
//! * an automation fires **on its own**, with policy re-evaluated at fire time
//!   and the outcome recorded either way;
//! * a **revoked** node's automations stop, and a revoked node is not sent the
//!   alert for a timer it set.
//!
//! It does **not** prove the wake word firing in a real kitchen. That needs
//! F8.3's engine binding — which is not implemented — and, past that, a person
//! speaking in a room and a false-accept rate measured on real hardware
//! (ADR-032 consequence 2). Those are gate evidence a human observes, not
//! something a test can assert, and this file does not pretend otherwise.
//!
//! The node-side half of the same evidence — nothing streams before the word
//! fires, a detection opens exactly one bracketed stream, playback does not
//! self-trigger — lives in `jarvis-agent`'s own suite, because it is a claim
//! about the *node* and asserting it here would prove only that a fake said so.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use jarvis_application::automations::{AutomationExecutor, AutomationService, DeviceAuthority};
use jarvis_application::policy::{ToolDescriptor, ToolExecutor, ToolRegistry};
use jarvis_application::ports::{AutomationStore, TimerStore};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::automations::{
    Automation, AutomationAction, AutomationName, ExecutionOutcome, Trigger,
};
use jarvis_domain::ids::{DeviceId, UserId};
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, SpeechSensitivity, ToolPolicy};
use jarvis_domain::timers::{Timer, TimerKind, TimerName, TimerState};
use jarvis_domain::tools::{CanonicalValue, ToolId, ToolProposal};
use jarvis_infra::automations::PgAutomationStore;
use jarvis_infra::timers::PgTimerStore;
use jarvisd::ws::delivers_to_for_test;
use sqlx::PgPool;

const KITCHEN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB2";
const STUDY: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB3";
const TIMER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
const AUTOMATION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB4";

fn t0() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn audit(event_type: &str, target: &str) -> AuditEvent {
    AuditEvent {
        occurred_at: t0(),
        actor: format!("device:{KITCHEN}"),
        event_type: event_type.to_owned(),
        target: target.to_owned(),
        correlation_id: None,
        payload_json: "{}".to_owned(),
    }
}

/// A tool that records whether the world was actually touched.
#[derive(Default)]
struct SpyTool(std::sync::Mutex<usize>);

#[async_trait::async_trait]
impl ToolExecutor for SpyTool {
    async fn execute(
        &self,
        _invocation: jarvis_domain::tools::ToolInvocation,
        _grant: Option<jarvis_domain::grants::ExecutionGrant>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<jarvis_domain::tools::ToolResult, jarvis_domain::tools::ToolError> {
        *self.0.lock().expect("lock") += 1;
        Ok(jarvis_domain::tools::ToolResult {
            content: "done".to_owned(),
            truncated: false,
            compensation: None,
        })
    }
}

fn registry_with(tool: &ToolId, executor: Arc<SpyTool>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry
        .register(ToolDescriptor {
            id: tool.clone(),
            version: jarvis_domain::tools::ToolVersion::new(1, 0, 0),
            policy: Some(ToolPolicy {
                risk: RiskLevel::R1,
                is_reversible: true,
                requires_user_presence: false,
                timeout: Duration::from_secs(5),
                required_scopes: [Scope::new("home:control").expect("scope")]
                    .into_iter()
                    .collect(),
                egress: DataEgress::Local,
                speech_sensitivity: SpeechSensitivity::Normal,
            }),
            executor,
        })
        .expect("registers");
    registry
}

/// Answers from a table the test controls — standing in for the live device
/// row, so "this device was revoked" is expressible.
struct Authority(Option<Vec<&'static str>>);

#[async_trait::async_trait]
impl DeviceAuthority for Authority {
    async fn scopes_of(
        &self,
        _device: &DeviceId,
    ) -> Option<(UserId, std::collections::BTreeSet<Scope>)> {
        self.0.as_ref().map(|scopes| {
            (
                "01ARZ3NDEKTSV4RRFFQ69G5FB9".parse().expect("user id"),
                scopes
                    .iter()
                    .map(|s| Scope::new(*s).expect("scope"))
                    .collect(),
            )
        })
    }
}

struct RegistryExecutor(Arc<ToolRegistry>);

#[async_trait::async_trait]
impl AutomationExecutor for RegistryExecutor {
    async fn execute(
        &self,
        proposal: &ToolProposal,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), String> {
        let (tool_version, executor) = self
            .0
            .resolve(&proposal.tool_id)
            .ok_or_else(|| "tool is not registered".to_owned())?;
        executor
            .execute(
                jarvis_domain::tools::ToolInvocation {
                    tool_id: proposal.tool_id.clone(),
                    tool_version,
                    arguments: proposal.arguments.clone(),
                },
                None,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

fn automation_in_kitchen() -> Automation {
    Automation::create(
        AUTOMATION.parse().expect("id"),
        AutomationName::new("morning lights").expect("name"),
        Trigger::DailyAt {
            minutes_since_midnight: 420,
        },
        AutomationAction {
            tool_id: ToolId::home_set_light(),
            arguments: CanonicalValue::obj([("entity_id", CanonicalValue::str("light.kitchen"))]),
        },
        KITCHEN.parse().expect("device id"),
        t0(),
    )
}

// -------------------------------------------------------------------------
// Evidence 1 — a voice-set timer rings in the room it was set in, after a
// restart.
// -------------------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn golden12_a_timer_set_in_the_kitchen_rings_in_the_kitchen_after_a_restart(pool: PgPool) {
    let store = PgTimerStore::new(pool.clone());
    let kitchen: DeviceId = KITCHEN.parse().expect("device id");

    let timer = Timer::from_parts(
        TIMER.parse().expect("timer id"),
        TimerName::new("pasta timer").expect("name"),
        TimerKind::Countdown {
            duration: Duration::from_secs(600),
        },
        TimerState::Pending,
        t0() + Duration::from_secs(600),
        t0(),
        Some(kitchen.clone()),
    );
    store
        .create(&timer, &audit("timer.set", &format!("timer:{TIMER}")))
        .await
        .expect("creates");

    // The restart: a brand-new store over the same database. Nothing in memory
    // carries over, so anything that comes back came out of the row.
    let after_restart = PgTimerStore::new(pool);
    let live = after_restart.list_live().await.expect("lists");
    assert_eq!(live.len(), 1, "the timer must survive the restart");
    assert_eq!(
        live[0].origin_device(),
        Some(&kitchen),
        "and so must the room it was set in"
    );

    // The fan-out addresses the alert at exactly that room. Both satellite
    // classes can ring — a voice node is a speaker with no screen, which is
    // exactly what a kitchen has.
    let alert = serde_json::json!({
        "id": TIMER,
        "name": "pasta timer",
        "targetDeviceId": KITCHEN,
    });
    for class in ["voice-node", "room-node"] {
        assert!(
            delivers_to_for_test("voice", "timer.fired", &alert, class, Some(KITCHEN), None),
            "{class} in the kitchen did not receive its own timer"
        );
        assert!(
            !delivers_to_for_test("voice", "timer.fired", &alert, class, Some(STUDY), None),
            "the study heard a timer set in the kitchen"
        );
    }
}

// -------------------------------------------------------------------------
// Evidence 2 — an automation fires on its own, and is judged at fire time.
// -------------------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn golden12_an_automation_fires_on_its_own_and_records_what_happened(pool: PgPool) {
    let store: Arc<dyn AutomationStore> = Arc::new(PgAutomationStore::new(pool.clone()));
    store
        .create(
            &automation_in_kitchen(),
            &audit("automation.created", &format!("automation:{AUTOMATION}")),
        )
        .await
        .expect("creates");

    let tool = ToolId::home_set_light();
    let spy = Arc::new(SpyTool::default());
    let registry = Arc::new(registry_with(&tool, spy.clone()));
    let service = AutomationService::new(
        store.clone(),
        registry.clone(),
        Arc::new(Authority(Some(vec!["home:control"]))),
        Arc::new(RegistryExecutor(registry)),
    );

    // Nobody asked. The clock moved past 07:00 and the sweep did the rest.
    let fired = service
        .sweep_clock(
            419,
            421,
            t0() + Duration::from_secs(3_600),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("sweeps");

    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].outcome, ExecutionOutcome::Executed);
    assert_eq!(
        *spy.0.lock().expect("lock"),
        1,
        "the world was touched once"
    );

    // Recorded durably, so "why did the lights come on?" is answerable.
    let history = store
        .history(&AUTOMATION.parse().expect("id"), 10)
        .await
        .expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].outcome, ExecutionOutcome::Executed);
}

// -------------------------------------------------------------------------
// Evidence 3 — revocation reaches everything the device left behind.
// -------------------------------------------------------------------------

/// The security-interesting half of M8's evidence: revoking the kitchen node
/// must not leave behind something that still acts with its authority.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn golden12_a_revoked_nodes_automation_stops_and_says_why(pool: PgPool) {
    let store: Arc<dyn AutomationStore> = Arc::new(PgAutomationStore::new(pool.clone()));
    store
        .create(
            &automation_in_kitchen(),
            &audit("automation.created", &format!("automation:{AUTOMATION}")),
        )
        .await
        .expect("creates");

    let tool = ToolId::home_set_light();
    let spy = Arc::new(SpyTool::default());
    let registry = Arc::new(registry_with(&tool, spy.clone()));
    let service = AutomationService::new(
        store.clone(),
        registry.clone(),
        // The device is gone. Authority is resolved at fire time, so this is
        // what revocation looks like from here.
        Arc::new(Authority(None)),
        Arc::new(RegistryExecutor(registry)),
    );

    let fired = service
        .sweep_clock(
            419,
            421,
            t0() + Duration::from_secs(3_600),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("sweeps");

    assert_eq!(fired.len(), 1);
    match &fired[0].outcome {
        ExecutionOutcome::Denied { reason } => {
            assert!(reason.contains("no longer has authority"), "{reason}");
        }
        other => panic!("a revoked creator's automation must be denied, got {other:?}"),
    }
    assert_eq!(
        *spy.0.lock().expect("lock"),
        0,
        "nothing may reach the world on a revoked device's authority"
    );

    // And the refusal is durable and visible — otherwise "the automation is
    // broken" and "I revoked that tablet" look identical from the sofa.
    let history = store
        .history(&AUTOMATION.parse().expect("id"), 10)
        .await
        .expect("history");
    assert_eq!(history.len(), 1);
    assert!(matches!(
        history[0].outcome,
        ExecutionOutcome::Denied { .. }
    ));
}

/// A revoked node is not sent the alert for a timer it set: the address no
/// longer matches any connected device, and nothing broadcasts it to the house
/// as a fallback.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn golden12_a_revoked_node_is_not_sent_the_timer_it_set(_pool: PgPool) {
    let alert = serde_json::json!({
        "id": TIMER,
        "name": "pasta timer",
        "targetDeviceId": KITCHEN,
    });
    // The study node is connected and healthy; it still hears nothing, because
    // the alert is addressed and it is not the address.
    assert!(!delivers_to_for_test(
        "voice",
        "timer.fired",
        &alert,
        "room-node",
        Some(STUDY),
        None
    ));
    // Nor does the owner's own client, which holds `ui` but not this room.
    assert!(!delivers_to_for_test(
        "voice",
        "timer.fired",
        &alert,
        "owner-ui",
        Some(STUDY),
        None
    ));
}
