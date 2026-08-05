//! F3b.8 acceptance — list persistence against live Postgres (FR-34, ADR-024,
//! migration 0012, invariant #6).
//!
//! The behaviours proven here are the ones a grocery list actually lives or
//! dies by: the list survives a restart in the order it was built, a check-off
//! addresses exactly one line by id, a write that changes nothing writes
//! nothing *including its audit row*, and the database itself refuses the
//! things the domain refuses — an unbounded list, a rewritten item, a
//! re-pointed promotion.

use std::time::{Duration, SystemTime};

use jarvis_application::ports::{ListStore, RepositoryError};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::{ArtifactId, ListId, ListItemId};
use jarvis_domain::lists::{ItemList, ItemText, ListItem, ListName};
use jarvis_infra::audit::verify_chain;
use jarvis_infra::lists::PgListStore;
use sqlx::PgPool;

const SHOPPING: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const TODO: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
const ARTIFACT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB9";
const ACTOR: &str = "device:01ARZ3NDEKTSV4RRFFQ69G5FB3";

fn list_id(raw: &str) -> ListId {
    raw.parse().expect("valid test ulid")
}

fn item_id(n: u16) -> ListItemId {
    format!("01J8Z{n:021}").parse().expect("valid test ulid")
}

fn t0() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

/// Audit events are ordered by their `occurred_at`; nudging each one forward
/// keeps a multi-write test's chain in the order the writes happened.
fn audit(seq: u64, event_type: &str, target: &str) -> AuditEvent {
    AuditEvent {
        occurred_at: t0() + Duration::from_secs(seq),
        actor: ACTOR.to_owned(),
        event_type: event_type.to_owned(),
        target: target.to_owned(),
        correlation_id: None,
        // Ids only, never list content (the application layer builds the real
        // payloads; this mirrors their shape).
        payload_json: format!(r#"{{"listId":"{target}"}}"#),
    }
}

fn item(n: u16, text: &str) -> ListItem {
    ListItem::new(item_id(n), ItemText::new(text).unwrap())
}

async fn seeded_shopping(store: &PgListStore) -> ItemList {
    let list = ItemList::new(list_id(SHOPPING), ListName::new("Shopping").unwrap());
    store
        .create(&list, &audit(0, "list.created", SHOPPING))
        .await
        .expect("create");
    list
}

async fn audit_types(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar!("SELECT event_type FROM audit.audit_events ORDER BY seq ASC")
        .fetch_all(pool)
        .await
        .expect("audit rows")
}

// ---------------------------------------------------------------------------
// THE test: a list survives, in order, with every change audited
// ---------------------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_list_round_trips_in_insertion_order_with_every_change_audited(pool: PgPool) {
    let store = PgListStore::new(pool.clone());
    seeded_shopping(&store).await;

    for (n, text) in [(1, "milk"), (2, "eggs"), (3, "bread")] {
        store
            .add_item(
                &list_id(SHOPPING),
                &item(n, text),
                &audit(u64::from(n), "list.item_added", SHOPPING),
            )
            .await
            .expect("add");
    }

    // Reading it back is what a restart does.
    let loaded = store
        .get(&list_id(SHOPPING))
        .await
        .unwrap()
        .expect("present");
    assert_eq!(loaded.name().as_str(), "Shopping");
    assert_eq!(
        loaded
            .items()
            .iter()
            .map(|i| i.text.as_str())
            .collect::<Vec<_>>(),
        vec!["milk", "eggs", "bread"],
        "items come back in the order they were added"
    );
    assert!(loaded.items().iter().all(|i| !i.checked));
    assert_eq!(loaded.promoted_artifact(), None);

    // Check off exactly one line, by id.
    assert!(
        store
            .set_checked(
                &list_id(SHOPPING),
                &item_id(2),
                true,
                &audit(10, "list.item_checked", SHOPPING),
            )
            .await
            .unwrap()
    );
    let loaded = store.get(&list_id(SHOPPING)).await.unwrap().unwrap();
    assert_eq!(
        loaded.items().iter().map(|i| i.checked).collect::<Vec<_>>(),
        vec![false, true, false],
        "only the addressed item moved"
    );
    assert_eq!(loaded.open_items().count(), 2);

    // Remove one line.
    assert!(
        store
            .remove_item(
                &list_id(SHOPPING),
                &item_id(1),
                &audit(11, "list.item_removed", SHOPPING),
            )
            .await
            .unwrap()
    );
    let loaded = store.get(&list_id(SHOPPING)).await.unwrap().unwrap();
    assert_eq!(loaded.items().len(), 2);
    assert_eq!(loaded.items()[0].text.as_str(), "eggs");

    // Every change co-transacted its audit row (invariant #6), in order, and
    // the hash chain still verifies.
    assert_eq!(
        audit_types(&pool).await,
        vec![
            "list.created",
            "list.item_added",
            "list.item_added",
            "list.item_added",
            "list.item_checked",
            "list.item_removed",
        ]
    );
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(verify_chain(&mut conn).await.unwrap(), 6);
}

