//! Deep-dive wire DTOs (F3b.6, FR-27, ADR-017, docs/12 §2.3/§2.5).
//!
//! Two surfaces:
//!
//! * [`HudCanvasDto`] — the payload of the transient `hud.canvas` event
//!   ([`crate::events::TransientEvent::HudCanvas`]), which is what turns the
//!   server's continuation-vs-new-topic decision into something the HUD acts
//!   on, and what carries the deep-dive cards (`card.sources`, `card.gallery`)
//!   and the list card (`card.list`) onto the canvas. It is the **first and
//!   only producer** of [`crate::cards::HudCardDto`] on the wire — see that
//!   module's doc comment for the no-producer precedent it retires.
//! * [`DeepDiveFindingsRequest`] / [`PromoteNotesResponse`] — the REST entry
//!   points that file what a turn consulted and accept the promotion offer.
//!
//! **The canvas event is transient, deliberately** (docs/05 §3). A canvas
//! instruction is not timeline history: panels shelve, expire silently on a TTL
//! and are dismissible (docs/12 §4), so a client that missed one is not missing
//! a fact about the past — the next turn corrects it, and the *durable* record
//! of a deep dive is the Research Notes artifact (FR-08), which has its own
//! replayable read surface. It also could not honestly be a
//! [`crate::events::DomainEvent`]: those are published from the outbox, in the
//! same transaction as the domain change they describe (invariant #6), and a
//! deep-dive turn commits no row.
//!
//! **Nothing here grants authority** (invariant #1). [`SourceHandoffDto`] names
//! a page Jarvis already cited — the same URL and domain the sources card
//! already carries — and says the human asked to read it. It carries no tool id
//! and no arguments, and there is no endpoint that takes it back: opening a page
//! is a browser handoff (ADR-017 §3) that reaches the browser worker only as a
//! `ToolProposal` through `policy::evaluate`, like any other tool call.

use crate::cards::HudCardDto;
use jarvis_domain::ids::SessionId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What this turn does to the materialization canvas (FR-24/FR-27, docs/12
/// §2.5/§4) — the wire mirror of `jarvis_application::deepdive::CanvasAction`.
///
/// Exhaustive and deliberately two-valued: a third answer would change the
/// panel lifecycle, which is a spec decision. Note what it cannot express —
/// anything about approvals. Pending approval cards are exempt from shelving
/// (docs/12 §4, F3b.4), and no value here can retract one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CanvasActionDto {
    /// A continuation: append these cards, leave the prior ones in place.
    Extend,
    /// A genuine topic change: shelve the current panels under `label` first.
    Shelve,
}

/// "Open that / let me read it" resolved to a page the thread already cited
/// (ADR-017 §3).
///
/// A **citation, not a command**: `url` and `domain` are already on the sources
/// card, `domain` is computed host-side from the parsed host
/// (`jarvis_domain::deepdive::display_domain`) so a spoofing URL cannot present
/// itself as a trusted site, and there is no tool id here for a client to
/// submit anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceHandoffDto {
    /// The page to open; always `http(s)`, validated when it was recorded.
    pub url: String,
    /// Display domain for what Jarvis says while handing off, e.g.
    /// "en.wikipedia.org".
    pub domain: String,
}

/// One canvas instruction (F3b.6). The payload of the transient `hud.canvas`
/// event.
///
/// `cards` is the **live card set for this canvas**, not a delta: the same card
/// id re-sent is the same card refreshed, so a client that applies it
/// upsert-by-id converges on the current state no matter how many events it
/// missed. That is what makes a transient classification safe here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HudCanvasDto {
    /// The conversation this turn belongs to. Absent for a canvas update that
    /// is not part of a deep-dive thread at all — a list card produced by the
    /// deterministic list grammar (FR-34), which has no session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<crate::schema::UlidString>")]
    pub session_id: Option<SessionId>,
    pub action: CanvasActionDto,
    /// The shelf chip label to file the *displaced* panels under when `action`
    /// is `shelve` (docs/12 §4: "Ramen places · Restore · ×"). Plain text.
    pub label: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cards: Vec<HudCardDto>,
    /// The spoken offer to keep this thread as a Research Notes document
    /// (docs/12 §2.5: Jarvis's normal voice, one line, never a dialog box).
    /// Present only on the turn that crosses `[ui] deepdive_promote_after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer: Option<String>,
    /// Present when the human asked to read one of the cited sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<SourceHandoffDto>,
}

