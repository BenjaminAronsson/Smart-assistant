//! F7.1 (FR-19): device classes, listing, and revocation against real
//! Postgres. Each test runs in an isolated throwaway database created by
//! `#[sqlx::test]` with the workspace migration stream applied.

use jarvis_application::ports::{IdentityStore, RevocationOutcome};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::identity::{Device, DeviceClass};
use jarvis_domain::ids::DeviceId;
use jarvis_infra::identity::PgIdentityStore;
use sqlx::PgPool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn ts(micros: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_micros(1_800_000_000_000_000 + micros)
}

fn device(id: &str, user: &str, name: &str, class: DeviceClass, hash: &str) -> Device {
    Device {
        id: id.parse().expect("ulid"),
        user_id: user.parse().expect("ulid"),
        name: name.to_owned(),
        token_hash: hash.to_owned(),
        class,
        created_at: ts(0),
        last_seen_at: None,
        revoked_at: None,
        revoked_reason: None,
    }
}

fn audit(event_type: &str, target: &DeviceId) -> AuditEvent {
    AuditEvent {
        occurred_at: ts(1),
        actor: "device:test".into(),
        event_type: event_type.to_owned(),
        target: format!("device:{target}"),
        correlation_id: None,
        payload_json: "{}".into(),
    }
}

const OWNER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA1";
const NODE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA2";
const OWNER_USER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";

/// Pair the owner, then attach a second device to the same user — the shape
/// F7.2's node-pairing route will produce.
async fn seed(pool: &PgPool) -> PgIdentityStore {
    let store = PgIdentityStore::new(pool.clone());
    let owner = device(
        OWNER,
        OWNER_USER,
        "laptop",
        DeviceClass::OwnerUi,
        "hash-owner",
    );
    store
        .pair_device("owner", &owner, &audit("device.paired", &owner.id))
        .await
        .expect("pairs");
    sqlx::query(
        "INSERT INTO identity.devices (id, user_id, name, token_hash, scopes, device_class, created_at) \
         VALUES ($1, $2, 'kitchen', 'hash-node', ARRAY['display-agent','voice-capture'], 'room-node', now())",
    )
    .bind(NODE)
    .bind(OWNER_USER)
    .execute(pool)
    .await
    .expect("seed node");
    store
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn pairing_records_the_class_and_authorization_reads_it_back(pool: PgPool) {
    let store = seed(&pool).await;

    let owner = store
        .find_active_device_by_token_hash("hash-owner")
        .await
        .expect("query")
        .expect("device found");
    assert_eq!(owner.class, DeviceClass::OwnerUi);
    assert!(
        owner
            .effective_scopes()
            .contains(&"home:control".to_owned())
    );

    let node = store
        .find_active_device_by_token_hash("hash-node")
        .await
        .expect("query")
        .expect("device found");
    assert_eq!(node.class, DeviceClass::RoomNode);
    assert_eq!(
        node.effective_scopes(),
        vec!["display-agent", "voice-capture"]
    );
}

/// The `scopes` column is a pairing-time snapshot, not authority. If it were
/// read back, a single UPDATE — a bad migration, a restore from a tampered
/// dump — would silently promote a kitchen screen to the owner's authority.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn stale_stored_scopes_do_not_widen_a_devices_authority(pool: PgPool) {
    let store = seed(&pool).await;
    sqlx::query("UPDATE identity.devices SET scopes = ARRAY['ui','home:control'] WHERE id = $1")
        .bind(NODE)
        .execute(&pool)
        .await
        .expect("tamper");

    let node = store
        .find_active_device_by_token_hash("hash-node")
        .await
        .expect("query")
        .expect("device found");
    assert_eq!(
        node.effective_scopes(),
        vec!["display-agent", "voice-capture"],
        "authority comes from device_class, never from the stored snapshot"
    );
}

