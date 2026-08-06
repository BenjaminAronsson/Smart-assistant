//! Memory persistence acceptance tests against live Postgres (FR-16,
//! migrations 0013/0014, invariant #6, docs/02 §7 "Forget removes derived
//! embeddings too"). `PgMemoryStore` backs four ports: `MemoryStore`
//! (create/get/list/replace/forget), `EmbeddedMemoryStore`
//! (create_embedded/replace_embedded — atomic memory+embedding+audit),
//! `MemoryContextStore` (record_context, writing the provenance table added
//! by migration 0014), and `MemoryRetriever` (retrieve). Nothing here
//! existed before this file.

use std::time::{Duration, SystemTime};

use jarvis_application::ports::{
    EmbeddedMemory, EmbeddedMemoryStore, MemoryContextStore, MemoryContextUse, MemoryRetriever,
    MemoryStore, RepositoryError,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::{MemoryId, MessageId, RunId, SessionId, UserId};
use jarvis_domain::location::Sensitivity;
use jarvis_domain::memory::{Memory, MemoryLayer, MemoryScope, MemorySource, RetentionRule};
use jarvis_infra::audit::verify_chain;
use jarvis_infra::memory::PgMemoryStore;
use sqlx::{PgPool, Row};

const USER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER_USER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const MEM_A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";
const MEM_B: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
const MEM_C: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB2";
const MSG: &str = "01ARZ3NDEKTSV4RRFFQ69G5FC0";
const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FC1";
const RUN_A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FD0";
const ACTOR: &str = "device:01ARZ3NDEKTSV4RRFFQ69G5FE0";

fn t0() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

/// A fixed instant far past `t0()` but also far past the database's real
/// wall clock at test time — `get`/`list` filter live rows on
/// `expires_at > now()` using Postgres's actual clock, not `t0()`. Whole
/// seconds only, so it round-trips through `timestamptz` (microsecond
/// precision) exactly.
fn far_future() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000)
}

fn uid(raw: &str) -> UserId {
    raw.parse().expect("valid test ulid")
}

fn mid(raw: &str) -> MemoryId {
    raw.parse().expect("valid test ulid")
}

fn rid(raw: &str) -> RunId {
    raw.parse().expect("valid test ulid")
}

#[allow(clippy::too_many_arguments)]
fn memory_at(
    id: &str,
    user: &str,
    layer: MemoryLayer,
    text: &str,
    source: MemorySource,
    scope: MemoryScope,
    retention: RetentionRule,
    confidence: f32,
    sensitivity: Sensitivity,
    pinned: bool,
    now: SystemTime,
) -> Memory {
    Memory::new(
        mid(id),
        uid(user),
        layer,
        text.to_owned(),
        source,
        scope,
        retention,
        confidence,
        sensitivity,
        pinned,
        now,
    )
    .expect("valid test memory")
}

fn memory(id: &str, text: &str) -> Memory {
    memory_at(
        id,
        USER,
        MemoryLayer::Semantic,
        text,
        MemorySource::Explicit,
        MemoryScope::User,
        RetentionRule::UntilForgotten,
        0.8,
        Sensitivity::Normal,
        false,
        t0(),
    )
}

fn audit_for(memory: &Memory, event_type: &str) -> AuditEvent {
    AuditEvent {
        occurred_at: memory.updated_at,
        actor: ACTOR.to_owned(),
        event_type: event_type.to_owned(),
        target: format!("memory:{}", memory.id.as_str()),
        correlation_id: None,
        payload_json: format!(r#"{{"memoryId":"{}"}}"#, memory.id.as_str()),
    }
}

fn embedding(model: &str, seed: f32) -> EmbeddedMemory {
    EmbeddedMemory {
        model_id: model.to_owned(),
        dimensions: 384,
        embedding: (0..384).map(|i| seed + (i as f32) * 0.001).collect(),
    }
}

async fn audit_types(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT event_type FROM audit.audit_events ORDER BY seq ASC")
        .fetch_all(pool)
        .await
        .expect("audit rows")
}

async fn embedding_count(pool: &PgPool, id: &MemoryId) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM memory.embeddings WHERE memory_id = $1")
        .bind(id.as_str())
        .fetch_one(pool)
        .await
        .expect("embedding count")
}