/// One page a turn consulted, as filed to `POST
/// /api/v1/sessions/{id}/deepdive/findings`.
///
/// Both fields are untrusted (Z4 — they come from a fetched page). The URL is
/// re-validated by `ResearchThread::record_source`, which refuses anything that
/// is not an attributable `http(s)` URL, and the title is capped and escaped
/// wherever it is rendered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceFindingDto {
    pub title: String,
    pub url: String,
}

/// One image a turn referenced, with **its own** page (ADR-017: provenance
/// differs per image, so one shared attribution is not acceptable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImageFindingDto {
    pub alt: String,
    pub url: String,
    /// The page this image was found on — never a neighbour's.
    pub source_url: String,
}

/// `POST /api/v1/sessions/{id}/deepdive/findings` — what this turn learned.
///
/// `facts` are **paraphrases Jarvis composed**, not fetched page text: the
/// domain refuses anything over
/// `jarvis_domain::deepdive::MAX_PARAPHRASE_CHARS` rather than truncating it,
/// because a truncated scrape is still a scrape (ADR-017). Everything in this
/// request is filed through the thread's guarded recorders; a rejected entry is
/// reported in [`DeepDiveFindingsResponse::refused`] and simply does not exist
/// in the thread.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeepDiveFindingsRequest {
    #[serde(default)]
    pub facts: Vec<String>,
    #[serde(default)]
    pub sources: Vec<SourceFindingDto>,
    #[serde(default)]
    pub images: Vec<ImageFindingDto>,
}

/// What was actually filed. Counts, plus a plain-text reason per rejected
/// entry so a caller learns *that* a scrape or an unattributable URL was
/// refused rather than discovering it missing later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeepDiveFindingsResponse {
    pub facts: u32,
    pub sources: u32,
    pub images: u32,
    /// One line per refused entry. Empty when everything was filed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<String>,
}

/// `POST /api/v1/sessions/{id}/deepdive/promote` — the human accepted the
/// offer; the thread is now a versioned markdown artifact (FR-08).
///
/// Re-promoting the same thread appends a version to the same document rather
/// than minting a rival one, the same shape as
/// [`crate::lists::PromoteListResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PromoteNotesResponse {
    #[schemars(with = "crate::schema::UlidString")]
    pub artifact_id: jarvis_domain::ids::ArtifactId,
    pub version: u32,
    /// Content address of the document, lowercase hex.
    pub sha256: String,
    /// True when this promotion created the document rather than versioning it.
    pub first_promotion: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_canvas_instruction_round_trips_and_omits_what_it_does_not_carry() {
        let dto = HudCanvasDto {
            session_id: None,
            action: CanvasActionDto::Extend,
            label: "Ramen places".to_owned(),
            cards: Vec::new(),
            offer: None,
            handoff: None,
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "action": "extend", "label": "Ramen places" })
        );
        assert_eq!(
            serde_json::from_value::<HudCanvasDto>(json).unwrap(),
            dto.clone()
        );
    }

    #[test]
    fn the_handoff_carries_a_citation_and_nothing_executable() {
        let json = serde_json::to_value(SourceHandoffDto {
            url: "https://en.wikipedia.org/wiki/Ramen".to_owned(),
            domain: "en.wikipedia.org".to_owned(),
        })
        .unwrap();
        let object = json.as_object().unwrap();
        assert_eq!(object.len(), 2);
        for forbidden in ["toolId", "arguments", "proposal", "grant"] {
            assert!(
                !object.contains_key(forbidden),
                "{forbidden} must be absent"
            );
        }
    }

    #[test]
    fn findings_default_to_empty_so_a_partial_body_is_not_a_decode_error() {
        let parsed: DeepDiveFindingsRequest =
            serde_json::from_str(r#"{"facts":["Kome opens at noon."]}"#).unwrap();
        assert_eq!(parsed.facts.len(), 1);
        assert!(parsed.sources.is_empty());
        assert!(parsed.images.is_empty());
    }
}
