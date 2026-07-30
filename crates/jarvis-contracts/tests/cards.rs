//! F3b.2: the HUD card grammar wire shapes (docs/12 §2.3, FR-09, FR-25/ADR-014).
//!
//! Two properties matter more than the rest: every card round-trips with its
//! dotted `type` tag, and **no card can carry a web image without its
//! attribution** — `SourcedImageDto`'s `sourceUrl`/`sourceDomain`/`alt` fields
//! are required, so a JSON payload that omits any of them fails to decode
//! rather than rendering an unattributed photo.

use jarvis_contracts::approvals::{ApprovalCardDto, DataEgressDto, RiskLevelDto};
use jarvis_contracts::cards::{
    HeadlineItemDto, HudCardDto, MediaGridItemDto, MiniStatDto, SourcedImageDto,
};
use serde_json::json;

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const APPROVAL: &str = "01BX5ZZKBKACTAV9WEVGEMMVS1";

fn photo() -> SourcedImageDto {
    SourcedImageDto {
        url: "https://cdn.example/ramen.jpg".into(),
        source_url: "https://en.wikipedia.org/wiki/Ramen".into(),
        source_domain: "wikipedia.org".into(),
        alt: "A bowl of ramen".into(),
    }
}

fn every_card() -> Vec<HudCardDto> {
    vec![
        HudCardDto::ValueReadout {
            id: "card-1".into(),
            label: "Weather in Berlin".into(),
            value: "72°F".into(),
            mini_stats: vec![MiniStatDto {
                label: "Humidity".into(),
                value: "68%".into(),
            }],
        },
        HudCardDto::Place {
            id: "card-2".into(),
            name: "Kome Ramen".into(),
            photo: Some(photo()),
            rating: Some("4.7".into()),
            distance: Some("8 min".into()),
            price_level: Some("$$".into()),
            pick: true,
        },
        HudCardDto::Entity {
            id: "card-3".into(),
            name: "Ada Lovelace".into(),
            photo: Some(photo()),
            confidence_pct: Some(92),
            facts: vec!["Mathematician".into(), "Wrote the first algorithm".into()],
        },
        HudCardDto::MediaGrid {
            id: "card-4".into(),
            title: "Menu".into(),
            items: vec![MediaGridItemDto {
                name: "Tonkotsu".into(),
                photo: Some(photo()),
                price: Some("$14".into()),
            }],
        },
        HudCardDto::Headlines {
            id: "card-5".into(),
            title: "World Cup".into(),
            items: vec![HeadlineItemDto {
                title: "Final set for Sunday".into(),
                summary: "Two sides confirmed after semifinal wins.".into(),
                relative_time: "2h ago".into(),
                source_url: "https://news.example/wc".into(),
                source_domain: "news.example".into(),
                thumbnail: None,
            }],
        },
        HudCardDto::NowPlaying {
            id: "card-6".into(),
            title: Some("Dancing Queen".into()),
            artist: Some("ABBA".into()),
            album: Some("Arrival".into()),
            art_url: Some("https://cdn.example/art.jpg".into()),
            source_app: "Spotify".into(),
        },
        HudCardDto::Approval {
            card: ApprovalCardDto {
                approval_id: APPROVAL.parse().unwrap(),
                run_id: RUN.parse().unwrap(),
                tool_id: "message.send".into(),
                exact_effect: "message.send {to=\"bob@example.com\"}".into(),
                proposed_arguments: json!({ "to": "bob@example.com" }),
                risk: RiskLevelDto::R2,
                reversible: false,
                egress: DataEgressDto::External,
            },
        },
        HudCardDto::Status {
            id: "card-7".into(),
            message: "Queued — provider recovering".into(),
            queued: true,
        },
        HudCardDto::Error {
            id: "card-8".into(),
            message: "Could not load this result".into(),
        },
    ]
}

#[test]
fn every_card_round_trips_and_carries_its_type_tag() {
    for card in every_card() {
        let value = serde_json::to_value(&card).unwrap();
        assert_eq!(value["type"], card.card_type());
        let back: HudCardDto = serde_json::from_value(value).unwrap();
        assert_eq!(back, card);
    }
}

#[test]
fn card_type_tags_are_dotted_and_disjoint() {
    let mut tags: Vec<&str> = every_card().iter().map(|c| c.card_type()).collect();
    tags.sort_unstable();
    tags.dedup();
    assert_eq!(
        tags,
        [
            "card.approval",
            "card.entity",
            "card.error",
            "card.headlines",
            "card.media_grid",
            "card.now_playing",
            "card.place",
            "card.status",
            "card.value_readout",
        ]
    );
}

#[test]
fn value_readout_serializes_camel_case() {
    let value = serde_json::to_value(HudCardDto::ValueReadout {
        id: "card-1".into(),
        label: "Weather".into(),
        value: "72°F".into(),
        mini_stats: vec![MiniStatDto {
            label: "Humidity".into(),
            value: "68%".into(),
        }],
    })
    .unwrap();
    assert_eq!(
        value,
        json!({
            "type": "card.value_readout",
            "id": "card-1",
            "label": "Weather",
            "value": "72°F",
            "miniStats": [{ "label": "Humidity", "value": "68%" }],
        })
    );
}

