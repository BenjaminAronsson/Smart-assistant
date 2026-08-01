//! List and quick-note persistence (FR-34, ADR-024, migration 0012,
//! invariant #6). The infra side of the [`ListStore`] port.
//!
//! Three properties this module exists to guarantee:
//!
//! * **Every write co-transacts its audit row.** A list that cannot be audited
//!   is not created, and an item that cannot be audited is not added, checked
//!   off, or removed (invariant #6, skill `sqlx-data` §6).
//! * **A miss writes nothing at all.** `set_checked` and `remove_item` are
//!   `… WHERE id = $1 AND list_id = $2`; when the row has already gone — another
//!   device removed it, or the id was never on that list — zero rows change, the
//!   transaction rolls back, and no audit row records a change that did not
//!   happen. The caller gets `Ok(false)`, not an error and not a lie.
//! * **The reader is strict.** Every column is re-validated through the domain
//!   newtypes on the way out. A row this module cannot interpret is an error,
//!   never a default: a list whose name no longer sanitizes must not read back
//!   as a blank list the owner cannot find.
//!
//! Ordering is explicit (`added_at, id`), matching the index in 0012, so the
//! card and the promoted document read the way the list was built rather than
//! the way Postgres happened to return the pages.

use async_trait::async_trait;
use jarvis_application::ports::{ListStore, RepositoryError};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::{ArtifactId, ListId, ListItemId};
use jarvis_domain::lists::{ItemList, ItemText, ListItem, ListName};
use sqlx::PgPool;
use std::collections::HashMap;
use std::time::SystemTime;
use time::OffsetDateTime;

/// Postgres-backed list store.
pub struct PgListStore {
    pool: PgPool,
}

impl PgListStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn storage(context: &'static str) -> impl Fn(sqlx::Error) -> RepositoryError {
    move |e| {
        // A duplicate id or name key is a conflict, not a generic failure —
        // the caller resolves it by loading the list that already exists.
        if let Some(db) = e.as_database_error()
            && db.code().as_deref() == Some("23505")
        {
            return RepositoryError::Conflict("list already exists".to_owned());
        }
        // Foreign-key violation: the list is gone (or never existed).
        if let Some(db) = e.as_database_error()
            && db.code().as_deref() == Some("23503")
        {
            return RepositoryError::Conflict("no such list".to_owned());
        }
        RepositoryError::Storage(format!("{context}: {e}"))
    }
}

fn utc(t: SystemTime) -> OffsetDateTime {
    OffsetDateTime::from(t)
}

/// One stored list row, before its items are attached.
struct ListRow {
    id: String,
    name: String,
    promoted_artifact_id: Option<String>,
}

/// One stored item row.
struct ItemRow {
    id: String,
    list_id: String,
    text: String,
    checked: bool,
}

impl ItemRow {
    fn into_item(self) -> Result<(String, ListItem), RepositoryError> {
        let id = self
            .id
            .parse::<ListItemId>()
            .map_err(|e| RepositoryError::Storage(format!("bad list item id: {e}")))?;
        let text = ItemText::new(&self.text)
            .map_err(|e| RepositoryError::Storage(format!("bad list item text: {e}")))?;
        Ok((
            self.list_id,
            ListItem {
                id,
                text,
                checked: self.checked,
            },
        ))
    }
}

impl ListRow {
    /// Rebuild the domain aggregate. `items` is already in insertion order.
    fn into_list(self, items: Vec<ListItem>) -> Result<ItemList, RepositoryError> {
        let id = self
            .id
            .parse::<ListId>()
            .map_err(|e| RepositoryError::Storage(format!("bad list id: {e}")))?;
        let name = ListName::new(&self.name)
            .map_err(|e| RepositoryError::Storage(format!("bad list name: {e}")))?;
        let promoted = self
            .promoted_artifact_id
            .map(|raw| raw.parse::<ArtifactId>())
            .transpose()
            .map_err(|e| RepositoryError::Storage(format!("bad promoted artifact id: {e}")))?;
        Ok(ItemList::from_parts(id, name, items, promoted))
    }
}

impl PgListStore {
    /// Items of one list, in insertion order.
    async fn items_of(&self, id: &ListId) -> Result<Vec<ListItem>, RepositoryError> {
        let rows = sqlx::query_as!(
            ItemRow,
            r#"
            SELECT id, list_id, text, checked
            FROM lists.items
            WHERE list_id = $1
            ORDER BY added_at ASC, id ASC
            "#,
            id.as_str(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage("list items"))?;
        rows.into_iter()
            .map(|r| r.into_item().map(|(_, item)| item))
            .collect()
    }

    async fn hydrate(&self, row: Option<ListRow>) -> Result<Option<ItemList>, RepositoryError> {
        let Some(row) = row else { return Ok(None) };
        let id = row
            .id
            .parse::<ListId>()
            .map_err(|e| RepositoryError::Storage(format!("bad list id: {e}")))?;
        let items = self.items_of(&id).await?;
        row.into_list(items).map(Some)
    }
}

#[async_trait]
impl ListStore for PgListStore {
    async fn create(&self, list: &ItemList, audit: &AuditEvent) -> Result<(), RepositoryError> {
        let now = utc(audit.occurred_at);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(storage("list create: begin"))?;

        sqlx::query!(
            r#"
            INSERT INTO lists.lists (id, name, name_key, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $4)
            "#,
            list.id().as_str(),
            list.name().as_str(),
            list.name().key(),
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(storage("list create: insert"))?;

        // Same transaction as the row (invariant #6).
        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("list create: audit: {e}")))?;

        tx.commit().await.map_err(storage("list create: commit"))?;
        Ok(())
    }

