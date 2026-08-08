//! Deep-dive card projection (F3b.6, FR-27, ADR-017, docs/12 §2.3/§2.5).
//!
//! `jarvis-application` may not see a wire type (invariant #3), so the deep-dive
//! use case yields domain values and this module projects them into the two
//! registered card types the feature adds: the **sources** card ("show me the
//! references") and the **gallery** card. Same division of labour as
//! [`crate::timers::TimerEncoder`].
//!
//! The projection is where the attribution rules are actually enforced, because
//! it is the last place before the HUD:
//!
//! * every item's chip label is computed here from the *parsed* host
//!   ([`display_domain`]), never handed through from anything untrusted, and
//! * an item whose URL has no honest label is **dropped**, not shown with a
//!   partial or fabricated one. A reference the HUD cannot attribute is a
//!   reference it does not show.
//!
//! Note also what these functions cannot produce: neither card has a field for
//! page body text, so "open that" has nowhere to render a fetched page even by
//! accident (ADR-017 §3 — the browser worker opens the real page instead).

use jarvis_application::nowplaying::NowPlaying;
use jarvis_contracts::cards::{HudCardDto, SourceItemDto, SourcedImageDto};
use jarvis_contracts::deepdive::HudCanvasDto;
use jarvis_domain::deepdive::{ImageRef, ResearchThread, SourceRef, display_domain};

/// The now-playing card's id (F5.7). A **constant**, not a fresh handle per
/// answer: asking twice must update the one card on the canvas rather than
/// stack copies, the same upsert-by-id reasoning as the list card's
/// `list-{id}`. There is only ever one "what's playing" answer on screen.
pub const NOW_PLAYING_CARD_ID: &str = "now-playing";

/// Where a produced canvas instruction goes (F3b.6). Implemented by
/// [`crate::ws::WsHub`], which wraps it in the transient `hud.canvas` envelope;
/// a test substitutes a recorder.
///
/// Narrow on purpose: a producer can publish a canvas instruction and do
/// nothing else with the hub. Synchronous because a broadcast to a bounded
/// channel is — publishing must never be a place a request can block.
pub trait CanvasSink: Send + Sync {
    fn publish(&self, canvas: HudCanvasDto);
}

/// Project the pages a thread consulted into a sources card (docs/12 §2.3:
/// "title + domain + link each").
///
/// Returns `None` when nothing survives attribution — an empty references card
/// would promise a bibliography and show none.
pub fn sources_card(
    id: impl Into<String>,
    title: impl Into<String>,
    thread: &ResearchThread,
) -> Option<HudCardDto> {
    let items: Vec<SourceItemDto> = thread.sources().iter().filter_map(source_item).collect();
    (!items.is_empty()).then(|| HudCardDto::Sources {
        id: id.into(),
        title: title.into(),
        items,
    })
}

/// Project a thread's images into a gallery card, capped at
/// [`jarvis_domain::deepdive::GALLERY_IMAGE_CAP`] by
/// [`ResearchThread::gallery_images`] (ADR-017).
///
/// Each tile is a full [`SourcedImageDto`], so every image carries **its own**
/// source chip — the wire type offers no way to share one across tiles, which is
/// exactly what ADR-017 forbids. An image whose own page cannot be attributed is
/// dropped rather than borrowing a neighbour's badge.
pub fn gallery_card(
    id: impl Into<String>,
    title: impl Into<String>,
    thread: &ResearchThread,
) -> Option<HudCardDto> {
    let images: Vec<SourcedImageDto> = thread
        .gallery_images()
        .iter()
        .filter_map(gallery_image)
        .collect();
    (!images.is_empty()).then(|| HudCardDto::Gallery {
        id: id.into(),
        title: title.into(),
        images,
    })
}

/// Project the answer to "what's playing" onto its card (F5.7, FR-32/ADR-022,
/// docs/12 §2.3).
///
/// A straight projection with **no filling in**: a field the player did not
/// publish is `None` on [`NowPlaying`] and stays `None` on the wire, so the
/// renderer degrades to text-only rather than showing a stand-in album or a
/// placeholder image — the same refusal-to-invent as [`sources_card`]'s
/// untitled reference and [`gallery_card`]'s unattributable tile.
///
/// `art_url` is the player's own art, already restricted to `https` by
/// `jarvis_domain::media::TrackMetadata::sanitized`; it needs no source chip
/// because a player showing its own cover art is not third-party web content
/// (the media bar treats it identically).
pub fn now_playing_card(now_playing: &NowPlaying) -> HudCardDto {
    HudCardDto::NowPlaying {
        id: NOW_PLAYING_CARD_ID.to_owned(),
        title: now_playing.title.clone(),
        artist: now_playing.artist.clone(),
        album: now_playing.album.clone(),
        art_url: now_playing.art_url.clone(),
        source_app: now_playing.source_app.clone(),
    }
}

