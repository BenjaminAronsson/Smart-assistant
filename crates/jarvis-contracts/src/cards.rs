//! HUD card grammar v1 (docs/12 §2.3, FR-09). The materialization canvas
//! renders **registered card types only** — this union is the registry. It is
//! also a security boundary (invariant #1, docs/12 §2.3/§9): the model
//! proposes card *content* through these narrow, typed fields; it never
//! proposes layout or HTML. There is no field on any variant below that the
//! client is allowed to interpolate as markup — every renderer treats every
//! field as plain text (docs/12 §9 "card grammar only").
//!
//! F3b.2 ships the grammar and its Angular renderers with **no server-side
//! producer** — no WS event carries a [`HudCardDto`] yet, the same
//! no-producer-less-replayable-event precedent as `crate::artifacts` (see that
//! module's doc comment). The first producer is the deep-dive work (F3b.6).
//! The v1 set is exactly the types F3b.2 owns per docs/milestones/M3-features.md:
//! value readout, place, entity/person, media/menu grid, headlines/digest,
//! now-playing (data only — live playback control stays on the media bar until
//! M5), approval (wire-reused from [`crate::approvals::ApprovalCardDto`], never
//! re-modeled), status/queued, and error. Timer/sources/gallery/map/product/
//! agenda cards are later features (F3b.5/6/7) and are deliberately absent —
//! adding one is an additive enum variant, never a change to this one's shape.
//! F3b.8 took exactly that route for the **list** card
//! ([`HudCardDto::List`], FR-34/ADR-024).
//!
//! **Source-chip is structurally unavoidable** (docs/12 §2.3, FR-25/ADR-014).
//! [`SourcedImageDto`] is the *only* way any card carries an image, and its
//! `source_url`/`source_domain`/`alt` fields are required, not optional
//! metadata bolted on afterward — a card cannot reference a web image without
//! also carrying its attribution and alt text in the same value. A card with no
//! extractable image simply omits the `Option<SourcedImageDto>` field and
//! renders text-only (docs/12 §2.3).

use crate::approvals::ApprovalCardDto;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A card image sourced from the web (docs/12 §2.3, FR-25/ADR-014 — person,
/// place, weather, menu photos absent a dedicated integration). `url` is the
/// image itself; `source_url`/`source_domain` are the page it was found on and
/// its chip label (e.g. "wikipedia.org"), computed once server-side so the
/// client never parses an untrusted URL to render trusted-looking text. `alt`
/// is required alt text (docs/12 §8 accessibility) — there is no constructor
/// path that produces an image without it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourcedImageDto {
    /// The `https` image URL.
    pub url: String,
    /// The page the image was found on — the source-chip's link target.
    pub source_url: String,
    /// Display domain for the chip, e.g. "wikipedia.org ↗".
    pub source_domain: String,
    /// Required alt text — never empty.
    pub alt: String,
}

/// One labeled value in a value-readout card's mini-stats row (docs/12 §2.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MiniStatDto {
    pub label: String,
    /// Display text, e.g. "68%" — rendered tabular-nums client-side, not
    /// computed there, so a mixed-format value never breaks alignment.
    pub value: String,
}

/// One tile of a media/menu grid card (docs/12 §2.3: "photo + name + price").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaGridItemDto {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub photo: Option<SourcedImageDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
}

/// One item of a headlines/digest card (docs/12 §2.3: "3-5 short items, each a
/// one-line title + one-line summary + relative time + source link"). The item
/// carries its own source link independent of `thumbnail`'s attribution,
/// because a digest's items may each come from a different page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeadlineItemDto {
    pub title: String,
    pub summary: String,
    /// Pre-formatted relative time (e.g. "2h ago"), computed once server-side
    /// — the client renders it as-is rather than ticking a live clock against
    /// a fact that is not actually live.
    pub relative_time: String,
    pub source_url: String,
    pub source_domain: String,
    /// Thumbnail is optional (docs/12 §2.3: "no photos required, thumbnail
    /// optional"); when present it still carries its own attribution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<SourcedImageDto>,
}