// ---------------------------------------------------------------------------
// A write that changes nothing writes nothing — audit row included
// ---------------------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_miss_writes_absolutely_nothing(pool: PgPool) {
    let store = PgListStore::new(pool.clone());
    seeded_shopping(&store).await;
    store
        .add_item(
            &list_id(SHOPPING),
            &item(1, "milk"),
            &audit(1, "list.item_added", SHOPPING),
        )
        .await
        .unwrap();
    let before = audit_types(&pool).await.len();

    // An item that is not on this list.
    assert!(
        !store
            .set_checked(
                &list_id(SHOPPING),
                &item_id(77),
                true,
                &audit(2, "list.item_checked", SHOPPING),
            )
            .await
            .unwrap()
    );
    assert!(
        !store
            .remove_item(
                &list_id(SHOPPING),
                &item_id(77),
                &audit(3, "list.item_removed", SHOPPING),
            )
            .await
            .unwrap()
    );
    // An item that exists but on a *different* list must not be reachable
    // through this list's id — the predicate names both.
    let other = ItemList::new(list_id(TODO), ListName::new("Todo").unwrap());
    store
        .create(&other, &audit(4, "list.created", TODO))
        .await
        .unwrap();
    assert!(
        !store
            .set_checked(
                &list_id(TODO),
                &item_id(1),
                true,
                &audit(5, "list.item_checked", TODO),
            )
            .await
            .unwrap(),
        "an item cannot be checked off through a list it is not on"
    );

    // Exactly one further audit row: the second list's creation. None of the
    // three misses left a trace.
    assert_eq!(audit_types(&pool).await.len(), before + 1);
    // And the milk is untouched.
    let loaded = store.get(&list_id(SHOPPING)).await.unwrap().unwrap();
    assert!(!loaded.items()[0].checked);
}

