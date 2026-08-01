//! List and quick-note wire DTOs (FR-34, docs/02 §11e, ADR-024).
//!
//! Three surfaces:
//!
//! * [`ListDto`] — one list with its items, as `GET /api/v1/lists/{id}` returns
//!   it and as the HUD list card ([`crate::cards::HudCardDto::List`]) renders
//!   it.
//! * The write requests — `POST /api/v1/lists`, `POST /api/v1/lists/{id}/items`,
//!   the check-off `PATCH`, and `POST /api/v1/lists/command`, which runs the
//!   **deterministic grammar** over one utterance. All owner-driven: a human on
//!   their own paired device, not a model proposal (see the module doc on
//!   `jarvisd::lists` for why that matters to invariant 1).
//! * [`PromoteListResponse`] — `POST /api/v1/lists/{id}/promote`, the FR-08
//!   promotion of a grown list into a versioned markdown artifact.
//!
//! `name` and `text` are human text, sanitized by `jarvis_domain::lists` before
//! they are projected here; the client renders them as **text only, never
//! markup** (docs/12 §9 card grammar).
//!
//! Note what is **absent**: there is no shared-with, no assignee, and no due
//! date. Sharing lists across users is explicitly out of scope for single-owner
//! v1 (ADR-024), and "remind me at six" is a timer (FR-33), not a list item —
//! the wire shape says so rather than leaving it to a comment.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One line on a list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListItemDto {
    #[schemars(with = "crate::schema::UlidString")]
    pub id: jarvis_domain::ids::ListItemId,
    /// Sanitized human text. Rendered as text.
    pub text: String,
    pub checked: bool,
}

impl From<&jarvis_domain::lists::ListItem> for ListItemDto {
    fn from(item: &jarvis_domain::lists::ListItem) -> Self {
        Self {
            id: item.id.clone(),
            text: item.text.as_str().to_owned(),
            checked: item.checked,
        }
    }
}

/// One named list with its items, in insertion order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListDto {
    #[schemars(with = "crate::schema::UlidString")]
    pub id: jarvis_domain::ids::ListId,
    /// Display name as the owner gave it ("Shopping"), sanitized.
    pub name: String,
    pub items: Vec<ListItemDto>,
    /// How many items are still open — the readback's "two things left"
    /// computed once server-side so the card and the spoken answer agree.
    pub open_count: u32,
    /// The versioned artifact this list has been promoted into, if any (FR-08).
    /// Present ⇒ a further promotion adds a *version* to this artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<crate::schema::UlidString>")]
    pub promoted_artifact_id: Option<jarvis_domain::ids::ArtifactId>,
    /// The list has grown into a document and promotion is worth offering
    /// (ADR-024). An offer the shell surfaces, never an automatic conversion.
    pub promotion_offered: bool,
}

impl From<&jarvis_domain::lists::ItemList> for ListDto {
    fn from(list: &jarvis_domain::lists::ItemList) -> Self {
        Self {
            id: list.id().clone(),
            name: list.name().as_str().to_owned(),
            items: list.items().iter().map(ListItemDto::from).collect(),
            open_count: u32::try_from(list.open_items().count()).unwrap_or(u32::MAX),
            promoted_artifact_id: list.promoted_artifact().cloned(),
            promotion_offered: list.should_offer_promotion(),
        }
    }
}

/// `GET /api/v1/lists` — every list the owner has, name-ordered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListIndexResponse {
    pub lists: Vec<ListDto>,
}

/// `POST /api/v1/lists` — create a named list, or return the existing one with
/// the same normalized name (creation is idempotent on the name key, because
/// "add milk to the shopping list" must work before a shopping list exists).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateListRequest {
    pub name: String,
}

/// `POST /api/v1/lists/{id}/items` — append one line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddListItemRequest {
    pub text: String,
}

/// `PATCH /api/v1/lists/{id}/items/{itemId}` — the card's check-off tap, and
/// from M5 the voice path. Deliberately a whole-value set rather than a toggle:
/// two devices tapping the same line converge on the same result instead of
/// racing each other back and forth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckListItemRequest {
    pub checked: bool,
}

/// `POST /api/v1/lists/command` — run the **deterministic grammar** over one
/// utterance (ADR-024). Zero model calls: an utterance the grammar does not
/// unambiguously recognize is a `list.unrecognized_command` 422, never a guess.
/// 422 rather than 400 because the body was perfectly valid — it is the
/// *content* that did not resolve here, and the caller's answer is to fall back
/// to the normal run path rather than to fix its request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListCommandRequest {
    /// The utterance as spoken or typed, e.g. "add milk to the shopping list".
    pub utterance: String,
}

/// What a grammar command did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ListEffectDto {
    Added,
    Removed,
    CheckedOff,
    /// A pure query — nothing was written and nothing was audited.
    Read,
}

/// The list after the command was applied and audited, so the card re-renders
/// without waiting for an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListCommandResponse {
    pub list: ListDto,
    pub effect: ListEffectDto,
    /// The item the command touched, absent for a read.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<crate::schema::UlidString>")]
    pub item_id: Option<jarvis_domain::ids::ListItemId>,
}

/// `POST /api/v1/lists/{id}/promote` — the list is now a versioned markdown
/// artifact (FR-08). A list promoted before gets the **next version** of the
/// same artifact, never a rival document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PromoteListResponse {
    #[schemars(with = "crate::schema::UlidString")]
    pub artifact_id: jarvis_domain::ids::ArtifactId,
    pub version: u32,
    /// Content address of the document, lowercase hex — the same value the
    /// artifact blob endpoint serves under.
    pub sha256: String,
    /// True when this promotion created the artifact rather than versioning it.
    pub first_promotion: bool,
}
