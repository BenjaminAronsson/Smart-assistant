//! F7.1 (FR-19): device classes, listing, and revocation against real
//! Postgres. Each test runs in an isolated throwaway database created by
//! `#[sqlx::test]` with the workspace migration stream applied.

use jarvis_application::ports::{IdentityStore, NodePairOutcome, RevocationOutcome};
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
        public_key: None,
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

/// **The guard actually takes the lock it depends on (security-auditor S4).**
///
/// The safety of the last-owner rule rests on reading the active-owner set
/// `FOR UPDATE`, so two revocations serialise instead of both observing "there
/// is another owner" and both committing. Racing two calls with `tokio::join!`
/// does NOT prove that — it passes with the locking removed, because the
/// dangerous interleaving is not schedulable on demand.
///
/// So prove the mechanism directly: hold the owner rows locked in this test's
/// own transaction, then revoke a **node** — whose own row this test has not
/// touched. If the port reads the owner set without locking it, that call
/// sails through; if it locks, it blocks until we roll back. Mutation-checked:
/// deleting `FOR UPDATE` from the query fails this test.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn the_last_owner_guard_locks_the_owner_set_before_deciding(pool: PgPool) {
    let store = seed(&pool).await;
    let node: DeviceId = NODE.parse().expect("ulid");

    let mut holder = pool.begin().await.expect("begin");
    sqlx::query(
        "SELECT id FROM identity.devices \
         WHERE device_class = 'owner-ui' AND revoked_at IS NULL ORDER BY id FOR UPDATE",
    )
    .fetch_all(&mut *holder)
    .await
    .expect("hold the owner set");

    let blocked = tokio::time::timeout(
        Duration::from_millis(750),
        store.revoke_device(&node, None, ts(40), &audit("device.revoked", &node)),
    )
    .await;
    assert!(
        blocked.is_err(),
        "revoke_device decided without waiting for the owner-set lock"
    );

    holder.rollback().await.expect("release");

    // And once released, the same call succeeds — the block was the lock, not
    // a broken query.
    assert_eq!(
        store
            .revoke_device(&node, None, ts(41), &audit("device.revoked", &node))
            .await
            .expect("query"),
        RevocationOutcome::Revoked
    );
}

/// Two revocations of the last two owner devices, issued together: exactly one
/// must win and an owner device must still be standing. Weaker than the lock
/// probe above — the dangerous interleaving is not schedulable on demand, so
/// this passes even with the locking removed — but it exercises the real pair
/// of calls end to end.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn two_concurrent_revocations_cannot_orphan_the_owner(pool: PgPool) {
    let store = std::sync::Arc::new(PgIdentityStore::new(pool.clone()));
    let first = device(
        OWNER,
        OWNER_USER,
        "laptop",
        DeviceClass::OwnerUi,
        "hash-owner",
    );
    store
        .pair_device("owner", &first, &audit("device.paired", &first.id))
        .await
        .expect("pairs");
    const SECOND: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA4";
    sqlx::query(
        "INSERT INTO identity.devices (id, user_id, name, token_hash, scopes, device_class, created_at) \
         VALUES ($1, $2, 'desktop', 'hash-owner-2', ARRAY['ui'], 'owner-ui', now())",
    )
    .bind(SECOND)
    .bind(OWNER_USER)
    .execute(&pool)
    .await
    .expect("seed second owner");

    let a: DeviceId = OWNER.parse().expect("ulid");
    let b: DeviceId = SECOND.parse().expect("ulid");
    let (left, right) = tokio::join!(
        {
            let store = store.clone();
            let a = a.clone();
            async move {
                store
                    .revoke_device(&a, Some("race a"), ts(30), &audit("device.revoked", &a))
                    .await
            }
        },
        {
            let store = store.clone();
            let b = b.clone();
            async move {
                store
                    .revoke_device(&b, Some("race b"), ts(31), &audit("device.revoked", &b))
                    .await
            }
        }
    );

    let outcomes = [left.expect("query a"), right.expect("query b")];
    let revoked = outcomes
        .iter()
        .filter(|o| **o == RevocationOutcome::Revoked)
        .count();
    let refused = outcomes
        .iter()
        .filter(|o| **o == RevocationOutcome::LastOwnerDevice)
        .count();
    assert_eq!(
        (revoked, refused),
        (1, 1),
        "exactly one revocation must win: {outcomes:?}"
    );

    // The invariant that actually matters: an owner device is still standing.
    let survivors: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM identity.devices WHERE device_class = 'owner-ui' AND revoked_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(survivors, 1, "the owner must never be locked out");
}

/// F7.2 (FR-19): a node joins the **owner's** user, records its public key,
/// and cannot pair into a house with no owner.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_node_pairs_onto_the_owners_user_and_keeps_its_key(pool: PgPool) {
    let store = PgIdentityStore::new(pool.clone());
    let owner = device(
        OWNER,
        OWNER_USER,
        "laptop",
        DeviceClass::OwnerUi,
        "hash-owner",
    );

    // No owner yet: nothing to join.
    let mut node = device(
        NODE,
        OWNER_USER,
        "kitchen",
        DeviceClass::RoomNode,
        "hash-node",
    );
    node.public_key = Some("dGVzdC1rZXktb25l".to_owned());
    assert_eq!(
        store
            .pair_node_device(&node, &audit("device.paired", &node.id))
            .await
            .expect("query"),
        NodePairOutcome::NoOwner
    );

    store
        .pair_device("owner", &owner, &audit("device.paired", &owner.id))
        .await
        .expect("pairs the owner");
    assert_eq!(
        store
            .pair_node_device(&node, &audit("device.paired", &node.id))
            .await
            .expect("query"),
        NodePairOutcome::Paired
    );

    let stored = store
        .find_active_device_by_token_hash("hash-node")
        .await
        .expect("query")
        .expect("node found");
    assert_eq!(stored.class, DeviceClass::RoomNode);
    assert_eq!(stored.public_key.as_deref(), Some("dGVzdC1rZXktb25l"));
    assert_eq!(stored.user_id, owner.user_id, "the node joins the owner");
    assert_eq!(
        stored.effective_scopes(),
        vec!["display-agent", "voice-capture"],
        "authority still comes from the class, not from anything the node said"
    );

    // The key is the identity: a second device cannot claim it.
    let mut twin = device(
        "01ARZ3NDEKTSV4RRFFQ69G5FA8",
        OWNER_USER,
        "impostor",
        DeviceClass::RoomNode,
        "hash-twin",
    );
    twin.public_key = stored.public_key.clone();
    assert_eq!(
        store
            .pair_node_device(&twin, &audit("device.paired", &twin.id))
            .await
            .expect("query"),
        NodePairOutcome::KeyAlreadyPaired
    );
}

/// Once the owner's only device is revoked, the house stops accepting new
/// satellites — an attacker who revoked their way in must not then be able to
/// enroll hardware.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_node_cannot_pair_against_a_revoked_owner(pool: PgPool) {
    let store = seed(&pool).await;
    sqlx::query("UPDATE identity.devices SET revoked_at = now() WHERE device_class = 'owner-ui'")
        .execute(&pool)
        .await
        .expect("revoke the owner behind the guard");

    let mut node = device(
        "01ARZ3NDEKTSV4RRFFQ69G5FA7",
        OWNER_USER,
        "late node",
        DeviceClass::VoiceNode,
        "hash-late",
    );
    node.public_key = Some("bGF0ZS1rZXk=".to_owned());
    assert_eq!(
        store
            .pair_node_device(&node, &audit("device.paired", &node.id))
            .await
            .expect("query"),
        NodePairOutcome::NoOwner
    );
}