    async fn get(&self, id: &ListId) -> Result<Option<ItemList>, RepositoryError> {
        let row = sqlx::query_as!(
            ListRow,
            r#"
            SELECT id, name, promoted_artifact_id
            FROM lists.lists
            WHERE id = $1
            "#,
            id.as_str(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(storage("list get"))?;
        self.hydrate(row).await
    }

    async fn find_by_key(&self, key: &str) -> Result<Option<ItemList>, RepositoryError> {
        let row = sqlx::query_as!(
            ListRow,
            r#"
            SELECT id, name, promoted_artifact_id
            FROM lists.lists
            WHERE name_key = $1
            "#,
            key,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(storage("list find_by_key"))?;
        self.hydrate(row).await
    }

    async fn list_all(&self) -> Result<Vec<ItemList>, RepositoryError> {
        // Two queries rather than a join: the index read is trivially small and
        // the grouping stays obvious, which matters more than one round trip on
        // an 8 GB target (docs/09 §5).
        let lists = sqlx::query_as!(
            ListRow,
            r#"
            SELECT id, name, promoted_artifact_id
            FROM lists.lists
            ORDER BY name_key ASC, id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage("list list_all"))?;

        let item_rows = sqlx::query_as!(
            ItemRow,
            r#"
            SELECT id, list_id, text, checked
            FROM lists.items
            ORDER BY list_id ASC, added_at ASC, id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage("list list_all: items"))?;

        let mut by_list: HashMap<String, Vec<ListItem>> = HashMap::new();
        for row in item_rows {
            let (list_id, item) = row.into_item()?;
            by_list.entry(list_id).or_default().push(item);
        }

        lists
            .into_iter()
            .map(|row| {
                let items = by_list.remove(&row.id).unwrap_or_default();
                row.into_list(items)
            })
            .collect()
    }

    async fn add_item(
        &self,
        list: &ListId,
        item: &ListItem,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        let now = utc(audit.occurred_at);
        let mut tx = self.pool.begin().await.map_err(storage("list add: begin"))?;

        sqlx::query!(
            r#"
            INSERT INTO lists.items (id, list_id, text, checked, added_at)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            item.id.as_str(),
            list.as_str(),
            item.text.as_str(),
            item.checked,
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(storage("list add: insert"))?;

        touch(&mut tx, list, now).await?;

        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("list add: audit: {e}")))?;

        tx.commit().await.map_err(storage("list add: commit"))?;
        Ok(())
    }

    async fn set_checked(
        &self,
        list: &ListId,
        item: &ListItemId,
        checked: bool,
        audit: &AuditEvent,
    ) -> Result<bool, RepositoryError> {
        let now = utc(audit.occurred_at);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(storage("list check: begin"))?;

        let updated = sqlx::query!(
            r#"
            UPDATE lists.items
            SET checked = $3
            WHERE id = $1 AND list_id = $2
            "#,
            item.as_str(),
            list.as_str(),
            checked,
        )
        .execute(&mut *tx)
        .await
        .map_err(storage("list check: update"))?
        .rows_affected();

        if updated == 0 {
            // The item is not on that list (any more). Roll back so NOTHING is
            // written — no audit row for a change that did not happen.
            tx.rollback()
                .await
                .map_err(storage("list check: rollback"))?;
            return Ok(false);
        }

        touch(&mut tx, list, now).await?;

        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("list check: audit: {e}")))?;

        tx.commit().await.map_err(storage("list check: commit"))?;
        Ok(true)
    }

    async fn remove_item(
        &self,
        list: &ListId,
        item: &ListItemId,
        audit: &AuditEvent,
    ) -> Result<bool, RepositoryError> {
        let now = utc(audit.occurred_at);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(storage("list remove: begin"))?;

        let deleted = sqlx::query!(
            r#"
            DELETE FROM lists.items
            WHERE id = $1 AND list_id = $2
            "#,
            item.as_str(),
            list.as_str(),
        )
        .execute(&mut *tx)
        .await
        .map_err(storage("list remove: delete"))?
        .rows_affected();

        if deleted == 0 {
            tx.rollback()
                .await
                .map_err(storage("list remove: rollback"))?;
            return Ok(false);
        }

        touch(&mut tx, list, now).await?;

        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("list remove: audit: {e}")))?;

        tx.commit().await.map_err(storage("list remove: commit"))?;
        Ok(true)
    }

    async fn record_promotion(
        &self,
        list: &ListId,
        artifact: &ArtifactId,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        let now = utc(audit.occurred_at);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(storage("list promote: begin"))?;

        // Write-once, enforced in the predicate as well as by the 0012 trigger:
        // a list that is already a document keeps that document's identity, and
        // a later promotion adds a version to it.
        let updated = sqlx::query!(
            r#"
            UPDATE lists.lists
            SET promoted_artifact_id = $2, updated_at = $3
            WHERE id = $1 AND promoted_artifact_id IS NULL
            "#,
            list.as_str(),
            artifact.as_str(),
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(storage("list promote: update"))?
        .rows_affected();

        if updated == 0 {
            tx.rollback()
                .await
                .map_err(storage("list promote: rollback"))?;
            return Err(RepositoryError::Conflict(
                "list is unknown or already promoted".to_owned(),
            ));
        }

        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("list promote: audit: {e}")))?;

        tx.commit()
            .await
            .map_err(storage("list promote: commit"))?;
        Ok(())
    }
}

/// Move the owning list's `updated_at`, inside the caller's transaction. Purely
/// a freshness marker for the shell's index; it is deliberately the only column
/// an item write touches on the parent row.
async fn touch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    list: &ListId,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    sqlx::query!(
        r#"UPDATE lists.lists SET updated_at = $2 WHERE id = $1"#,
        list.as_str(),
        now,
    )
    .execute(&mut **tx)
    .await
    .map_err(storage("list touch"))?;
    Ok(())
}
