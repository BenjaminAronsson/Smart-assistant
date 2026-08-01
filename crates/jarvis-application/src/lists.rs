//! Lists and quick notes use cases (FR-34, ADR-024, docs/02 §11e).
//!
//! The application half of the lists module: it sequences the pure domain
//! operations ([`jarvis_domain::lists`]) against the [`ListStore`],
//! [`BlobStore`] and [`ArtifactStore`] ports, and builds the audit events those
//! ports write **in the same transaction** as the row they describe
//! (invariant 6).
//!
//! Four properties are deliberate and tested:
//!
//! * **Zero model calls.** [`ListsService`] holds no [`crate::model::ModelProvider`]
//!   and there is no field through which it could reach one. Add, check-off and
//!   read run entirely on the deterministic grammar
//!   ([`jarvis_domain::lists::parse_list_command`]) — offline, degraded-mode
//!   safe, and free (ADR-024). LLM assist for genuinely ambiguous phrasing is a
//!   later feature and would arrive as a *caller* of this service, never inside
//!   it.
//! * **Everything is cancellable** (invariant 4). Every mutating method takes a
//!   [`CancellationToken`] and checks it immediately before the persist phase.
//!   The ports themselves are short single-statement writes, so check-before-
//!   persist is exactly where cancellation matters: an abandoned run does not
//!   get to leave a row behind.
//! * **Audit payloads carry ids, never content.** A list item's text is
//!   untrusted display data; the audit row records *which* list and *which* item
//!   changed (both ULIDs) and what the change was. That keeps the hash-chained
//!   audit free of user prose and makes the payload trivially well-formed JSON
//!   without a JSON dependency in this layer (invariant 3).
//! * **Reading never creates.** "What's on the shopping list" with no shopping
//!   list is [`ListsError::UnknownList`] — a speakable answer — not a silently
//!   minted empty list. Only `add` and `note` create, because only they were
//!   asked to put something somewhere.

use std::sync::Arc;

use jarvis_domain::artifact::{
    ArtifactContent, ArtifactKind, ArtifactManifest, ArtifactSource, BuildProvenance, MediaType,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::{ArtifactId, ListId, ListItemId, RunId};
use jarvis_domain::lists::{
    ItemList, ItemText, ListCommand, ListError, ListItem, ListName, MAX_ITEMS_PER_LIST,
};
use jarvis_domain::location::Sensitivity;
use tokio_util::sync::CancellationToken;

use crate::orchestrator::Clock;
use crate::ports::{ArtifactStore, BlobStore, ListStore, RepositoryError};

/// Media type of a promoted list document.
pub const LIST_DOCUMENT_MEDIA_TYPE: &str = "text/markdown";

/// Why a lists use case could not complete.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ListsError {
    /// No list with that id or name. A clean, speakable outcome ("there is no
    /// shopping list yet"), not a storage failure.
    #[error("there is no list named {0}")]
    UnknownList(String),
    /// The addressed item is not on that list — it was already removed, or the
    /// grammar's text matched nothing on it.
    #[error("that item is not on the list")]
    UnknownItem,
    /// The supplied name or text was empty / unusable after sanitization, or the
    /// list is at its bound.
    #[error(transparent)]
    Invalid(#[from] jarvis_domain::lists::ListError),
    /// The list (or its document) moved under this request: another writer got
    /// there first. **Permanent for this request and not a storage outage** —
    /// the caller re-reads and decides again, rather than retrying blind. Kept
    /// distinct from [`Self::Storage`] because collapsing the two tells the
    /// client, and the ops dashboard, that the service is unhealthy when it is
    /// merely contended.
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("the operation was cancelled")]
    Cancelled,
    #[error("storage failure: {0}")]
    Storage(String),
}

impl From<RepositoryError> for ListsError {
    fn from(e: RepositoryError) -> Self {
        match e {
            // A conflict is a losable race, not a broken store.
            RepositoryError::Conflict(m) => ListsError::Conflict(m),
            RepositoryError::IdempotencyConflict => {
                ListsError::Conflict("idempotency key reused with a different payload".to_owned())
            }
            RepositoryError::Storage(m) => ListsError::Storage(m),
        }
    }
}

/// Whether [`ListsService::ensure_list`] minted the list or found one that was
/// already there. Mirrors [`crate::ports::CreateOutcome`]: an idempotent create
/// that cannot say which of the two happened forces its caller to answer
/// `201 Created` for a list it did not create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsuredList {
    Created(ItemList),
    AlreadyExists(ItemList),
}

impl EnsuredList {
    pub fn list(&self) -> &ItemList {
        match self {
            Self::Created(list) | Self::AlreadyExists(list) => list,
        }
    }

    pub fn into_list(self) -> ItemList {
        match self {
            Self::Created(list) | Self::AlreadyExists(list) => list,
        }
    }

    /// True only when this call is the one that created the list.
    pub fn was_created(&self) -> bool {
        matches!(self, Self::Created(_))
    }
}

/// The result of promoting a list to a versioned artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotedList {
    pub artifact_id: ArtifactId,
    pub version: u32,
    /// Content address of the markdown document (lowercase hex).
    pub sha256_hex: String,
    /// True when this promotion created the artifact rather than appending a
    /// version to an existing one.
    pub first_promotion: bool,
}

/// What a grammar command did. The list is always returned alongside so the
/// caller can render the card and speak the readback from one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandEffect {
    Added(ListItemId),
    Removed(ListItemId),
    CheckedOff(ListItemId),
    /// A pure query — nothing was written and nothing was audited.
    Read,
}

/// Outcome of [`ListsService::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    pub list: ItemList,
    pub effect: CommandEffect,
}

/// Ids the host mints for one grammar command. Both are minted up front (a ULID
/// is free) and the unused one is simply dropped — the application layer never
/// generates randomness (docs/04 §2: ids are minted at the edges).
#[derive(Debug, Clone)]
pub struct CommandIds {
    pub list: ListId,
    pub item: ListItemId,
}

/// Lists and quick notes (ADR-024).
pub struct ListsService {
    lists: Arc<dyn ListStore>,
    blobs: Arc<dyn BlobStore>,
    artifacts: Arc<dyn ArtifactStore>,
    clock: Arc<dyn Clock>,
}