/// Dynamic (non-macro) query so this file needs no `.sqlx` offline-cache
/// entry — the `rank` column is new (migration 0014) and any mismatch
/// surfaces as a runtime error here rather than a compile failure.
async fn context_rows(pool: &PgPool) -> Vec<(String, String, String, i32, f32)> {
    sqlx::query(
        "SELECT user_id, run_id, memory_id, rank, similarity \
         FROM memory.context_provenance ORDER BY run_id, memory_id",
    )
    .fetch_all(pool)
    .await
    .expect("context rows")
    .into_iter()
    .map(|row| {
        (
            row.try_get::<String, _>("user_id").unwrap(),
            row.try_get::<String, _>("run_id").unwrap(),
            row.try_get::<String, _>("memory_id").unwrap(),
            row.try_get::<i32, _>("rank").unwrap(),
            row.try_get::<f32, _>("similarity").unwrap(),
        )
    })
    .collect()
}

// ---------------------------------------------------------------------------
// MemoryStore: create / get / list / replace / forget
// ---------------------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn create_then_get_round_trips_every_field(pool: PgPool) {
    let store = PgMemoryStore::new(pool.clone());
    let item = memory_at(
        MEM_A,
        USER,
        MemoryLayer::Working,
        "Benjamin prefers oat milk",
        MemorySource::Message(MSG.parse::<MessageId>().expect("valid test ulid")),
        MemoryScope::Session(SESSION.parse::<SessionId>().expect("valid test ulid")),
        RetentionRule::ExpiresAt(far_future()),
        0.6,
        Sensitivity::Sensitive,
        true,
        t0(),
    );
    store
        .create(&item, &audit_for(&item, "memory.created"))
        .await
        .unwrap();

    let fetched = store.get(&uid(USER), &mid(MEM_A)).await.unwrap();
    assert_eq!(
        fetched.as_ref(),
        Some(&item),
        "scope/retention/source/sensitivity/pinned all round-trip, not just the text"
    );

    assert_eq!(
        store.get(&uid(OTHER_USER), &mid(MEM_A)).await.unwrap(),
        None,
        "a memory is not readable through the wrong owner"
    );
    assert_eq!(store.get(&uid(USER), &mid(MEM_B)).await.unwrap(), None);

    assert_eq!(audit_types(&pool).await, vec!["memory.created"]);
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(verify_chain(&mut conn).await.unwrap(), 1);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn list_filters_by_layer_and_query_and_respects_limit(pool: PgPool) {
    let store = PgMemoryStore::new(pool.clone());
    let working_a = memory_at(
        MEM_A,
        USER,
        MemoryLayer::Working,
        "likes oat milk",
        MemorySource::Explicit,
        MemoryScope::User,
        RetentionRule::UntilForgotten,
        0.8,
        Sensitivity::Normal,
        false,
        t0(),
    );
    let working_b = memory_at(
        MEM_B,
        USER,
        MemoryLayer::Working,
        "allergic to peanuts",
        MemorySource::Explicit,
        MemoryScope::User,
        RetentionRule::UntilForgotten,
        0.8,
        Sensitivity::Normal,
        false,
        t0() + Duration::from_secs(1),
    );
    let semantic = memory_at(
        MEM_C,
        USER,
        MemoryLayer::Semantic,
        "the capital of France is Paris",
        MemorySource::Explicit,
        MemoryScope::User,
        RetentionRule::UntilForgotten,
        0.8,
        Sensitivity::Normal,
        false,
        t0() + Duration::from_secs(2),
    );
    for item in [&working_a, &working_b, &semantic] {
        store
            .create(item, &audit_for(item, "memory.created"))
            .await
            .unwrap();
    }

    let working_only = store
        .list(&uid(USER), Some(MemoryLayer::Working), None, 10)
        .await
        .unwrap();
    assert_eq!(
        working_only
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec![MEM_B, MEM_A],
        "newest first, and the semantic memory is excluded by the layer filter"
    );

    let query_hit = store
        .list(&uid(USER), None, Some("peanuts"), 10)
        .await
        .unwrap();
    assert_eq!(query_hit.len(), 1);
    assert_eq!(query_hit[0].id.as_str(), MEM_B);

    let limited = store.list(&uid(USER), None, None, 2).await.unwrap();
    assert_eq!(limited.len(), 2, "limit is honoured");
    assert_eq!(
        limited.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec![MEM_C, MEM_B],
        "limit keeps only the most recently updated"
    );

    assert!(
        store
            .list(&uid(OTHER_USER), None, None, 10)
            .await
            .unwrap()
            .is_empty(),
        "list is owner-scoped"
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn replace_updates_mutable_fields_and_conflicts_when_absent_or_foreign(pool: PgPool) {
    let store = PgMemoryStore::new(pool.clone());
    let original = memory(MEM_A, "likes tea");
    store
        .create(&original, &audit_for(&original, "memory.created"))
        .await
        .unwrap();

    let mut updated = memory_at(
        MEM_A,
        USER,
        MemoryLayer::Semantic,
        "likes coffee now",
        MemorySource::Explicit,
        MemoryScope::User,
        RetentionRule::UntilForgotten,
        0.5,
        Sensitivity::Normal,
        true,
        original.created_at,
    );
    updated.updated_at = t0() + Duration::from_secs(60);
    store
        .replace(&updated, &audit_for(&updated, "memory.replaced"))
        .await
        .unwrap();

    let fetched = store.get(&uid(USER), &mid(MEM_A)).await.unwrap().unwrap();
    assert_eq!(fetched.text, "likes coffee now");
    assert_eq!(fetched.confidence, 0.5);
    assert!(fetched.pinned);
    assert_eq!(fetched.updated_at, updated.updated_at, "updated_at moved");
    assert_eq!(
        fetched.created_at, original.created_at,
        "created_at is immutable"
    );

    // Replacing a memory that was never created is a conflict, not a silent upsert.
    let ghost = memory(MEM_B, "never existed");
    assert!(matches!(
        store
            .replace(&ghost, &audit_for(&ghost, "memory.replaced"))
            .await,
        Err(RepositoryError::Conflict(_))
    ));

    // The row exists, but not reachable through a different owner's user_id.
    let mut wrong_owner = updated.clone();
    wrong_owner.user_id = uid(OTHER_USER);
    assert!(matches!(
        store
            .replace(&wrong_owner, &audit_for(&wrong_owner, "memory.replaced"))
            .await,
        Err(RepositoryError::Conflict(_))
    ));

    // Neither failed replace left a trace.
    assert_eq!(
        audit_types(&pool).await,
        vec!["memory.created", "memory.replaced"]
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn forget_removes_the_row_is_idempotent_and_cascades_to_embeddings(pool: PgPool) {
    let store = PgMemoryStore::new(pool.clone());
    let item = memory(MEM_A, "likes tea");
    store
        .create_embedded(
            &item,
            &embedding("bge-small-en-v1.5", 0.1),
            &audit_for(&item, "memory.created"),
        )
        .await
        .unwrap();
    assert_eq!(embedding_count(&pool, &mid(MEM_A)).await, 1);

    let deleted = store
        .forget(
            &uid(USER),
            &mid(MEM_A),
            &audit_for(&item, "memory.forgotten"),
        )
        .await
        .unwrap();
    assert!(deleted, "the first forget removes a real row");
    assert_eq!(store.get(&uid(USER), &mid(MEM_A)).await.unwrap(), None);
    assert_eq!(
        embedding_count(&pool, &mid(MEM_A)).await,
        0,
        "forget removes the derived embedding too (docs/02 §7)"
    );

    let repeat = store
        .forget(
            &uid(USER),
            &mid(MEM_A),
            &audit_for(&item, "memory.forgotten"),
        )
        .await
        .unwrap();
    assert!(
        !repeat,
        "forgetting an already-absent memory is a no-op, not an error"
    );

    assert_eq!(
        audit_types(&pool).await,
        vec!["memory.created", "memory.forgotten"],
        "the redundant forget wrote no audit row"
    );
}

// ---------------------------------------------------------------------------
// EmbeddedMemoryStore: create_embedded / replace_embedded
// ---------------------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn create_embedded_writes_memory_source_embedding_and_audit_atomically(pool: PgPool) {
    let store = PgMemoryStore::new(pool.clone());
    let item = memory(MEM_A, "the wifi network is HomeNet");
    let vector = embedding("bge-small-en-v1.5", 0.2);
    store
        .create_embedded(&item, &vector, &audit_for(&item, "memory.created"))
        .await
        .unwrap();

    assert_eq!(
        store.get(&uid(USER), &mid(MEM_A)).await.unwrap(),
        Some(item.clone())
    );
    assert_eq!(embedding_count(&pool, &mid(MEM_A)).await, 1);
    assert_eq!(audit_types(&pool).await, vec!["memory.created"]);
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(verify_chain(&mut conn).await.unwrap(), 1);

    // The embedding is retrievable — querying with the exact stored vector
    // is a near-perfect cosine match.
    let hits = store
        .retrieve(&uid(USER), None, &vector.embedding, 5)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memory, item);
    assert!(
        (hits[0].similarity - 1.0).abs() < 1e-4,
        "expected similarity ~1.0, got {}",
        hits[0].similarity
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn replace_embedded_updates_the_vector_and_rejects_dimension_mismatch(pool: PgPool) {
    let store = PgMemoryStore::new(pool.clone());
    let item = memory(MEM_A, "the wifi network is HomeNet");
    store
        .create_embedded(
            &item,
            &embedding("bge-small-en-v1.5", 0.2),
            &audit_for(&item, "memory.created"),
        )
        .await
        .unwrap();

    let mut updated = memory_at(
        MEM_A,
        USER,
        MemoryLayer::Semantic,
        "the wifi network is HomeNet-5G",
        MemorySource::Explicit,
        MemoryScope::User,
        RetentionRule::UntilForgotten,
        0.9,
        Sensitivity::Normal,
        false,
        item.created_at,
    );
    updated.updated_at = t0() + Duration::from_secs(30);
    let new_vector = embedding("bge-small-en-v1.5", 0.9);
    store
        .replace_embedded(
            &updated,
            &new_vector,
            &audit_for(&updated, "memory.replaced"),
        )
        .await
        .unwrap();

    assert_eq!(
        embedding_count(&pool, &mid(MEM_A)).await,
        1,
        "the vector is replaced in place, not duplicated"
    );
    let hits = store
        .retrieve(&uid(USER), None, &new_vector.embedding, 5)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memory.text, "the wifi network is HomeNet-5G");
    assert!((hits[0].similarity - 1.0).abs() < 1e-4);

    // Mismatched dimensions/embedding-length combinations are rejected
    // before any write — the same guard `stored_embedding()` applies to a
    // fresh create (requires dimensions == embedding.len() == 384).
    let bad_length = EmbeddedMemory {
        model_id: "bge-small-en-v1.5".to_owned(),
        dimensions: 384,
        embedding: vec![0.1; 100],
    };
    assert!(matches!(
        store
            .replace_embedded(
                &updated,
                &bad_length,
                &audit_for(&updated, "memory.replaced")
            )
            .await,
        Err(RepositoryError::Storage(_))
    ));
    let bad_dimensions = EmbeddedMemory {
        model_id: "bge-small-en-v1.5".to_owned(),
        dimensions: 100,
        embedding: vec![0.1; 100],
    };
    assert!(matches!(
        store
            .replace_embedded(
                &updated,
                &bad_dimensions,
                &audit_for(&updated, "memory.replaced")
            )
            .await,
        Err(RepositoryError::Storage(_))
    ));

    // Neither rejected call wrote a second audit row.
    assert_eq!(
        audit_types(&pool).await,
        vec!["memory.created", "memory.replaced"]
    );
}

// ---------------------------------------------------------------------------
// MemoryContextStore: record_context (migration 0014, the `rank` fix)
// ---------------------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn record_context_inserts_a_queryable_row_with_its_rank(pool: PgPool) {
    let store = PgMemoryStore::new(pool.clone());
    let item = memory(MEM_A, "likes tea");
    store
        .create(&item, &audit_for(&item, "memory.created"))
        .await
        .unwrap();

    let use_ = MemoryContextUse {
        run_id: rid(RUN_A),
        memory_id: mid(MEM_A),
        rank: 2,
        similarity: 0.87,
        used_at: t0(),
    };
    store
        .record_context(&uid(USER), std::slice::from_ref(&use_))
        .await
        .unwrap();

    assert_eq!(
        context_rows(&pool).await,
        vec![(
            USER.to_owned(),
            RUN_A.to_owned(),
            MEM_A.to_owned(),
            2,
            0.87_f32
        )]
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn record_context_is_a_no_op_on_a_duplicate_run_and_memory(pool: PgPool) {
    let store = PgMemoryStore::new(pool.clone());
    let item = memory(MEM_A, "likes tea");
    store
        .create(&item, &audit_for(&item, "memory.created"))
        .await
        .unwrap();
    let use_ = MemoryContextUse {
        run_id: rid(RUN_A),
        memory_id: mid(MEM_A),
        rank: 1,
        similarity: 0.5,
        used_at: t0(),
    };

    store
        .record_context(&uid(USER), std::slice::from_ref(&use_))
        .await
        .unwrap();
    store
        .record_context(&uid(USER), std::slice::from_ref(&use_))
        .await
        .unwrap();

    assert_eq!(
        context_rows(&pool).await.len(),
        1,
        "the same (run_id, memory_id) pair is recorded once — ON CONFLICT DO NOTHING"
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn record_context_silently_drops_a_memory_the_caller_does_not_own(pool: PgPool) {
    let store = PgMemoryStore::new(pool.clone());
    let item = memory(MEM_A, "likes tea"); // owned by USER
    store
        .create(&item, &audit_for(&item, "memory.created"))
        .await
        .unwrap();

    let use_ = MemoryContextUse {
        run_id: rid(RUN_A),
        memory_id: mid(MEM_A),
        rank: 0,
        similarity: 0.9,
        used_at: t0(),
    };
    // A different caller cannot record provenance for a memory it doesn't own.
    store
        .record_context(&uid(OTHER_USER), std::slice::from_ref(&use_))
        .await
        .unwrap();

    assert!(
        context_rows(&pool).await.is_empty(),
        "the WHERE EXISTS ownership guard silently drops the row"
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn record_context_rejects_an_out_of_range_rank_before_touching_the_database(pool: PgPool) {
    let store = PgMemoryStore::new(pool.clone());
    let item = memory(MEM_A, "likes tea");
    store
        .create(&item, &audit_for(&item, "memory.created"))
        .await
        .unwrap();

    for bad_rank in [-1, 9] {
        let use_ = MemoryContextUse {
            run_id: rid(RUN_A),
            memory_id: mid(MEM_A),
            rank: bad_rank,
            similarity: 0.5,
            used_at: t0(),
        };
        let result = store
            .record_context(&uid(USER), std::slice::from_ref(&use_))
            .await;
        assert!(
            matches!(result, Err(RepositoryError::Storage(_))),
            "rank {bad_rank} is outside 0..=8 and must be rejected, got {result:?}"
        );
    }
    assert!(context_rows(&pool).await.is_empty());
}
