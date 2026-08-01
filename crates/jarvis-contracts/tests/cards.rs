//! F3b.2: the HUD card grammar wire shapes (docs/12 §2.3, FR-09, FR-25/ADR-014).
//!
//! Two properties matter more than the rest: every card round-trips with its
//! dotted `type` tag, and **no card can carry a web image without its
//! attribution** — `SourcedImageDto`'s `sourceUrl`/`sourceDomain`/`alt` fields
//! are required, so a JSON payload that omits any of them fails to decode
//! rather than rendering an unattributed photo.

use jarvis_contracts::approvals::{ApprovalCardDto, DataEgressDto, RiskLevelDto};
use jarvis_contracts::cards::{
    HeadlineItemDto, HudCardDto, MapPointDto, MediaGridItemDto, MiniStatDto, SourceItemDto,
    SourcedImageDto,
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
        HudCardDto::Map {
            id: "card-11".into(),
            label: "Ramen Nagi".into(),
            destination: MapPointDto {
                lon: -122.4194,
                lat: 37.7749,
            },
            destination_label: Some("Ramen Nagi".into()),
            current_location: Some(MapPointDto {
                lon: -122.42,
                lat: 37.77,
            }),
            route: vec![
                MapPointDto {
                    lon: -122.42,
                    lat: 37.77,
                },
                MapPointDto {
                    lon: -122.4194,
                    lat: 37.7749,
                },
            ],
            distance: Some("1.2 mi".into()),
            walk_time: Some("24 min".into()),
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
        HudCardDto::Sources {
            id: "card-9".into(),
            title: "References".into(),
            items: vec![SourceItemDto {
                title: "Ramen — Wikipedia".into(),
                url: "https://en.wikipedia.org/wiki/Ramen".into(),
                domain: "en.wikipedia.org".into(),
            }],
        },
        HudCardDto::Gallery {
            id: "card-10".into(),
            title: "Pictures of ramen".into(),
            images: vec![photo()],
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
            "card.gallery",
            "card.headlines",
            "card.map",
            "card.media_grid",
            "card.now_playing",
            "card.place",
            "card.sources",
            "card.status",
            "card.value_readout",
        ]
    );
}

// --- Deep-dive cards (F3b.6, FR-27/ADR-017) ------------------------------

#[test]
fn sources_card_carries_a_title_domain_and_link_for_every_page() {
    // docs/12 §2.3: "a compact list of pages consulted — title + domain + link
    // each". `domain` is computed server-side so the client never derives
    // trusted-looking text from an untrusted URL.
    let value = serde_json::to_value(HudCardDto::Sources {
        id: "card-9".into(),
        title: "References".into(),
        items: vec![SourceItemDto {
            title: "Ramen — Wikipedia".into(),
            url: "https://en.wikipedia.org/wiki/Ramen".into(),
            domain: "en.wikipedia.org".into(),
        }],
    })
    .unwrap();
    assert_eq!(
        value,
        json!({
            "type": "card.sources",
            "id": "card-9",
            "title": "References",
            "items": [{
                "title": "Ramen — Wikipedia",
                "url": "https://en.wikipedia.org/wiki/Ramen",
                "domain": "en.wikipedia.org",
            }],
        })
    );
}

#[test]
fn a_source_item_cannot_omit_its_domain_or_url() {
    // Same stance as `SourcedImageDto`: attribution is required, so a payload
    // missing it is a decode error rather than an unlabelled reference.
    for missing in [
        json!({ "title": "Ramen", "url": "https://en.wikipedia.org/wiki/Ramen" }),
        json!({ "title": "Ramen", "domain": "en.wikipedia.org" }),
    ] {
        assert!(serde_json::from_value::<SourceItemDto>(missing).is_err());
    }
}

