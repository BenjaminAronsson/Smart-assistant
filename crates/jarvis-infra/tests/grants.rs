//! F2.4 acceptance — the execution-grant lifecycle against live Postgres
//! (docs/06 §4, invariant #1, skill `policy-grants` "grant lifecycle" table):
//! mint · validate · consume · replay · args-mismatch · wrong-binding · expire ·
//! missing. Proves the DB row is the source of truth, single-use is enforced by
//! the store (not a flag), and every lifecycle event lands transactionally in
//! the append-only audit chain (invariant #6, carry-forward CF-2).

use std::time::{Duration, SystemTime};

use jarvis_application::policy::{GrantBinding, GrantMinter, GrantValidator};
use jarvis_domain::grants::{ExecutionGrant, GrantError, GrantId, Sha256};
use jarvis_domain::ids::{DeviceId, RunId, UserId};
use jarvis_domain::policy::ResourcePattern;
use jarvis_domain::tools::{CanonicalValue as V, ToolId, ToolInvocation, ToolVersion};
use jarvis_infra::audit::verify_chain;
use jarvis_infra::grants::PgGrantStore;
use sqlx::PgPool;

const USER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const DEVICE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";
const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";

fn args() -> V {
    V::obj([("to", V::str("alice@example.com")), ("body", V::str("hi"))])
}

fn tool() -> ToolId {
    "message.send".parse().unwrap()
}

fn version() -> ToolVersion {
    ToolVersion::new(1, 0, 0)
}

fn binding() -> GrantBinding {
    GrantBinding {
        user_id: USER.parse::<UserId>().unwrap(),
        device_id: DEVICE.parse::<DeviceId>().unwrap(),
        run_id: RUN.parse::<RunId>().unwrap(),
        tool_id: tool(),
        tool_version: version(),
        arguments: args(),
        target_resource: "message:alice@example.com"
            .parse::<ResourcePattern>()
            .unwrap(),
        ttl: Duration::from_secs(300),
    }
}

/// The invocation the grant authorizes — same tool, version, and arguments.
fn matching_invocation() -> ToolInvocation {
    ToolInvocation {
        tool_id: tool(),
        tool_version: version(),
        arguments: args(),
    }
}

async fn consumed_at(pool: &PgPool, grant: &ExecutionGrant) -> Option<time::OffsetDateTime> {
    sqlx::query_scalar!(
        "SELECT consumed_at FROM tooling.grants WHERE grant_id = $1",
        grant.grant_id.to_string()
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn mint_then_validate_consumes_once_and_replay_is_rejected(pool: PgPool) {
    let store = PgGrantStore::new(pool.clone());

    let grant = store.mint(binding()).await;
    // Minted, unspent, and bound to real arguments (a hash, never the raw args).
    assert!(consumed_at(&pool, &grant).await.is_none());
    assert_eq!(grant.tool_id, tool());

    // First validation succeeds and consumes the grant.
    let now = SystemTime::now();
    store
        .validate(&grant, &matching_invocation(), now)
        .await
        .expect("valid grant executes");
    assert!(
        consumed_at(&pool, &grant).await.is_some(),
        "a validated grant is consumed"
    );

    // Replay of the very same call is rejected — single-use is enforced.
    assert_eq!(
        store.validate(&grant, &matching_invocation(), now).await,
        Err(GrantError::Consumed)
    );

    // mint + consume + reject all landed in the append-only chain, intact.
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(verify_chain(&mut conn).await.unwrap(), 3);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn concurrent_validation_consumes_exactly_once(pool: PgPool) {
    // The single-use guarantee under a real race, not just by analysis: two
    // validators hit the same grant at once. `SELECT ... FOR UPDATE` must let
    // exactly one consume it; the other blocks, then sees consumed_at and reports
    // Consumed. Anything else (two successes) would be a double-execution.
    let store_a = PgGrantStore::new(pool.clone());
    let store_b = PgGrantStore::new(pool.clone());
    let grant = store_a.mint(binding()).await;
    let now = SystemTime::now();
    let inv = matching_invocation();

    let (a, b) = tokio::join!(
        store_a.validate(&grant, &inv, now),
        store_b.validate(&grant, &inv, now),
    );

    let mut results = [a, b];
    results.sort_by_key(|r| r.is_err()); // Ok (false) sorts before Err (true).
    assert_eq!(results[0], Ok(()), "exactly one validation succeeds");
    assert_eq!(
        results[1],
        Err(GrantError::Consumed),
        "the racing validation is a replay"
    );
    assert!(consumed_at(&pool, &grant).await.is_some());
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn edited_arguments_fail_validation_and_do_not_consume(pool: PgPool) {
    let store = PgGrantStore::new(pool.clone());
    let grant = store.mint(binding()).await;

    // The model proposes a DIFFERENT recipient than was approved.
    let edited = ToolInvocation {
        tool_id: tool(),
        tool_version: version(),
        arguments: V::obj([
            ("to", V::str("mallory@evil.example")),
            ("body", V::str("hi")),
        ]),
    };
    assert_eq!(
        store.validate(&grant, &edited, SystemTime::now()).await,
        Err(GrantError::ArgsMismatch)
    );
    assert!(
        consumed_at(&pool, &grant).await.is_none(),
        "a rejected grant must remain spendable by its real call"
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_grant_bound_to_another_run_is_rejected(pool: PgPool) {
    let store = PgGrantStore::new(pool.clone());
    let grant = store.mint(binding()).await;

    // Tamper the in-memory grant to point at a different run.
    let mut tampered = grant.clone();
    tampered.run_id = "01ARZ3NDEKTSV4RRFFQ69G5FZZ".parse::<RunId>().unwrap();
    assert_eq!(
        store
            .validate(&tampered, &matching_invocation(), SystemTime::now())
            .await,
        Err(GrantError::WrongRun)
    );
    assert!(consumed_at(&pool, &grant).await.is_none());
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn an_expired_grant_is_rejected(pool: PgPool) {
    let store = PgGrantStore::new(pool.clone());
    let grant = store.mint(binding()).await;

    // A clock past the grant's expiry.
    let past_expiry = grant.expires_at + Duration::from_secs(1);
    assert_eq!(
        store
            .validate(&grant, &matching_invocation(), past_expiry)
            .await,
        Err(GrantError::Expired)
    );
    assert!(consumed_at(&pool, &grant).await.is_none());
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn an_unknown_grant_is_missing(pool: PgPool) {
    let store = PgGrantStore::new(pool.clone());
    // A fully fabricated grant that was never minted — nothing is in the table,
    // so the row lookup fails before any binding check.
    let grant = ExecutionGrant {
        grant_id: GrantId::from_bytes([0u8; 32]),
        user_id: USER.parse::<UserId>().unwrap(),
        device_id: DEVICE.parse::<DeviceId>().unwrap(),
        run_id: RUN.parse::<RunId>().unwrap(),
        tool_id: tool(),
        tool_version: version(),
        normalized_args_sha256: Sha256::from_bytes([0u8; 32]),
        target_resource: "message:alice@example.com"
            .parse::<ResourcePattern>()
            .unwrap(),
        expires_at: SystemTime::now() + Duration::from_secs(300),
        single_use: true,
    };
    assert_eq!(
        store
            .validate(&grant, &matching_invocation(), SystemTime::now())
            .await,
        Err(GrantError::Missing)
    );
}