impl ListsService {
    pub fn new(
        lists: Arc<dyn ListStore>,
        blobs: Arc<dyn BlobStore>,
        artifacts: Arc<dyn ArtifactStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            lists,
            blobs,
            artifacts,
            clock,
        }
    }

    fn audit(
        &self,
        actor: &str,
        event_type: &str,
        target: String,
        payload_json: String,
    ) -> AuditEvent {
        AuditEvent {
            occurred_at: self.clock.now(),
            actor: actor.to_owned(),
            event_type: event_type.to_owned(),
            target,
            correlation_id: None,
            payload_json,
        }
    }

    // --- reads -------------------------------------------------------------

    /// One list by id.
    pub async fn get(&self, id: &ListId) -> Result<ItemList, ListsError> {
        self.lists
            .get(id)
            .await?
            .ok_or_else(|| ListsError::UnknownList(id.to_string()))
    }

    /// Every list, for the shell's index.
    pub async fn all(&self) -> Result<Vec<ItemList>, ListsError> {
        Ok(self.lists.list_all().await?)
    }

    /// Resolve a spoken list name to a stored list. A miss is
    /// [`ListsError::UnknownList`] — reading or checking off never creates.
    pub async fn resolve(&self, name: &ListName) -> Result<ItemList, ListsError> {
        self.lists
            .find_by_key(name)
            .await?
            .ok_or_else(|| ListsError::UnknownList(name.to_string()))
    }

    // --- writes ------------------------------------------------------------

    /// Get the list with this name, creating it if it does not exist yet.
    ///
    /// "add milk to the shopping list" must work before a shopping list exists,
    /// so creation is implicit. A concurrent creator losing the unique-key race
    /// re-reads rather than failing: two devices saying the same thing at once
    /// converge on one list instead of producing two.
    ///
    /// The [`EnsuredList`] says which of the two happened, so the REST surface
    /// can answer `201 Created` only for a list this call actually created.
    pub async fn ensure_list(
        &self,
        id: ListId,
        name: ListName,
        actor: &str,
        cancel: &CancellationToken,
    ) -> Result<EnsuredList, ListsError> {
        if let Some(existing) = self.lists.find_by_key(&name).await? {
            return Ok(EnsuredList::AlreadyExists(existing));
        }
        if cancel.is_cancelled() {
            return Err(ListsError::Cancelled);
        }
        let list = ItemList::new(id.clone(), name);
        let audit = self.audit(
            actor,
            "list.created",
            format!("list:{id}"),
            format!("{{\"listId\":\"{id}\"}}"),
        );
        match self.lists.create(&list, &audit).await {
            Ok(()) => Ok(EnsuredList::Created(list)),
            // Lost the race against another creator with the same key.
            Err(RepositoryError::Conflict(_)) => self
                .lists
                .find_by_key(list.name())
                .await?
                .map(EnsuredList::AlreadyExists)
                .ok_or_else(|| ListsError::Storage("list key conflict without a list".to_owned())),
            Err(e) => Err(e.into()),
        }
    }

    /// Append an item to an existing list.
    ///
    /// The bound is enforced by the domain aggregate here, before any write; the
    /// migration's trigger is defence in depth for a writer that skipped the
    /// aggregate entirely, not the primary check.
    pub async fn add_item(
        &self,
        list_id: &ListId,
        item_id: ListItemId,
        text: ItemText,
        actor: &str,
        cancel: &CancellationToken,
    ) -> Result<ItemList, ListsError> {
        let mut list = self.get(list_id).await?;
        let item = ListItem::new(item_id.clone(), text);
        list.add(item.clone())?;
        if cancel.is_cancelled() {
            return Err(ListsError::Cancelled);
        }
        let audit = self.audit(
            actor,
            "list.item_added",
            format!("list:{list_id}"),
            format!("{{\"listId\":\"{list_id}\",\"itemId\":\"{item_id}\"}}"),
        );
        match self.lists.add_item(list_id, &item, &audit).await {
            Ok(()) => Ok(list),
            // The store refused the append even though our (possibly stale)
            // view of the list allowed it. Both of its reasons are permanent
            // for this request and neither is an outage, so say which.
            Err(RepositoryError::Conflict(_)) => Err(self.explain_rejected_append(list_id).await),
            Err(e) => Err(e.into()),
        }
    }

    /// Attribute an append the store rejected. The list having gone away (its
    /// foreign key) and the list already being at its bound (its trigger) both
    /// arrive as one conflict, and the difference matters to the caller — "no
    /// such list" and "that list is full" are different answers, and different
    /// status codes. Resolved by re-reading rather than by parsing the store's
    /// message.
    async fn explain_rejected_append(&self, list_id: &ListId) -> ListsError {
        match self.lists.get(list_id).await {
            Ok(None) => ListsError::UnknownList(list_id.to_string()),
            Ok(Some(fresh)) if fresh.items().len() >= MAX_ITEMS_PER_LIST => {
                ListsError::Invalid(ListError::Full)
            }
            Ok(Some(_)) => {
                ListsError::Conflict("the list changed while the line was being added".to_owned())
            }
            Err(e) => e.into(),
        }
    }

    /// Check off (or un-check) an item by id — the card's tap affordance and,
    /// from M5, the voice path both land here.
    pub async fn set_checked(
        &self,
        list_id: &ListId,
        item_id: &ListItemId,
        checked: bool,
        actor: &str,
        cancel: &CancellationToken,
    ) -> Result<ItemList, ListsError> {
        let mut list = self.get(list_id).await?;
        if !list.set_checked(item_id, checked) {
            return Err(ListsError::UnknownItem);
        }
        if cancel.is_cancelled() {
            return Err(ListsError::Cancelled);
        }
        let audit = self.audit(
            actor,
            "list.item_checked",
            format!("list:{list_id}"),
            format!("{{\"listId\":\"{list_id}\",\"itemId\":\"{item_id}\",\"checked\":{checked}}}"),
        );
        if !self
            .lists
            .set_checked(list_id, item_id, checked, &audit)
            .await?
        {
            // Another device got there first and removed it; nothing was
            // written, so report the miss rather than the stale local view.
            return Err(ListsError::UnknownItem);
        }
        Ok(list)
    }

    /// Remove an item by id.
    pub async fn remove_item(
        &self,
        list_id: &ListId,
        item_id: &ListItemId,
        actor: &str,
        cancel: &CancellationToken,
    ) -> Result<ItemList, ListsError> {
        let mut list = self.get(list_id).await?;
        if !list.remove(item_id) {
            return Err(ListsError::UnknownItem);
        }
        if cancel.is_cancelled() {
            return Err(ListsError::Cancelled);
        }
        let audit = self.audit(
            actor,
            "list.item_removed",
            format!("list:{list_id}"),
            format!("{{\"listId\":\"{list_id}\",\"itemId\":\"{item_id}\"}}"),
        );
        if !self.lists.remove_item(list_id, item_id, &audit).await? {
            return Err(ListsError::UnknownItem);
        }
        Ok(list)
    }

    /// Capture a quick note: a single-item write into the well-known `Notes`
    /// list, created on first use (ADR-024).
    pub async fn quick_note(
        &self,
        ids: CommandIds,
        text: ItemText,
        actor: &str,
        cancel: &CancellationToken,
    ) -> Result<ItemList, ListsError> {
        let notes = self
            .ensure_list(ids.list, ListName::notes(), actor, cancel)
            .await?
            .into_list();
        self.add_item(notes.id(), ids.item, text, actor, cancel)
            .await
    }

    /// Execute a command produced by the **deterministic grammar** — the whole
    /// FR-34 voice path, with no model in it.
    pub async fn apply(
        &self,
        command: &ListCommand,
        ids: CommandIds,
        actor: &str,
        cancel: &CancellationToken,
    ) -> Result<CommandOutcome, ListsError> {
        match command {
            ListCommand::Note { text } => {
                let item = ids.item.clone();
                let list = self.quick_note(ids, text.clone(), actor, cancel).await?;
                Ok(CommandOutcome {
                    list,
                    effect: CommandEffect::Added(item),
                })
            }
            ListCommand::Add { list, text } => {
                let target = self
                    .ensure_list(ids.list, list.clone(), actor, cancel)
                    .await?
                    .into_list();
                let item = ids.item.clone();
                let updated = self
                    .add_item(target.id(), ids.item, text.clone(), actor, cancel)
                    .await?;
                Ok(CommandOutcome {
                    list: updated,
                    effect: CommandEffect::Added(item),
                })
            }
            ListCommand::Read { list } => {
                let found = self.resolve(list).await?;
                Ok(CommandOutcome {
                    list: found,
                    effect: CommandEffect::Read,
                })
            }
            ListCommand::CheckOff { list, text } => {
                let found = self.resolve(list).await?;
                let item = found
                    .find_by_text(text)
                    .ok_or(ListsError::UnknownItem)?
                    .id
                    .clone();
                let updated = self
                    .set_checked(found.id(), &item, true, actor, cancel)
                    .await?;
                Ok(CommandOutcome {
                    list: updated,
                    effect: CommandEffect::CheckedOff(item),
                })
            }
            ListCommand::Remove { list, text } => {
                let found = self.resolve(list).await?;
                let item = found
                    .find_by_text(text)
                    .ok_or(ListsError::UnknownItem)?
                    .id
                    .clone();
                let updated = self.remove_item(found.id(), &item, actor, cancel).await?;
                Ok(CommandOutcome {
                    list: updated,
                    effect: CommandEffect::Removed(item),
                })
            }
        }
    }

    // --- promotion ---------------------------------------------------------

    /// Promote a list to a **versioned artifact** (FR-08, ADR-024: "when it
    /// grows into a document"). Reuses the existing artifact path verbatim: the
    /// markdown bytes go into the [`BlobStore`], the manifest and its
    /// `artifact.created` audit event are written by
    /// [`ArtifactStore::create_version`] in **one transaction** (invariant 6).
    /// There is no second artifact path and no second escaper — the document is
    /// rendered by [`ItemList::to_markdown`], which shares
    /// [`jarvis_domain::markdown::escape`] with the Research Notes promotion.
    ///
    /// A list already promoted once appends the **next version** of the same
    /// artifact, so the document keeps one identity as the list evolves;
    /// `fresh_artifact_id` is used only on the first promotion.
    ///
    /// **The pointer is written before the artifact, deliberately.** The two
    /// halves land in two transactions — `record_promotion` on the list row,
    /// `create_version` on the artifact — and no ordering makes them one write.
    /// Anchoring the pointer first is the ordering that stays *recoverable*: if
    /// the artifact never lands, the list points at a document with no versions
    /// and the next promotion simply creates version 1 of that same document
    /// (handled below). The reverse order cannot be made safe — an artifact
    /// written while the pointer is still NULL is invisible to the list, so the
    /// next promotion mints a **rival** document for it, which is precisely what
    /// 0012's `lists_guard` trigger and ADR-024 exist to prevent.
    pub async fn promote(
        &self,
        list_id: &ListId,
        fresh_artifact_id: ArtifactId,
        run_id: RunId,
        actor: &str,
        cancel: &CancellationToken,
    ) -> Result<PromotedList, ListsError> {
        let list = self.get(list_id).await?;
        // Untrusted item text is escaped by the domain renderer — the `#` and
        // `- [ ]` structure is ours, the content is data (docs/06 §2).
        let document = list.to_markdown();
        if cancel.is_cancelled() {
            return Err(ListsError::Cancelled);
        }
        let sha256 = self
            .blobs
            .put(document.as_bytes())
            .await
            .map_err(|e| ListsError::Storage(e.to_string()))?;
        let sha256_hex = sha256.to_string();
        let content = ArtifactContent {
            sha256,
            media_type: LIST_DOCUMENT_MEDIA_TYPE
                .parse::<MediaType>()
                .expect("text/markdown is a valid media type"),
            kind: ArtifactKind::MarkdownHtml,
            sources: vec![ArtifactSource::Run(run_id.clone())],
            // A list is personal context (a shopping list, a packing list, a
            // note to self): label it sensitive so context assembly treats it
            // under the stricter visibility rules (NFR-02).
            sensitivity: Sensitivity::Sensitive,
            build: BuildProvenance::none(),
            capabilities: Vec::new(),
        };

        // Bind the list to its document's identity first, once and for all.
        let artifact_id = match list.promoted_artifact() {
            Some(existing) => existing.clone(),
            None => {
                self.anchor_promotion(list_id, fresh_artifact_id, actor)
                    .await?
            }
        };

        // Version off whatever chain that identity already has. `None` is the
        // recovery path, not an error: the pointer was anchored but its first
        // version never landed, so this promotion finishes the job instead of
        // failing forever on a document that does not exist yet.
        let manifest = match self.artifacts.latest(&artifact_id).await? {
            Some(latest) => latest
                .next_version(run_id.clone(), content)
                .ok_or_else(|| ListsError::Storage("artifact version overflow".to_owned()))?,
            None => ArtifactManifest::initial(artifact_id.clone(), run_id.clone(), content),
        };

        let version = manifest.version().get();
        // A fact about the document, not about which branch got us here: the
        // promotion that wrote version 1 is the one that created it.
        let first_promotion = version == 1;
        let item_count = list.items().len();
        let created = AuditEvent {
            occurred_at: self.clock.now(),
            actor: actor.to_owned(),
            event_type: "artifact.created".to_owned(),
            target: format!("artifact:{artifact_id}"),
            correlation_id: Some(run_id.to_string()),
            // Ids and counts only — never the list's (untrusted) content.
            payload_json: format!(
                "{{\"kind\":\"markdown_html\",\"mediaType\":\"{LIST_DOCUMENT_MEDIA_TYPE}\",\
                 \"sha256\":\"{sha256_hex}\",\"listId\":\"{list_id}\",\"version\":{version},\
                 \"itemCount\":{item_count}}}"
            ),
        };
        self.artifacts.create_version(&manifest, &created).await?;

        Ok(PromotedList {
            artifact_id,
            version,
            sha256_hex,
            first_promotion,
        })
    }

    /// Bind this list to the artifact that will carry its document, write-once
    /// (the store's predicate and 0012's `lists_guard` both enforce it), with
    /// the `list.promoted` audit row in the same transaction (invariant 6).
    ///
    /// Losing that race is **not** a failure: another promotion — this device a
    /// moment earlier, or another one — already chose the document's identity,
    /// so we adopt it rather than minting a second document for the same list.
    async fn anchor_promotion(
        &self,
        list_id: &ListId,
        fresh: ArtifactId,
        actor: &str,
    ) -> Result<ArtifactId, ListsError> {
        let promoted = self.audit(
            actor,
            "list.promoted",
            format!("list:{list_id}"),
            format!("{{\"listId\":\"{list_id}\",\"artifactId\":\"{fresh}\"}}"),
        );
        match self
            .lists
            .record_promotion(list_id, &fresh, &promoted)
            .await
        {
            Ok(()) => Ok(fresh),
            // Write-once refused: the list is already a document, or it is
            // gone. Re-read to tell those apart rather than guessing.
            Err(RepositoryError::Conflict(_)) => {
                let list = self.get(list_id).await?;
                list.promoted_artifact().cloned().ok_or_else(|| {
                    ListsError::Conflict(
                        "the list refused promotion but names no document".to_owned(),
                    )
                })
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{BlobStoreError, RepositoryError};
    use crate::testing::ManualClock;
    use jarvis_domain::artifact::ArtifactVersion;
    use jarvis_domain::grants::Sha256;
    use jarvis_domain::lists::{MAX_ITEMS_PER_LIST, parse_list_command};
    use std::collections::HashMap;
    use std::sync::Mutex;

    const ACTOR: &str = "device:01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn list_id(n: u8) -> ListId {
        format!("01J8Z0000000000000000000{n:02}").parse().unwrap()
    }

    fn item_id(n: u8) -> ListItemId {
        format!("01J8Z0000000000000000001{n:02}").parse().unwrap()
    }

    fn artifact_id(n: u8) -> ArtifactId {
        format!("01J8Z0000000000000000002{n:02}").parse().unwrap()
    }

    fn run_id() -> RunId {
        "01J8Z000000000000000000900".parse().unwrap()
    }

    fn ids(n: u8) -> CommandIds {
        CommandIds {
            list: list_id(n),
            item: item_id(n),
        }
    }

    fn text(raw: &str) -> ItemText {
        ItemText::new(raw).unwrap()
    }

    // ---- doubles -----------------------------------------------------------

    /// How the next `add_item` should be rejected by the *store*, after our own
    /// (necessarily stale) read of the list said it would fit. Both are races
    /// the Postgres store really has — its foreign key and its item-bound
    /// trigger — and the service has to attribute them rather than call them
    /// outages.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    enum AddFault {
        #[default]
        None,
        /// Another device deleted the list between our read and our write.
        ListVanished,
        /// Another writer filled the list to its bound first.
        ListFilled,
    }

    /// How the next `record_promotion` should behave.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    enum PromotionFault {
        #[default]
        None,
        /// The pointer write fails outright — the process dying between the two
        /// halves of a promotion looks exactly like this to the caller.
        Fails,
        /// Another promotion anchored this list to a *different* document
        /// first, so the write-once predicate refuses ours.
        LostToAnotherDocument(ArtifactId),
    }

    /// An in-memory [`ListStore`] with the same semantics as the Postgres one:
    /// unique on the name key, items in insertion order, a miss is `Ok(false)`
    /// with nothing written, and every write co-transacts its audit row.
    #[derive(Default)]
    struct FakeLists {
        rows: Mutex<Vec<ItemList>>,
        audits: Mutex<Vec<AuditEvent>>,
        /// Set to fail the next `create` with a key conflict, to exercise the
        /// concurrent-creator path.
        conflict_on_create: Mutex<bool>,
        add_fault: Mutex<AddFault>,
        promotion_fault: Mutex<PromotionFault>,
    }

    impl FakeLists {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn fail_next_add(&self, fault: AddFault) {
            *self.add_fault.lock().unwrap() = fault;
        }

        fn fail_next_promotion(&self, fault: PromotionFault) {
            *self.promotion_fault.lock().unwrap() = fault;
        }

        fn audit_types(&self) -> Vec<String> {
            self.audits
                .lock()
                .unwrap()
                .iter()
                .map(|a| a.event_type.clone())
                .collect()
        }

        fn audit_payloads(&self) -> Vec<String> {
            self.audits
                .lock()
                .unwrap()
                .iter()
                .map(|a| a.payload_json.clone())
                .collect()
        }

        fn stored(&self, id: &ListId) -> Option<ItemList> {
            self.rows
                .lock()
                .unwrap()
                .iter()
                .find(|l| l.id() == id)
                .cloned()
        }

        fn seed(&self, list: ItemList) {
            self.rows.lock().unwrap().push(list);
        }
    }

    #[async_trait::async_trait]
    impl ListStore for FakeLists {
        async fn create(&self, list: &ItemList, audit: &AuditEvent) -> Result<(), RepositoryError> {
            let mut rows = self.rows.lock().unwrap();
            let mut flag = self.conflict_on_create.lock().unwrap();
            if *flag {
                *flag = false;
                // Simulate the other creator committing first.
                rows.push(ItemList::new(list_id(99), list.name().clone()));
                return Err(RepositoryError::Conflict("duplicate key".to_owned()));
            }
            if rows.iter().any(|l| l.name().key() == list.name().key()) {
                return Err(RepositoryError::Conflict("duplicate key".to_owned()));
            }
            rows.push(list.clone());
            self.audits.lock().unwrap().push(audit.clone());
            Ok(())
        }

        async fn get(&self, id: &ListId) -> Result<Option<ItemList>, RepositoryError> {
            Ok(self.stored(id))
        }

        async fn find_by_key(&self, name: &ListName) -> Result<Option<ItemList>, RepositoryError> {
            let key = name.key();
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .find(|l| l.name().key() == key)
                .cloned())
        }

        async fn list_all(&self) -> Result<Vec<ItemList>, RepositoryError> {
            let mut all = self.rows.lock().unwrap().clone();
            all.sort_by_key(|l| l.name().clone());
            Ok(all)
        }

        async fn add_item(
            &self,
            list: &ListId,
            item: &ListItem,
            audit: &AuditEvent,
        ) -> Result<(), RepositoryError> {
            let mut rows = self.rows.lock().unwrap();
            // Apply the scripted race first: it happens *between* the service's
            // read and this write, which is exactly what makes it a race.
            match std::mem::take(&mut *self.add_fault.lock().unwrap()) {
                AddFault::None => {}
                AddFault::ListVanished => {
                    rows.retain(|l| l.id() != list);
                    // A foreign key violation, as the Postgres store reports it.
                    return Err(RepositoryError::Conflict("no such list".to_owned()));
                }
                AddFault::ListFilled => {
                    if let Some(row) = rows.iter_mut().find(|l| l.id() == list) {
                        while row.items().len() < MAX_ITEMS_PER_LIST {
                            let n = row.items().len();
                            let id: ListItemId = format!("01J8Z{n:021}").parse().unwrap();
                            row.add(ListItem::new(id, ItemText::new("x").unwrap()))
                                .unwrap();
                        }
                    }
                    // The 0012 item-bound trigger, as the store reports it.
                    return Err(RepositoryError::Conflict("list is full".to_owned()));
                }
            }
            let Some(row) = rows.iter_mut().find(|l| l.id() == list) else {
                return Err(RepositoryError::Conflict("unknown list".to_owned()));
            };
            row.add(item.clone())
                .map_err(|e| RepositoryError::Conflict(e.to_string()))?;
            self.audits.lock().unwrap().push(audit.clone());
            Ok(())
        }

        async fn set_checked(
            &self,
            list: &ListId,
            item: &ListItemId,
            checked: bool,
            audit: &AuditEvent,
        ) -> Result<bool, RepositoryError> {
            let mut rows = self.rows.lock().unwrap();
            let Some(row) = rows.iter_mut().find(|l| l.id() == list) else {
                return Ok(false);
            };
            if !row.set_checked(item, checked) {
                // Nothing written — audit included.
                return Ok(false);
            }
            self.audits.lock().unwrap().push(audit.clone());
            Ok(true)
        }

        async fn remove_item(
            &self,
            list: &ListId,
            item: &ListItemId,
            audit: &AuditEvent,
        ) -> Result<bool, RepositoryError> {
            let mut rows = self.rows.lock().unwrap();
            let Some(row) = rows.iter_mut().find(|l| l.id() == list) else {
                return Ok(false);
            };
            if !row.remove(item) {
                return Ok(false);
            }
            self.audits.lock().unwrap().push(audit.clone());
            Ok(true)
        }

        async fn record_promotion(
            &self,
            list: &ListId,
            artifact: &ArtifactId,
            audit: &AuditEvent,
        ) -> Result<(), RepositoryError> {
            let mut rows = self.rows.lock().unwrap();
            let fault = std::mem::take(&mut *self.promotion_fault.lock().unwrap());
            let Some(row) = rows.iter_mut().find(|l| l.id() == list) else {
                return Err(RepositoryError::Conflict("unknown list".to_owned()));
            };
            let artifact = match fault {
                PromotionFault::None => artifact,
                PromotionFault::Fails => {
                    return Err(RepositoryError::Storage("pointer write failed".to_owned()));
                }
                // Another promotion anchored first; the write-once predicate
                // then refuses ours, and the row names *their* document.
                PromotionFault::LostToAnotherDocument(ref theirs) => {
                    *row = ItemList::from_parts(
                        row.id().clone(),
                        row.name().clone(),
                        row.items().to_vec(),
                        Some(theirs.clone()),
                    );
                    return Err(RepositoryError::Conflict("already promoted".to_owned()));
                }
            };
            if row.promoted_artifact().is_some() {
                return Err(RepositoryError::Conflict("already promoted".to_owned()));
            }
            *row = ItemList::from_parts(
                row.id().clone(),
                row.name().clone(),
                row.items().to_vec(),
                Some(artifact.clone()),
            );
            self.audits.lock().unwrap().push(audit.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeBlobs {
        blobs: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl FakeBlobs {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }
        fn get_text(&self, hash: &str) -> String {
            String::from_utf8(self.blobs.lock().unwrap()[hash].clone()).unwrap()
        }
        fn count(&self) -> usize {
            self.blobs.lock().unwrap().len()
        }
    }

    /// A deterministic stand-in for SHA-256. The fake only needs the two
    /// properties the port's contract rests on — same bytes ⇒ same address,
    /// different bytes ⇒ different address — and this layer has no digest
    /// dependency (invariant 3). Real hashing lives in
    /// `jarvis_infra::artifact_cas`, which is where it is tested.
    fn digest(bytes: &[u8]) -> Sha256 {
        let mut out = [0u8; 32];
        for (lane, slot) in out.iter_mut().enumerate() {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ (lane as u64);
            for b in bytes {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            *slot = (h >> ((lane % 8) * 8)) as u8;
        }
        Sha256::from_bytes(out)
    }

    #[async_trait::async_trait]
    impl BlobStore for FakeBlobs {
        async fn put(&self, bytes: &[u8]) -> Result<Sha256, BlobStoreError> {
            let hash = digest(bytes);
            self.blobs
                .lock()
                .unwrap()
                .insert(hash.to_string(), bytes.to_vec());
            Ok(hash)
        }
        async fn get(&self, hash: &Sha256) -> Result<Option<Vec<u8>>, BlobStoreError> {
            Ok(self.blobs.lock().unwrap().get(&hash.to_string()).cloned())
        }
        async fn contains(&self, hash: &Sha256) -> Result<bool, BlobStoreError> {
            Ok(self.blobs.lock().unwrap().contains_key(&hash.to_string()))
        }
    }

    #[derive(Default)]
    struct FakeArtifacts {
        manifests: Mutex<Vec<ArtifactManifest>>,
        audits: Mutex<Vec<AuditEvent>>,
    }

    impl FakeArtifacts {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }
        fn audit_payloads(&self) -> Vec<String> {
            self.audits
                .lock()
                .unwrap()
                .iter()
                .map(|a| a.payload_json.clone())
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl ArtifactStore for FakeArtifacts {
        async fn create_version(
            &self,
            manifest: &ArtifactManifest,
            audit: &AuditEvent,
        ) -> Result<(), RepositoryError> {
            let mut all = self.manifests.lock().unwrap();
            if all
                .iter()
                .any(|m| m.id() == manifest.id() && m.version() == manifest.version())
            {
                return Err(RepositoryError::Conflict("version exists".to_owned()));
            }
            all.push(manifest.clone());
            self.audits.lock().unwrap().push(audit.clone());
            Ok(())
        }
        async fn get(
            &self,
            id: &ArtifactId,
            version: ArtifactVersion,
        ) -> Result<Option<ArtifactManifest>, RepositoryError> {
            Ok(self
                .manifests
                .lock()
                .unwrap()
                .iter()
                .find(|m| m.id() == id && m.version() == version)
                .cloned())
        }
        async fn latest(
            &self,
            id: &ArtifactId,
        ) -> Result<Option<ArtifactManifest>, RepositoryError> {
            Ok(self
                .manifests
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.id() == id)
                .max_by_key(|m| m.version().get())
                .cloned())
        }
        async fn list_versions(
            &self,
            id: &ArtifactId,
        ) -> Result<Vec<ArtifactManifest>, RepositoryError> {
            let mut v: Vec<ArtifactManifest> = self
                .manifests
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.id() == id)
                .cloned()
                .collect();
            v.sort_by_key(|m| m.version().get());
            Ok(v)
        }
    }

    struct Harness {
        service: ListsService,
        lists: Arc<FakeLists>,
        blobs: Arc<FakeBlobs>,
        artifacts: Arc<FakeArtifacts>,
    }

    fn harness() -> Harness {
        let lists = FakeLists::new();
        let blobs = FakeBlobs::new();
        let artifacts = FakeArtifacts::new();
        let service = ListsService::new(
            lists.clone(),
            blobs.clone(),
            artifacts.clone(),
            Arc::new(ManualClock::at_unix(1_000_000)),
        );
        Harness {
            service,
            lists,
            blobs,
            artifacts,
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    // ---- the grammar path, end to end, with no model anywhere -------------

    #[tokio::test]
    async fn the_whole_grammar_round_trip_runs_without_a_model() {
        let h = harness();
        // "add milk to the shopping list" — the list does not exist yet.
        let add = parse_list_command("add milk to the shopping list").unwrap();
        let out = h.service.apply(&add, ids(1), ACTOR, &live()).await.unwrap();
        assert_eq!(out.list.items().len(), 1);
        assert_eq!(out.list.items()[0].text.as_str(), "milk");
        assert!(matches!(out.effect, CommandEffect::Added(_)));

        // "what's on the shopping list" — a pure query.
        let read = parse_list_command("what's on the shopping list").unwrap();
        let out = h
            .service
            .apply(&read, ids(2), ACTOR, &live())
            .await
            .unwrap();
        assert_eq!(out.effect, CommandEffect::Read);
        assert_eq!(out.list.items().len(), 1);

        // "check off milk on the shopping list" — resolved to an id first.
        let check = parse_list_command("check off milk on the shopping list").unwrap();
        let out = h
            .service
            .apply(&check, ids(3), ACTOR, &live())
            .await
            .unwrap();
        assert_eq!(out.effect, CommandEffect::CheckedOff(item_id(1)));
        assert!(out.list.items()[0].checked);
        assert_eq!(out.list.open_items().count(), 0);

        // "remove milk from the shopping list".
        let remove = parse_list_command("remove milk from the shopping list").unwrap();
        let out = h
            .service
            .apply(&remove, ids(4), ACTOR, &live())
            .await
            .unwrap();
        assert_eq!(out.effect, CommandEffect::Removed(item_id(1)));
        assert!(out.list.is_empty());

        // Every mutation audited, in order; the read audited nothing.
        assert_eq!(
            h.lists.audit_types(),
            vec![
                "list.created",
                "list.item_added",
                "list.item_checked",
                "list.item_removed",
            ]
        );
    }

    #[tokio::test]
    async fn a_read_of_an_unknown_list_answers_rather_than_creating_one() {
        let h = harness();
        let read = parse_list_command("what's on the shopping list").unwrap();
        let err = h
            .service
            .apply(&read, ids(1), ACTOR, &live())
            .await
            .unwrap_err();
        assert!(matches!(err, ListsError::UnknownList(_)));
        assert!(
            h.lists.list_all().await.unwrap().is_empty(),
            "a query must not mint a list"
        );
        assert!(h.lists.audit_types().is_empty());
    }

    #[tokio::test]
    async fn a_quick_note_lands_in_the_well_known_notes_list() {
        let h = harness();
        let note = parse_list_command("take a note: call the plumber").unwrap();
        let out = h
            .service
            .apply(&note, ids(1), ACTOR, &live())
            .await
            .unwrap();
        assert!(out.list.name().is_notes());
        assert_eq!(out.list.items()[0].text.as_str(), "call the plumber");

        // A second note joins the same list rather than making a second one.
        let note2 = parse_list_command("note that the boiler is loud").unwrap();
        let out = h
            .service
            .apply(&note2, ids(2), ACTOR, &live())
            .await
            .unwrap();
        assert_eq!(out.list.items().len(), 2);
        assert_eq!(h.lists.list_all().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn checking_off_something_not_on_the_list_is_reported_not_invented() {
        let h = harness();
        h.service
            .apply(
                &parse_list_command("add milk to the shopping list").unwrap(),
                ids(1),
                ACTOR,
                &live(),
            )
            .await
            .unwrap();
        let check = parse_list_command("check off bread on the shopping list").unwrap();
        let err = h
            .service
            .apply(&check, ids(2), ACTOR, &live())
            .await
            .unwrap_err();
        assert_eq!(err, ListsError::UnknownItem);
        // Nothing beyond the create + add was written.
        assert_eq!(h.lists.audit_types().len(), 2);
    }

    #[tokio::test]
    async fn a_lost_check_off_race_writes_nothing_and_reports_the_miss() {
        let h = harness();
        let mut list = ItemList::new(list_id(1), ListName::new("Shopping").unwrap());
        list.add(ListItem::new(item_id(1), text("milk"))).unwrap();
        h.lists.seed(list);
        // Another device removes it between our read and our write.
        h.lists
            .remove_item(
                &list_id(1),
                &item_id(1),
                &AuditEvent {
                    occurred_at: std::time::SystemTime::UNIX_EPOCH,
                    actor: "device:other".to_owned(),
                    event_type: "list.item_removed".to_owned(),
                    target: "list:x".to_owned(),
                    correlation_id: None,
                    payload_json: "{}".to_owned(),
                },
            )
            .await
            .unwrap();
        let err = h
            .service
            .set_checked(&list_id(1), &item_id(1), true, ACTOR, &live())
            .await
            .unwrap_err();
        assert_eq!(err, ListsError::UnknownItem);
    }

    #[tokio::test]
    async fn two_devices_naming_the_same_list_converge_on_one() {
        let h = harness();
        *h.lists.conflict_on_create.lock().unwrap() = true;
        let ensured = h
            .service
            .ensure_list(
                list_id(1),
                ListName::new("Shopping").unwrap(),
                ACTOR,
                &live(),
            )
            .await
            .unwrap();
        // We got the other creator's list, not a rival one — and we are honest
        // that we did not create it.
        assert!(!ensured.was_created());
        assert_eq!(ensured.list().id(), &list_id(99));
        assert_eq!(h.lists.list_all().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn ensure_list_says_whether_it_created_the_list_or_found_it() {
        let h = harness();
        let first = h
            .service
            .ensure_list(
                list_id(1),
                ListName::new("Shopping").unwrap(),
                ACTOR,
                &live(),
            )
            .await
            .unwrap();
        assert!(first.was_created());
        // The same list under a different spelling of the same normalized key.
        let second = h
            .service
            .ensure_list(
                list_id(2),
                ListName::new("shopping list").unwrap(),
                ACTOR,
                &live(),
            )
            .await
            .unwrap();
        assert!(
            !second.was_created(),
            "the second call found the list; it did not create one"
        );
        assert_eq!(second.list().id(), first.list().id());
        // Exactly one create was audited, because exactly one happened.
        assert_eq!(h.lists.audit_types(), vec!["list.created"]);
    }

    // ---- a losable race is a conflict, never an outage (R4) ---------------

    #[tokio::test]
    async fn an_append_that_loses_its_list_is_a_miss_not_a_storage_outage() {
        let h = harness();
        h.lists.seed(ItemList::new(
            list_id(1),
            ListName::new("Shopping").unwrap(),
        ));
        // The list is deleted between our read and our write.
        h.lists.fail_next_add(AddFault::ListVanished);
        let err = h
            .service
            .add_item(&list_id(1), item_id(1), text("milk"), ACTOR, &live())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ListsError::UnknownList(_)),
            "a vanished list is a 404, not a 503: got {err:?}"
        );
        assert!(
            !matches!(err, ListsError::Storage(_)),
            "a permanent conflict must never be reported as storage trouble"
        );
    }

    #[tokio::test]
    async fn an_append_the_store_bounds_is_full_not_a_storage_outage() {
        let h = harness();
        h.lists.seed(ItemList::new(
            list_id(1),
            ListName::new("Shopping").unwrap(),
        ));
        // Another writer fills the list to its bound before our insert lands,
        // so the store's own trigger refuses what our stale read allowed.
        h.lists.fail_next_add(AddFault::ListFilled);
        let err = h
            .service
            .add_item(&list_id(1), item_id(1), text("milk"), ACTOR, &live())
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ListsError::Invalid(ListError::Full),
            "the store's bound and the aggregate's bound are the same answer"
        );
    }

    #[tokio::test]
    async fn a_repository_conflict_is_never_flattened_into_storage() {
        // The mapping itself, independent of any call site: a conflict is a
        // losable race (409-shaped), a storage error is an outage (503-shaped),
        // and collapsing the first into the second tells the client and the ops
        // dashboard that a healthy service is sick.
        assert_eq!(
            ListsError::from(RepositoryError::Conflict("duplicate key".to_owned())),
            ListsError::Conflict("duplicate key".to_owned())
        );
        assert_eq!(
            ListsError::from(RepositoryError::Storage("connection reset".to_owned())),
            ListsError::Storage("connection reset".to_owned())
        );
        assert!(matches!(
            ListsError::from(RepositoryError::IdempotencyConflict),
            ListsError::Conflict(_)
        ));
    }

    #[tokio::test]
    async fn a_full_list_refuses_before_anything_is_written() {
        let h = harness();
        let mut list = ItemList::new(list_id(1), ListName::new("Shopping").unwrap());
        for n in 0..MAX_ITEMS_PER_LIST {
            let id: ListItemId = format!("01J8Z{n:021}").parse().unwrap();
            list.add(ListItem::new(id, text("x"))).unwrap();
        }
        h.lists.seed(list);
        let err = h
            .service
            .add_item(
                &list_id(1),
                item_id(1),
                text("one too many"),
                ACTOR,
                &live(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ListsError::Invalid(jarvis_domain::lists::ListError::Full)
        ));
        assert!(h.lists.audit_types().is_empty(), "nothing was written");
    }

    #[tokio::test]
    async fn a_cancelled_command_leaves_no_row_behind() {
        let h = harness();
        h.lists.seed(ItemList::new(
            list_id(1),
            ListName::new("Shopping").unwrap(),
        ));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = h
            .service
            .add_item(&list_id(1), item_id(1), text("milk"), ACTOR, &cancel)
            .await
            .unwrap_err();
        assert_eq!(err, ListsError::Cancelled);
        assert!(h.lists.stored(&list_id(1)).unwrap().is_empty());
        assert!(h.lists.audit_types().is_empty());
    }

    #[tokio::test]
    async fn audit_payloads_carry_ids_never_the_items_text() {
        let h = harness();
        h.service
            .apply(
                &parse_list_command("add my secret diary password to the shopping list").unwrap(),
                ids(1),
                ACTOR,
                &live(),
            )
            .await
            .unwrap();
        for payload in h.lists.audit_payloads() {
            assert!(
                !payload.contains("secret diary password"),
                "audit payload leaked list content: {payload}"
            );
            assert!(payload.starts_with("{\"listId\":\""), "{payload}");
        }
    }

    // ---- promotion (FR-08, ADR-024) ---------------------------------------

    #[tokio::test]
    async fn promotion_writes_a_versioned_markdown_artifact_with_escaped_content() {
        let h = harness();
        let mut list = ItemList::new(list_id(1), ListName::new("Shopping").unwrap());
        list.add(ListItem::new(item_id(1), text("milk"))).unwrap();
        list.add(ListItem::new(item_id(2), text("# not a heading")))
            .unwrap();
        h.lists.seed(list);

        let promoted = h
            .service
            .promote(&list_id(1), artifact_id(1), run_id(), ACTOR, &live())
            .await
            .unwrap();
        assert_eq!(promoted.version, 1);
        assert!(promoted.first_promotion);
        assert_eq!(promoted.artifact_id, artifact_id(1));

        let document = h.blobs.get_text(&promoted.sha256_hex);
        assert!(document.starts_with("# Shopping\n"));
        assert!(document.contains("- [ ] milk"));
        assert!(
            document.contains("- [ ] \\# not a heading"),
            "item text must not become a heading: {document}"
        );

        // The manifest's audit payload names ids and counts, never content.
        let payload = &h.artifacts.audit_payloads()[0];
        assert!(payload.contains("\"itemCount\":2"));
        assert!(!payload.contains("not a heading"));

        // The list now remembers its document.
        let stored = h.lists.stored(&list_id(1)).unwrap();
        assert_eq!(stored.promoted_artifact(), Some(&artifact_id(1)));
        assert_eq!(
            h.lists.audit_types().last().map(String::as_str),
            Some("list.promoted")
        );
    }

    #[tokio::test]
    async fn a_second_promotion_versions_the_same_document_rather_than_forking_it() {
        let h = harness();
        let mut list = ItemList::new(list_id(1), ListName::new("Shopping").unwrap());
        list.add(ListItem::new(item_id(1), text("milk"))).unwrap();
        h.lists.seed(list);

        let first = h
            .service
            .promote(&list_id(1), artifact_id(1), run_id(), ACTOR, &live())
            .await
            .unwrap();
        h.service
            .add_item(&list_id(1), item_id(2), text("eggs"), ACTOR, &live())
            .await
            .unwrap();
        // A *different* fresh id is offered; it must be ignored in favour of
        // the artifact the list is already bound to.
        let second = h
            .service
            .promote(&list_id(1), artifact_id(2), run_id(), ACTOR, &live())
            .await
            .unwrap();

        assert_eq!(second.artifact_id, first.artifact_id);
        assert_eq!(second.version, 2);
        assert!(!second.first_promotion);
        assert_ne!(second.sha256_hex, first.sha256_hex);
        assert_eq!(
            h.artifacts
                .list_versions(&artifact_id(1))
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            h.artifacts
                .list_versions(&artifact_id(2))
                .await
                .unwrap()
                .is_empty(),
            "no rival document was minted"
        );
        // Both versions' bytes are in the blob store; the older one is intact.
        assert_eq!(h.blobs.count(), 2);
        assert!(!h.blobs.get_text(&first.sha256_hex).contains("eggs"));
    }

    #[tokio::test]
    async fn a_cancelled_promotion_stores_no_blob_and_no_manifest() {
        let h = harness();
        h.lists.seed(ItemList::new(
            list_id(1),
            ListName::new("Shopping").unwrap(),
        ));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = h
            .service
            .promote(&list_id(1), artifact_id(1), run_id(), ACTOR, &cancel)
            .await
            .unwrap_err();
        assert_eq!(err, ListsError::Cancelled);
        assert_eq!(h.blobs.count(), 0);
        assert!(
            h.artifacts
                .list_versions(&artifact_id(1))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_promotion_that_fails_half_way_never_forks_the_document() {
        // The two halves of a promotion are two transactions and cannot be one.
        // What must hold across a failure between them is that the list ends up
        // with ONE document identity — the thing 0012's `lists_guard` trigger
        // and ADR-024 exist to protect. Anchoring the pointer first is what
        // makes that true: if the pointer never lands, no artifact was written
        // for it either, so there is nothing to fork from.
        let h = harness();
        let mut list = ItemList::new(list_id(1), ListName::new("Shopping").unwrap());
        list.add(ListItem::new(item_id(1), text("milk"))).unwrap();
        h.lists.seed(list);

        h.lists.fail_next_promotion(PromotionFault::Fails);
        let err = h
            .service
            .promote(&list_id(1), artifact_id(1), run_id(), ACTOR, &live())
            .await
            .unwrap_err();
        assert!(matches!(err, ListsError::Storage(_)), "{err:?}");
        assert!(
            h.lists
                .stored(&list_id(1))
                .unwrap()
                .promoted_artifact()
                .is_none(),
            "the failed promotion left no pointer"
        );
        assert!(
            h.artifacts
                .list_versions(&artifact_id(1))
                .await
                .unwrap()
                .is_empty(),
            "no document may exist that the list does not know about — that \
             orphan is what the NEXT promotion would fork away from"
        );

        // The owner tries again. A different fresh id is offered, as it would
        // be: the handler mints one per request.
        let retry = h
            .service
            .promote(&list_id(1), artifact_id(2), run_id(), ACTOR, &live())
            .await
            .unwrap();
        assert_eq!(retry.version, 1);
        assert!(retry.first_promotion);
        assert_eq!(retry.artifact_id, artifact_id(2));
        assert_eq!(
            h.lists.stored(&list_id(1)).unwrap().promoted_artifact(),
            Some(&artifact_id(2))
        );
        // Exactly one document exists for this list, across both attempts.
        assert!(
            h.artifacts
                .list_versions(&artifact_id(1))
                .await
                .unwrap()
                .is_empty(),
            "the abandoned attempt must not have left a rival document"
        );
        assert_eq!(
            h.artifacts
                .list_versions(&artifact_id(2))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_promotion_whose_first_version_never_landed_is_finished_not_refused() {
        // The other side of anchoring the pointer first: the pointer is set but
        // the artifact write did not land (the process died, the artifact store
        // was briefly down). The list names a document with no versions, and the
        // next promotion must CREATE version 1 of that document rather than
        // failing forever — or minting a second one.
        let h = harness();
        h.lists.seed(ItemList::from_parts(
            list_id(1),
            ListName::new("Shopping").unwrap(),
            vec![ListItem::new(item_id(1), text("milk"))],
            Some(artifact_id(1)),
        ));

        let promoted = h
            .service
            .promote(&list_id(1), artifact_id(9), run_id(), ACTOR, &live())
            .await
            .unwrap();
        assert_eq!(
            promoted.artifact_id,
            artifact_id(1),
            "the document the list already names, not the fresh id"
        );
        assert_eq!(promoted.version, 1);
        assert!(promoted.first_promotion, "version 1 created the document");
        assert!(
            h.artifacts
                .list_versions(&artifact_id(9))
                .await
                .unwrap()
                .is_empty(),
            "the offered fresh id was rightly ignored"
        );
    }

    #[tokio::test]
    async fn a_promotion_that_loses_the_anchor_race_adopts_the_winner_s_document() {
        // Two promotions in flight; the other one anchored first, to its own
        // artifact. Ours must join that document, not mint a rival for the same
        // list.
        let h = harness();
        h.lists.seed(ItemList::new(
            list_id(1),
            ListName::new("Shopping").unwrap(),
        ));
        h.lists
            .fail_next_promotion(PromotionFault::LostToAnotherDocument(artifact_id(7)));

        let promoted = h
            .service
            .promote(&list_id(1), artifact_id(1), run_id(), ACTOR, &live())
            .await
            .unwrap();
        assert_eq!(promoted.artifact_id, artifact_id(7), "we adopted theirs");
        assert!(
            h.artifacts
                .list_versions(&artifact_id(1))
                .await
                .unwrap()
                .is_empty(),
            "our fresh id never became a document"
        );
        assert_eq!(
            h.lists.stored(&list_id(1)).unwrap().promoted_artifact(),
            Some(&artifact_id(7)),
            "the list keeps the one identity it was given"
        );
    }

    #[tokio::test]
    async fn promoting_an_unknown_list_is_a_clean_miss() {
        let h = harness();
        let err = h
            .service
            .promote(&list_id(7), artifact_id(1), run_id(), ACTOR, &live())
            .await
            .unwrap_err();
        assert!(matches!(err, ListsError::UnknownList(_)));
        assert_eq!(h.blobs.count(), 0);
    }
}