#[test]
fn every_gallery_tile_carries_its_own_attribution() {
    // ADR-017: images may come from different pages, so "one shared source link
    // is not acceptable". The wire type makes sharing impossible — the gallery
    // holds full `SourcedImageDto`s and has no card-level source field at all.
    let a = SourcedImageDto {
        url: "https://cdn.example/one.jpg".into(),
        source_url: "https://a.example/page".into(),
        source_domain: "a.example".into(),
        alt: "A bowl of shoyu ramen".into(),
    };
    let b = SourcedImageDto {
        url: "https://cdn.example/two.jpg".into(),
        source_url: "https://b.example/other".into(),
        source_domain: "b.example".into(),
        alt: "A bowl of miso ramen".into(),
    };
    let value = serde_json::to_value(HudCardDto::Gallery {
        id: "card-10".into(),
        title: "Pictures of ramen".into(),
        images: vec![a, b],
    })
    .unwrap();
    assert_eq!(value["images"][0]["sourceDomain"], "a.example");
    assert_eq!(value["images"][1]["sourceDomain"], "b.example");
    // No card-level attribution field exists to fall back on.
    assert!(value.get("sourceUrl").is_none());
    assert!(value.get("sourceDomain").is_none());
}

#[test]
fn a_gallery_image_missing_its_alt_text_fails_the_whole_card() {
    let bad = json!({
        "type": "card.gallery",
        "id": "card-10",
        "title": "Pictures",
        "images": [{
            "url": "https://cdn.example/one.jpg",
            "sourceUrl": "https://a.example/page",
            "sourceDomain": "a.example",
        }],
    });
    assert!(serde_json::from_value::<HudCardDto>(bad).is_err());
}

#[test]
fn no_card_variant_carries_page_body_text() {
    // ADR-017 §3 / docs/12 §2.5: reading a source is a browser handoff — the
    // HUD never re-renders full page content. This asserts the *wire* has no
    // field to put it in: no variant has a body/content/html/text field, so
    // there is nothing for a producer to fill with a fetched page.
    for card in every_card() {
        let value = serde_json::to_value(&card).unwrap();
        let object = value.as_object().expect("cards serialize as objects");
        for forbidden in ["body", "content", "html", "text", "pageText", "fullText"] {
            assert!(
                !object.contains_key(forbidden),
                "{} must not carry a `{forbidden}` field",
                card.card_type()
            );
        }
    }
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
fn map_card_omits_absent_optional_fields_and_empty_route() {
    // A bare "where is X" query with no navigation intent: no current
    // location, no route, no routing facts — text-only besides the pin.
    let value = serde_json::to_value(HudCardDto::Map {
        id: "card-9".into(),
        label: "Angkor Wat".into(),
        destination: MapPointDto {
            lon: 103.866667,
            lat: 13.412500,
        },
        destination_label: None,
        current_location: None,
        route: vec![],
        distance: None,
        walk_time: None,
    })
    .unwrap();
    assert_eq!(
        value,
        json!({
            "type": "card.map",
            "id": "card-9",
            "label": "Angkor Wat",
            "destination": { "lon": 103.866667, "lat": 13.4125 },
        })
    );
    for absent in [
        "destinationLabel",
        "currentLocation",
        "route",
        "distance",
        "walkTime",
    ] {
        assert!(
            value.get(absent).is_none(),
            "expected {absent} to be absent"
        );
    }
}

#[test]
fn map_card_carries_route_and_current_location_camel_case() {
    let card = HudCardDto::Map {
        id: "card-9".into(),
        label: "Ramen Nagi".into(),
        destination: MapPointDto {
            lon: -122.4194,
            lat: 37.7749,
        },
        destination_label: Some("Ramen Nagi".into()),
        current_location: Some(MapPointDto {
            lon: -122.42,
            lat: 37.77,
        }),
        route: vec![MapPointDto {
            lon: -122.42,
            lat: 37.77,
        }],
        distance: Some("1.2 mi".into()),
        walk_time: Some("24 min".into()),
    };
    let value = serde_json::to_value(&card).unwrap();
    assert_eq!(value["type"], "card.map");
    assert_eq!(value["destinationLabel"], "Ramen Nagi");
    assert_eq!(value["currentLocation"]["lat"], 37.77);
    assert_eq!(value["route"][0]["lon"], -122.42);
    assert_eq!(value["distance"], "1.2 mi");
    assert_eq!(value["walkTime"], "24 min");
    let back: HudCardDto = serde_json::from_value(value).unwrap();
    assert_eq!(back, card);
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