/// Registered HUD card types (docs/12 §2.3). The `type` discriminator is
/// dotted-namespaced (`card.value_readout`, …), matching the envelope/event
/// convention. **Strict, no catch-all** — every card is authored by jarvisd
/// itself (there is no third-party card producer), the same reasoning as
/// [`crate::events::DomainEvent`]. The client-side defense for a genuinely
/// unrecognized discriminant (a future contract version, a malformed payload)
/// is the Angular `hud-card` switch component degrading to the error card —
/// belt-and-suspenders, not a substitute for this union staying the single
/// source of truth for what "registered" means.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum HudCardDto {
    /// Hero number with optional staggered mini-stats (docs/12 §2.3): weather,
    /// a single metric, a quick count.
    #[serde(rename = "card.value_readout")]
    ValueReadout {
        id: String,
        label: String,
        /// The hero value as display text (e.g. "72°F") — the client applies
        /// tabular-nums and the count-up animation, it does not compute the
        /// value.
        value: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mini_stats: Vec<MiniStatDto>,
    },
    /// A place result (docs/12 §2.3): photo, rating/distance/price pills, and
    /// the `pick` variant (hue ring) marking a top recommendation.
    #[serde(rename = "card.place")]
    Place {
        id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        photo: Option<SourcedImageDto>,
        #[serde(skip_serializing_if = "Option::is_none")]
        rating: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        distance: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        price_level: Option<String>,
        /// Marks the top recommendation among a set of place cards — rendered
        /// with a hue ring (docs/12 §2.3), never a colour picked ad hoc.
        #[serde(default)]
        pick: bool,
    },
    /// An entity/person result (docs/12 §2.3): photo, confidence, facts.
    #[serde(rename = "card.entity")]
    Entity {
        id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        photo: Option<SourcedImageDto>,
        /// 0-100 confidence in the entity resolution.
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence_pct: Option<u8>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        facts: Vec<String>,
    },
    /// Media/menu grid (docs/12 §2.3: "photo + name + price").
    #[serde(rename = "card.media_grid")]
    MediaGrid {
        id: String,
        title: String,
        items: Vec<MediaGridItemDto>,
    },
    /// Headlines/digest (docs/12 §2.3: several current items, not one fact —
    /// distinct from a single entity/value-readout card for that reason).
    #[serde(rename = "card.headlines")]
    Headlines {
        id: String,
        title: String,
        items: Vec<HeadlineItemDto>,
    },
    /// "What's playing" as a first-class query (docs/12 §2.3, FR-32/ADR-022):
    /// **data only** — this variant answers a query, it does not add playback
    /// controls. Live control stays on the media bar (`crate::media`); a
    /// control surface on this card is M5 (FR-32). Album art here is the
    /// player's own content, not third-party web content, so it is a plain
    /// (already `https`-validated) URL rather than a [`SourcedImageDto`] — no
    /// source chip is owed for a player showing its own art, matching the
    /// media bar's existing treatment.
    #[serde(rename = "card.now_playing")]
    NowPlaying {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        artist: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        album: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        art_url: Option<String>,
        source_app: String,
    },
    /// A named list with its items (docs/12 §2.3, FR-34/ADR-024): the face of
    /// "what's on the shopping list", with check-off by voice or tap. The list
    /// itself is wire-reused from [`crate::lists::ListDto`] rather than
    /// re-modeled, the same discipline as the approval variant below — there is
    /// exactly one type that says what a list is on the wire.
    ///
    /// `listId` rides alongside the card's own `id` because the check-off tap
    /// posts to `/api/v1/lists/{listId}/items/{itemId}`: a card id is a
    /// presentation handle, never an address the client may derive a write from.
    #[serde(rename = "card.list")]
    List {
        id: String,
        #[schemars(with = "crate::schema::UlidString")]
        list_id: jarvis_domain::ids::ListId,
        list: crate::lists::ListDto,
    },
    /// The approval surface, wire-reused verbatim (docs/06 §3) — never
    /// re-modeled as a distinct card shape, so there is exactly one type that
    /// carries `exactEffect`/`proposedArguments` on the wire.
    #[serde(rename = "card.approval")]
    Approval { card: ApprovalCardDto },
    /// A transient status readout (docs/12 §2.3), e.g. a run parked in
    /// degraded-mode queueing (FR-12).
    #[serde(rename = "card.status")]
    Status {
        id: String,
        message: String,
        #[serde(default)]
        queued: bool,
    },
    /// The fallback face for a failure — and the client's own degrade target
    /// for an unrecognized discriminant (docs/12 §9).
    #[serde(rename = "card.error")]
    Error { id: String, message: String },
}

impl HudCardDto {
    /// The `type` discriminator for this card. Kept in lockstep with the
    /// `#[serde(rename)]` tags above (same pattern as
    /// [`crate::events::DomainEvent::event_type`]).
    pub fn card_type(&self) -> &'static str {
        match self {
            Self::ValueReadout { .. } => "card.value_readout",
            Self::Place { .. } => "card.place",
            Self::Entity { .. } => "card.entity",
            Self::MediaGrid { .. } => "card.media_grid",
            Self::Headlines { .. } => "card.headlines",
            Self::NowPlaying { .. } => "card.now_playing",
            Self::List { .. } => "card.list",
            Self::Approval { .. } => "card.approval",
            Self::Status { .. } => "card.status",
            Self::Error { .. } => "card.error",
        }
    }
}