fn source_item(source: &SourceRef) -> Option<SourceItemDto> {
    let domain = display_domain(source.url())?;
    let title = source.title().trim();
    Some(SourceItemDto {
        // A reference with no title still needs something readable; the domain
        // is honest and is already computed.
        title: if title.is_empty() {
            domain.clone()
        } else {
            title.to_owned()
        },
        url: source.url().to_owned(),
        domain,
    })
}

fn gallery_image(image: &ImageRef) -> Option<SourcedImageDto> {
    // Both the image and the page it came from must be real web URLs: the tile
    // paints one and links the other.
    let source_domain = display_domain(image.source_url())?;
    display_domain(image.url())?;
    let alt = image.alt().trim();
    Some(SourcedImageDto {
        url: image.url().to_owned(),
        source_url: image.source_url().to_owned(),
        source_domain,
        // Alt text is required by the wire type (docs/12 §8). An image that
        // arrived without any gets a plain, honest stand-in rather than an empty
        // string, which a screen reader would announce as nothing at all.
        alt: if alt.is_empty() {
            "Image from the web".to_owned()
        } else {
            alt.to_owned()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::deepdive::GALLERY_IMAGE_CAP;

    fn thread() -> ResearchThread {
        let mut thread = ResearchThread::new("Ramen in Kreuzberg");
        thread
            .record_source("Ramen — Wikipedia", "https://en.wikipedia.org/wiki/Ramen")
            .unwrap();
        thread
            .record_source("Berlin Ramen Guide", "https://www.guide.example/ramen?p=2")
            .unwrap();
        thread
    }

    #[test]
    fn a_sources_card_lists_title_domain_and_link_for_each_page() {
        let card = sources_card("card-1", "References", &thread()).unwrap();
        let HudCardDto::Sources { items, .. } = &card else {
            panic!("expected a sources card");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].domain, "en.wikipedia.org");
        assert_eq!(items[0].title, "Ramen — Wikipedia");
        // `www.` is stripped and the query string is not part of the label.
        assert_eq!(items[1].domain, "guide.example");
        assert_eq!(items[1].url, "https://www.guide.example/ramen?p=2");
    }

    #[test]
    fn a_thread_with_no_attributable_sources_produces_no_card() {
        let empty = ResearchThread::new("Ramen");
        assert!(sources_card("card-1", "References", &empty).is_none());
    }

    #[test]
    fn an_untitled_reference_falls_back_to_its_domain_not_to_blank() {
        let mut thread = ResearchThread::new("Ramen");
        thread
            .record_source("   ", "https://example.org/x")
            .unwrap();
        let card = sources_card("card-1", "References", &thread).unwrap();
        let HudCardDto::Sources { items, .. } = &card else {
            panic!("expected a sources card");
        };
        assert_eq!(items[0].title, "example.org");
    }

    #[test]
    fn every_gallery_tile_gets_its_own_chip_from_its_own_page() {
        let mut thread = ResearchThread::new("Ramen");
        thread
            .record_image(
                "shoyu ramen",
                "https://cdn.a.example/1.jpg",
                "https://a.example/page",
            )
            .unwrap();
        thread
            .record_image(
                "miso ramen",
                "https://cdn.b.example/2.jpg",
                "https://b.example/other",
            )
            .unwrap();

        let card = gallery_card("card-2", "Pictures of ramen", &thread).unwrap();
        let HudCardDto::Gallery { images, .. } = &card else {
            panic!("expected a gallery card");
        };
        // Provenance differs per image, so the chips must differ too (ADR-017).
        assert_eq!(images[0].source_domain, "a.example");
        assert_eq!(images[1].source_domain, "b.example");
        assert_eq!(images[0].source_url, "https://a.example/page");
        assert_eq!(images[1].source_url, "https://b.example/other");
    }

    #[test]
    fn a_gallery_never_exceeds_the_adr_017_cap() {
        let mut thread = ResearchThread::new("Ramen");
        for i in 0..(GALLERY_IMAGE_CAP + 4) {
            thread
                .record_image(
                    format!("bowl {i}"),
                    format!("https://cdn.example/{i}.jpg"),
                    format!("https://example.org/p/{i}"),
                )
                .unwrap();
        }
        let card = gallery_card("card-2", "Pictures", &thread).unwrap();
        let HudCardDto::Gallery { images, .. } = &card else {
            panic!("expected a gallery card");
        };
        assert_eq!(images.len(), GALLERY_IMAGE_CAP);
    }

    #[test]
    fn an_image_with_no_alt_text_still_gets_something_a_screen_reader_can_read() {
        let mut thread = ResearchThread::new("Ramen");
        thread
            .record_image("", "https://cdn.example/1.jpg", "https://example.org/p")
            .unwrap();
        let card = gallery_card("card-2", "Pictures", &thread).unwrap();
        let HudCardDto::Gallery { images, .. } = &card else {
            panic!("expected a gallery card");
        };
        assert!(!images[0].alt.is_empty());
    }

    #[test]
    fn a_spoofing_source_url_is_labelled_by_its_real_host() {
        // The chip must never let `evil.example` present itself as
        // `wikipedia.org` (the userinfo trick).
        let mut thread = ResearchThread::new("Ramen");
        thread
            .record_source("Totally Wikipedia", "https://wikipedia.org@evil.example/x")
            .unwrap();
        let card = sources_card("card-1", "References", &thread).unwrap();
        let HudCardDto::Sources { items, .. } = &card else {
            panic!("expected a sources card");
        };
        assert_eq!(items[0].domain, "evil.example");
    }

    // ---- F5.7: the now-playing card ------------------------------------

    fn now_playing(album: Option<&str>, art: Option<&str>) -> NowPlaying {
        NowPlaying {
            title: Some("Dancing Queen".to_owned()),
            artist: Some("ABBA".to_owned()),
            album: album.map(str::to_owned),
            art_url: art.map(str::to_owned),
            source_app: "Spotify".to_owned(),
        }
    }

    #[test]
    fn a_now_playing_card_carries_title_artist_album_art_and_the_source_app() {
        let card = now_playing_card(&now_playing(
            Some("Arrival"),
            Some("https://cdn.example/art.jpg"),
        ));
        let HudCardDto::NowPlaying {
            id,
            title,
            artist,
            album,
            art_url,
            source_app,
        } = &card
        else {
            panic!("expected a now-playing card");
        };
        assert_eq!(id, NOW_PLAYING_CARD_ID);
        assert_eq!(title.as_deref(), Some("Dancing Queen"));
        assert_eq!(artist.as_deref(), Some("ABBA"));
        assert_eq!(album.as_deref(), Some("Arrival"));
        assert_eq!(art_url.as_deref(), Some("https://cdn.example/art.jpg"));
        assert_eq!(source_app, "Spotify");
    }

    /// A field the player did not publish is simply absent on the wire — the
    /// renderer degrades to text-only rather than showing a fabricated album
    /// or a placeholder image.
    #[test]
    fn a_missing_album_or_art_is_omitted_rather_than_invented() {
        let value = serde_json::to_value(now_playing_card(&now_playing(None, None))).unwrap();
        let object = value.as_object().unwrap();
        assert!(!object.contains_key("album"), "{value}");
        assert!(!object.contains_key("artUrl"), "{value}");
        assert_eq!(object["sourceApp"], "Spotify");
    }

    /// The id is a constant so re-asking updates the one card on the canvas
    /// instead of stacking copies (upsert-by-id, like the list card).
    #[test]
    fn asking_twice_addresses_the_same_card() {
        let first = now_playing_card(&now_playing(Some("Arrival"), None));
        let second = now_playing_card(&NowPlaying {
            title: Some("Take a Chance on Me".to_owned()),
            ..now_playing(None, None)
        });
        let (HudCardDto::NowPlaying { id: a, .. }, HudCardDto::NowPlaying { id: b, .. }) =
            (&first, &second)
        else {
            panic!("expected now-playing cards");
        };
        assert_eq!(a, b);
    }

    /// The answer card is **data only** (ADR-022): no transport affordance and
    /// nowhere to put one, so a query answer can never grow into a control
    /// surface that bypasses `policy::evaluate`.
    #[test]
    fn the_now_playing_card_has_nowhere_to_put_a_control() {
        let value = serde_json::to_value(now_playing_card(&now_playing(
            Some("Arrival"),
            Some("https://cdn.example/art.jpg"),
        )))
        .unwrap();
        let object = value.as_object().unwrap();
        for forbidden in ["controls", "actions", "player", "commands", "canPause"] {
            assert!(!object.contains_key(forbidden), "carries `{forbidden}`");
        }
    }

    #[test]
    fn neither_card_has_anywhere_to_put_page_body_text() {
        // ADR-017 §3: reading a source is a browser handoff, so the HUD never
        // re-renders a fetched page. Asserted on the serialized card because
        // that is what actually reaches the client.
        let mut thread = thread();
        thread
            .record_image(
                "shoyu ramen",
                "https://cdn.a.example/1.jpg",
                "https://a.example/page",
            )
            .unwrap();
        for card in [
            sources_card("card-1", "References", &thread).unwrap(),
            gallery_card("card-2", "Pictures", &thread).unwrap(),
        ] {
            let value = serde_json::to_value(&card).unwrap();
            let object = value.as_object().unwrap();
            for forbidden in ["body", "content", "html", "text", "pageText", "excerpt"] {
                assert!(
                    !object.contains_key(forbidden),
                    "{} must not carry `{forbidden}`",
                    card.card_type()
                );
            }
        }
    }
}