#[test]
fn value_readout_omits_empty_mini_stats() {
    let value = serde_json::to_value(HudCardDto::ValueReadout {
        id: "card-1".into(),
        label: "Weather".into(),
        value: "72°F".into(),
        mini_stats: vec![],
    })
    .unwrap();
    assert!(value.get("miniStats").is_none());
}

#[test]
fn place_card_omits_absent_optional_fields_and_defaults_pick_false() {
    let value = serde_json::to_value(HudCardDto::Place {
        id: "card-2".into(),
        name: "Kome Ramen".into(),
        photo: None,
        rating: None,
        distance: None,
        price_level: None,
        pick: false,
    })
    .unwrap();
    assert_eq!(
        value,
        json!({
            "type": "card.place",
            "id": "card-2",
            "name": "Kome Ramen",
            "pick": false,
        })
    );
    // A place card with no extractable image renders text-only (docs/12 §2.3):
    // the photo field is entirely absent, not null.
    assert!(value.get("photo").is_none());
}

// --- The source chip is structurally unavoidable (docs/12 §2.3, FR-25/ADR-014) ---

#[test]
fn sourced_image_serializes_with_all_required_attribution_fields() {
    let value = serde_json::to_value(photo()).unwrap();
    assert_eq!(
        value,
        json!({
            "url": "https://cdn.example/ramen.jpg",
            "sourceUrl": "https://en.wikipedia.org/wiki/Ramen",
            "sourceDomain": "wikipedia.org",
            "alt": "A bowl of ramen",
        })
    );
}

#[test]
fn sourced_image_fails_to_decode_without_source_domain() {
    // No optional escape hatch: an image payload missing attribution is a
    // decode error, never a card that quietly renders an unattributed photo.
    let missing_domain = json!({
        "url": "https://cdn.example/ramen.jpg",
        "sourceUrl": "https://en.wikipedia.org/wiki/Ramen",
        "alt": "A bowl of ramen",
    });
    assert!(serde_json::from_value::<SourcedImageDto>(missing_domain).is_err());
}

#[test]
fn sourced_image_fails_to_decode_without_alt_text() {
    let missing_alt = json!({
        "url": "https://cdn.example/ramen.jpg",
        "sourceUrl": "https://en.wikipedia.org/wiki/Ramen",
        "sourceDomain": "wikipedia.org",
    });
    assert!(serde_json::from_value::<SourcedImageDto>(missing_alt).is_err());
}

#[test]
fn place_card_with_a_photo_carries_the_photo_s_attribution_inline() {
    // There is no field on the place card for a bare image URL — the only way
    // to carry a photo at all is a full SourcedImageDto.
    let card = HudCardDto::Place {
        id: "card-2".into(),
        name: "Kome Ramen".into(),
        photo: Some(photo()),
        rating: None,
        distance: None,
        price_level: None,
        pick: false,
    };
    let value = serde_json::to_value(&card).unwrap();
    assert_eq!(value["photo"]["sourceDomain"], "wikipedia.org");
    assert_eq!(value["photo"]["alt"], "A bowl of ramen");
}

#[test]
fn now_playing_art_is_a_plain_url_not_a_sourced_image() {
    // Player-published art is the player's own content, not third-party web
    // content (docs/12 §2.3) — it carries no source chip, unlike every other
    // card's photo field.
    let card = HudCardDto::NowPlaying {
        id: "card-6".into(),
        title: Some("Dancing Queen".into()),
        artist: Some("ABBA".into()),
        album: None,
        art_url: Some("https://cdn.example/art.jpg".into()),
        source_app: "Spotify".into(),
    };
    let value = serde_json::to_value(&card).unwrap();
    assert_eq!(value["artUrl"], "https://cdn.example/art.jpg");
    assert!(value["artUrl"].is_string());
}

#[test]
fn approval_card_variant_carries_the_wire_reused_dto_verbatim() {
    let approval = ApprovalCardDto {
        approval_id: APPROVAL.parse().unwrap(),
        run_id: RUN.parse().unwrap(),
        tool_id: "message.send".into(),
        exact_effect: "message.send {to=\"bob@example.com\"}".into(),
        proposed_arguments: json!({ "to": "bob@example.com" }),
        risk: RiskLevelDto::R2,
        reversible: false,
        egress: DataEgressDto::External,
    };
    let card = HudCardDto::Approval {
        card: approval.clone(),
    };
    let value = serde_json::to_value(&card).unwrap();
    assert_eq!(value["type"], "card.approval");
    assert_eq!(value["card"]["approvalId"], APPROVAL);
    assert_eq!(
        value["card"]["exactEffect"],
        "message.send {to=\"bob@example.com\"}"
    );
}

#[test]
fn unrecognized_type_tag_fails_to_decode() {
    // The server side is strict (no catch-all) — a genuinely unknown card is a
    // decode error here. The client's defense for the same case (a payload
    // that never reaches this strict decode, or a newer contract version) is
    // the Angular `hud-card` switch degrading to the error card.
    let bogus = json!({ "type": "card.time_machine", "id": "x" });
    assert!(serde_json::from_value::<HudCardDto>(bogus).is_err());
}