/// A class this build does not know is not a device this build authenticates.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn an_unknown_stored_class_fails_closed(pool: PgPool) {
    let store = seed(&pool).await;
    sqlx::query("UPDATE identity.devices SET device_class = 'superuser' WHERE id = $1")
        .bind(NODE)
        .execute(&pool)
        .await
        .expect("tamper");

    let result = store.find_active_device_by_token_hash("hash-node").await;
    assert!(
        result.is_err(),
        "an unparseable class must error, never default to a class with authority"
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn revocation_is_transactional_idempotent_and_visible(pool: PgPool) {
    let store = seed(&pool).await;
    let node: DeviceId = NODE.parse().expect("ulid");

    let outcome = store
        .revoke_device(
            &node,
            Some("sold the screen"),
            ts(10),
            &audit("device.revoked", &node),
        )
        .await
        .expect("revokes");
    assert_eq!(outcome, RevocationOutcome::Revoked);

    // Gone from authentication…
    assert!(
        store
            .find_active_device_by_token_hash("hash-node")
            .await
            .expect("query")
            .is_none()
    );
    // …but still in the owner's list, with the reason.
    let listed = store.list_devices().await.expect("lists");
    let node_row = listed.iter().find(|d| d.id == node).expect("still listed");
    assert_eq!(node_row.revoked_reason.as_deref(), Some("sold the screen"));
    assert!(!node_row.is_active());

    // The audit row landed in the same transaction (invariant 6).
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit.audit_events WHERE event_type = 'device.revoked'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(audits, 1);

    // Idempotent, and no second audit row.
    let again = store
        .revoke_device(&node, None, ts(20), &audit("device.revoked", &node))
        .await
        .expect("second revoke");
    assert_eq!(again, RevocationOutcome::AlreadyRevoked);
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit.audit_events WHERE event_type = 'device.revoked'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(audits, 1, "an idempotent no-op writes no audit row");
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn the_last_owner_device_is_refused_but_a_replaced_one_is_not(pool: PgPool) {
    let store = seed(&pool).await;
    let owner: DeviceId = OWNER.parse().expect("ulid");

    assert_eq!(
        store
            .revoke_device(&owner, None, ts(10), &audit("device.revoked", &owner))
            .await
            .expect("query"),
        RevocationOutcome::LastOwnerDevice,
        "revoking the only owner device would leave nothing able to pair"
    );
    // Refused means refused: nothing was written.
    assert!(
        store
            .find_active_device_by_token_hash("hash-owner")
            .await
            .expect("query")
            .is_some()
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit.audit_events WHERE event_type = 'device.revoked'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(audits, 0);

    // Pair a replacement, and the old one becomes revocable.
    sqlx::query(
        "INSERT INTO identity.devices (id, user_id, name, token_hash, scopes, device_class, created_at) \
         VALUES ('01ARZ3NDEKTSV4RRFFQ69G5FA3', $1, 'new laptop', 'hash-owner-2', ARRAY['ui'], 'owner-ui', now())",
    )
    .bind(OWNER_USER)
    .execute(&pool)
    .await
    .expect("seed replacement");

    assert_eq!(
        store
            .revoke_device(&owner, None, ts(11), &audit("device.revoked", &owner))
            .await
            .expect("query"),
        RevocationOutcome::Revoked
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn revoking_an_unknown_device_is_not_found(pool: PgPool) {
    let store = seed(&pool).await;
    let ghost: DeviceId = "01ARZ3NDEKTSV4RRFFQ69G5FZ9".parse().expect("ulid");
    assert_eq!(
        store
            .revoke_device(&ghost, None, ts(10), &audit("device.revoked", &ghost))
            .await
            .expect("query"),
        RevocationOutcome::NotFound
    );
}

/// The schema refuses a reason without a revocation — the device list must
/// never show "revoked because: stolen" next to an active device.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_reason_without_a_revocation_is_rejected_by_the_schema(pool: PgPool) {
    seed(&pool).await;
    let result = sqlx::query("UPDATE identity.devices SET revoked_reason = 'stolen' WHERE id = $1")
        .bind(NODE)
        .execute(&pool)
        .await;
    assert!(result.is_err(), "check constraint holds");
}
