//! The override layer and the durable character budget (F8.8, F8.11,
//! migration 0020), against a real Postgres.

use jarvis_application::ports::{SettingsStore, SpendLedger, VoiceOverrides};
use jarvis_domain::audit::AuditEvent;
use jarvis_infra::settings::{PgSettingsStore, PgSpendLedger, period_of};
use sqlx::PgPool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEVICE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn audit() -> AuditEvent {
    AuditEvent {
        occurred_at: SystemTime::now(),
        actor: format!("device:{DEVICE}"),
        event_type: "settings.voice.updated".into(),
        target: "settings:voice".into(),
        correlation_id: None,
        payload_json: r#"{"elevenlabsEnabled":true}"#.into(),
    }
}

/// An installation that has never been configured reports no overrides at all
/// — which is what lets `None` mean "whatever the config file says" rather
/// than "off".
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn an_unconfigured_installation_has_no_overrides(pool: PgPool) {
    let store = PgSettingsStore::new(pool);
    assert_eq!(
        store.voice_overrides().await.expect("reads"),
        VoiceOverrides::default()
    );
}

/// A change survives a restart, asserted through a **fresh store over the same
/// database** — a value returned by the object that just wrote it would prove
/// only that a field was set.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_voice_override_survives_a_restart(pool: PgPool) {
    let before = PgSettingsStore::new(pool.clone());
    before
        .set_voice_overrides(
            &VoiceOverrides {
                wake_word: Some("hey jarvis".into()),
                elevenlabs_enabled: Some(true),
            },
            &DEVICE.parse().expect("device id"),
            SystemTime::now(),
            &audit(),
        )
        .await
        .expect("writes");

    let after = PgSettingsStore::new(pool);
    assert_eq!(
        after.voice_overrides().await.expect("reads"),
        VoiceOverrides {
            wake_word: Some("hey jarvis".into()),
            elevenlabs_enabled: Some(true),
        }
    );
}

/// A partial update changes only what it names.
///
/// This is what stops two shell tabs from overwriting each other: the request
/// says what to change, not what everything is. Without it, toggling
/// ElevenLabs in one tab would silently revert a wake word set in another.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_partial_update_leaves_the_other_setting_alone(pool: PgPool) {
    let store = PgSettingsStore::new(pool);
    let device = DEVICE.parse().expect("device id");

    store
        .set_voice_overrides(
            &VoiceOverrides {
                wake_word: Some("alexa".into()),
                elevenlabs_enabled: Some(true),
            },
            &device,
            SystemTime::now(),
            &audit(),
        )
        .await
        .expect("writes");

    let after = store
        .set_voice_overrides(
            &VoiceOverrides {
                wake_word: None,
                elevenlabs_enabled: Some(false),
            },
            &device,
            SystemTime::now(),
            &audit(),
        )
        .await
        .expect("writes");

    assert_eq!(after.wake_word.as_deref(), Some("alexa"), "untouched");
    assert_eq!(after.elevenlabs_enabled, Some(false), "changed");
}

/// The spend accumulates and is readable back — the half of ADR-033 §5 that
/// through F8.11 lived in an `AtomicU64` and vanished on every restart.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn spend_accumulates_and_survives_a_restart(pool: PgPool) {
    let before = PgSpendLedger::new(pool.clone());
    assert_eq!(before.spent().await.expect("reads"), 0);

    assert_eq!(before.reserve(100).await.expect("reserves"), 100);
    assert_eq!(
        before.reserve(50).await.expect("reserves"),
        150,
        "the ledger returns the running total, so two callers cannot both \
         reserve against the same figure"
    );

    let after = PgSpendLedger::new(pool);
    assert_eq!(after.spent().await.expect("reads"), 150);
}

/// A refund gives budget back, and can never drive the period negative — the
/// column's CHECK would reject that, and a refund must not be able to fail.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_refund_never_goes_below_zero(pool: PgPool) {
    let ledger = PgSpendLedger::new(pool);
    ledger.reserve(100).await.expect("reserves");
    ledger.refund(40).await.expect("refunds");
    assert_eq!(ledger.spent().await.expect("reads"), 60);

    ledger.refund(1_000).await.expect("over-refunds");
    assert_eq!(ledger.spent().await.expect("reads"), 0);
}

/// The budget is *monthly*, and the rollover needs no scheduled job: a new
/// period is a new key, so last month's spend neither carries forward nor has
/// to be zeroed by anything.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn the_budget_rolls_over_with_the_month(pool: PgPool) {
    // 2026-08-15 and 2026-09-15, as fixed instants.
    let august = UNIX_EPOCH + Duration::from_secs(1_786_000_000);
    let september = august + Duration::from_secs(31 * 24 * 60 * 60);
    assert_ne!(
        period_of(august),
        period_of(september),
        "the fixture must actually straddle a month boundary"
    );

    let in_august = PgSpendLedger::with_clock(pool.clone(), move || august);
    in_august.reserve(90_000).await.expect("reserves");

    let in_september = PgSpendLedger::with_clock(pool, move || september);
    assert_eq!(
        in_september.spent().await.expect("reads"),
        0,
        "a new month starts with a full budget"
    );
    // And August is still readable rather than destroyed.
    assert_eq!(in_august.spent().await.expect("reads"), 90_000);
}