// ---------------------------------------------------------------------------
// Name keys, lookup, and the index
// ---------------------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_second_spelling_of_the_same_name_is_a_conflict_not_a_rival_list(pool: PgPool) {
    let store = PgListStore::new(pool.clone());
    seeded_shopping(&store).await;

    let rival = ItemList::new(list_id(TODO), ListName::new("shopping list").unwrap());
    let err = store
        .create(&rival, &audit(1, "list.created", TODO))
        .await
        .expect_err("the key already exists");
    assert!(matches!(err, RepositoryError::Conflict(_)), "{err:?}");

    // The grammar's normalized key finds the original from any spelling — and
    // the store derives that key itself, from the name it is handed, so no
    // caller can look a list up by a key it normalized differently.
    for spelling in ["Shopping", "shopping list", "  SHOPPING  LIST "] {
        let name = ListName::new(spelling).unwrap();
        let found = store.find_by_key(&name).await.unwrap().expect(spelling);
        assert_eq!(found.id(), &list_id(SHOPPING));
    }
    assert!(
        store
            .find_by_key(&ListName::new("nonexistent").unwrap())
            .await
            .unwrap()
            .is_none()
    );
    // The failed create left no audit row.
    assert_eq!(audit_types(&pool).await.len(), 1);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn the_index_lists_every_list_with_its_own_items(pool: PgPool) {
    let store = PgListStore::new(pool.clone());
    seeded_shopping(&store).await;
    let todo = ItemList::new(list_id(TODO), ListName::new("Todo").unwrap());
    store
        .create(&todo, &audit(1, "list.created", TODO))
        .await
        .unwrap();

    store
        .add_item(
            &list_id(SHOPPING),
            &item(1, "milk"),
            &audit(2, "list.item_added", SHOPPING),
        )
        .await
        .unwrap();
    store
        .add_item(
            &list_id(TODO),
            &item(2, "call the plumber"),
            &audit(3, "list.item_added", TODO),
        )
        .await
        .unwrap();

    let all = store.list_all().await.unwrap();
    assert_eq!(all.len(), 2);
    // Name-key ordered, so the shell's index is stable.
    assert_eq!(all[0].name().as_str(), "Shopping");
    assert_eq!(all[1].name().as_str(), "Todo");
    assert_eq!(all[0].items()[0].text.as_str(), "milk");
    assert_eq!(all[1].items()[0].text.as_str(), "call the plumber");
    assert_eq!(all[0].items().len(), 1, "items are not cross-attributed");
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn an_empty_list_and_an_unknown_list_are_distinguishable(pool: PgPool) {
    let store = PgListStore::new(pool.clone());
    seeded_shopping(&store).await;
    let empty = store
        .get(&list_id(SHOPPING))
        .await
        .unwrap()
        .expect("exists");
    assert!(empty.is_empty());
    assert!(
        store.get(&list_id(TODO)).await.unwrap().is_none(),
        "an unknown list is None, never an empty list"
    );
}

// ---------------------------------------------------------------------------
// Promotion is write-once
// ---------------------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_promoted_list_keeps_one_document_identity(pool: PgPool) {
    let store = PgListStore::new(pool.clone());
    seeded_shopping(&store).await;
    let artifact: ArtifactId = ARTIFACT.parse().unwrap();

    store
        .record_promotion(
            &list_id(SHOPPING),
            &artifact,
            &audit(1, "list.promoted", SHOPPING),
        )
        .await
        .expect("first promotion");
    let loaded = store.get(&list_id(SHOPPING)).await.unwrap().unwrap();
    assert_eq!(loaded.promoted_artifact(), Some(&artifact));

    // A second promotion to a *different* artifact is refused: the version
    // chain the owner has been reading must not be silently orphaned.
    let rival: ArtifactId = "01ARZ3NDEKTSV4RRFFQ69G5FC0".parse().unwrap();
    let err = store
        .record_promotion(
            &list_id(SHOPPING),
            &rival,
            &audit(2, "list.promoted", SHOPPING),
        )
        .await
        .expect_err("already promoted");
    assert!(matches!(err, RepositoryError::Conflict(_)), "{err:?}");
    let loaded = store.get(&list_id(SHOPPING)).await.unwrap().unwrap();
    assert_eq!(loaded.promoted_artifact(), Some(&artifact));
    assert_eq!(
        audit_types(&pool).await.len(),
        2,
        "the refusal wrote nothing"
    );

    // An unknown list is the same conflict, not a panic.
    assert!(
        store
            .record_promotion(&list_id(TODO), &artifact, &audit(3, "list.promoted", TODO))
            .await
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// The database enforces the same rules as the domain (defence in depth)
// ---------------------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn the_database_refuses_to_rewrite_an_item_or_move_it_between_lists(pool: PgPool) {
    let store = PgListStore::new(pool.clone());
    seeded_shopping(&store).await;
    let todo = ItemList::new(list_id(TODO), ListName::new("Todo").unwrap());
    store
        .create(&todo, &audit(1, "list.created", TODO))
        .await
        .unwrap();
    store
        .add_item(
            &list_id(SHOPPING),
            &item(1, "milk"),
            &audit(2, "list.item_added", SHOPPING),
        )
        .await
        .unwrap();

    // Editing a line is remove + add; a silent overwrite is refused.
    let rewrite = sqlx::query("UPDATE lists.items SET text = 'caviar' WHERE id = $1")
        .bind(item_id(1).as_str())
        .execute(&pool)
        .await;
    assert!(rewrite.is_err(), "an item's text is immutable");

    let move_it = sqlx::query("UPDATE lists.items SET list_id = $2 WHERE id = $1")
        .bind(item_id(1).as_str())
        .bind(TODO)
        .execute(&pool)
        .await;
    assert!(move_it.is_err(), "an item never changes list");

    // The one column that may move, still moves.
    assert!(
        store
            .set_checked(
                &list_id(SHOPPING),
                &item_id(1),
                true,
                &audit(3, "list.item_checked", SHOPPING),
            )
            .await
            .unwrap()
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn the_database_refuses_a_blank_or_oversized_line(pool: PgPool) {
    let store = PgListStore::new(pool.clone());
    seeded_shopping(&store).await;

    for bad in ["", &"x".repeat(513)] {
        let inserted = sqlx::query(
            "INSERT INTO lists.items (id, list_id, text, checked, added_at) \
             VALUES ($1, $2, $3, false, now())",
        )
        .bind(item_id(9).as_str())
        .bind(SHOPPING)
        .bind(bad)
        .execute(&pool)
        .await;
        assert!(
            inserted.is_err(),
            "the CHECK must refuse a {}-byte line",
            bad.len()
        );
    }
    // A list item must belong to a list that exists.
    let orphan = sqlx::query(
        "INSERT INTO lists.items (id, list_id, text, checked, added_at) \
         VALUES ($1, $2, 'milk', false, now())",
    )
    .bind(item_id(9).as_str())
    .bind(TODO)
    .execute(&pool)
    .await;
    assert!(orphan.is_err(), "no orphan items");
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn the_database_bounds_a_list_even_if_a_writer_skips_the_aggregate(pool: PgPool) {
    let store = PgListStore::new(pool.clone());
    seeded_shopping(&store).await;

    // Fill to the bound behind the aggregate's back, then let the store try one
    // more through the normal path.
    sqlx::query(
        "INSERT INTO lists.items (id, list_id, text, checked, added_at) \
         SELECT lpad(n::text, 26, '0'), $1, 'x', false, now() \
         FROM generate_series(1, 500) AS n",
    )
    .bind(SHOPPING)
    .execute(&pool)
    .await
    .expect("bulk seed");

    let err = store
        .add_item(
            &list_id(SHOPPING),
            &item(999, "one too many"),
            &audit(1, "list.item_added", SHOPPING),
        )
        .await
        .expect_err("the trigger refuses the 501st item");
    // Refused, and nothing was written — not the item, not the audit row.
    assert!(matches!(err, RepositoryError::Storage(_)), "{err:?}");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM lists.items WHERE list_id = $1")
        .bind(SHOPPING)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 500);
    assert_eq!(audit_types(&pool).await.len(), 1, "only the create");
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn removing_a_list_takes_its_items_with_it(pool: PgPool) {
    let store = PgListStore::new(pool.clone());
    seeded_shopping(&store).await;
    store
        .add_item(
            &list_id(SHOPPING),
            &item(1, "milk"),
            &audit(1, "list.item_added", SHOPPING),
        )
        .await
        .unwrap();

    sqlx::query("DELETE FROM lists.lists WHERE id = $1")
        .bind(SHOPPING)
        .execute(&pool)
        .await
        .expect("delete cascades");
    let orphans: i64 = sqlx::query_scalar("SELECT count(*) FROM lists.items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(orphans, 0, "no item outlives its list");
}
