use super::shared::RepositoryError;
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::{ArtifactId, ListId, ListItemId};
use jarvis_domain::lists::{ItemList, ListItem, ListName};

/// Lists and quick-notes persistence (FR-34, ADR-024, invariant 6). Plain rows,
/// exportable — a list is a grocery line, not an artifact (that is what
/// promotion is for).
///
/// Three properties belong to the contract rather than the implementation:
///
/// * **Every mutating method co-transacts its [`AuditEvent`]** (invariant 6): a
///   check-off that cannot be audited did not happen. Reads take no audit event
///   — "what's on the shopping list" is a query, not a side effect.
/// * **Items are addressed by [`ListItemId`], never by their text.** Two lines
///   may legitimately read the same, and the text is untrusted; the grammar
///   resolves text to an id in the domain before this port is called.
/// * **A miss is `Ok(false)`, not an error.** An item that was already removed,
///   or checked off from another device a moment earlier, is a normal outcome
///   the caller reports honestly — and then **nothing** is written, audit row
///   included.
///
/// Lists are looked up by [`Self::find_by_key`] because the grammar names a list
/// by (untrusted, case-varying) text and the normalized
/// [`jarvis_domain::lists::ListName::key`] is what uniqueness is enforced on.
/// That normalization is why the lookup takes a
/// [`jarvis_domain::lists::ListName`] and not a `&str`: a caller holding a raw
/// name would key on the wrong string and miss every list, silently.
#[async_trait::async_trait]
pub trait ListStore: Send + Sync {
    /// Create a list. A key that already exists is a
    /// [`RepositoryError::Conflict`] — the caller resolves it by loading the
    /// existing list rather than shadowing it with a second one.
    async fn create(&self, list: &ItemList, audit: &AuditEvent) -> Result<(), RepositoryError>;

    /// One list, with its items in insertion order. Unknown => `Ok(None)`.
    async fn get(&self, id: &ListId) -> Result<Option<ItemList>, RepositoryError>;

    /// Find by name, matched on its normalized
    /// [`jarvis_domain::lists::ListName::key`]. Unknown => `Ok(None)`.
    /// Implementations derive the key themselves so no caller can bypass the
    /// normalization — "Shopping", "shopping list" and "  SHOPPING  " must all
    /// find the one list.
    async fn find_by_key(&self, name: &ListName) -> Result<Option<ItemList>, RepositoryError>;

    /// Every list, name-ordered so the shell renders a stable index.
    async fn list_all(&self) -> Result<Vec<ItemList>, RepositoryError>;

    /// Append one item, ordered by `audit.occurred_at`. An unknown list, or a
    /// list already at its item bound, is a [`RepositoryError::Conflict`].
    async fn add_item(
        &self,
        list: &ListId,
        item: &ListItem,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError>;

    /// Check off / un-check one item **by id**. `Ok(false)` when that list has
    /// no such item.
    async fn set_checked(
        &self,
        list: &ListId,
        item: &ListItemId,
        checked: bool,
        audit: &AuditEvent,
    ) -> Result<bool, RepositoryError>;

    /// Remove one item by id. `Ok(false)` when it was not there.
    async fn remove_item(
        &self,
        list: &ListId,
        item: &ListItemId,
        audit: &AuditEvent,
    ) -> Result<bool, RepositoryError>;

    /// Record which artifact this list was promoted into, so a later promotion
    /// appends a **version** to that artifact instead of minting a second
    /// document for the same list. Write-once: a list that is already promoted
    /// is a [`RepositoryError::Conflict`], never a silently repointed chain.
    async fn record_promotion(
        &self,
        list: &ListId,
        artifact: &ArtifactId,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError>;
}
