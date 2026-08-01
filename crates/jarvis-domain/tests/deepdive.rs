//! F3b.6 deep-dive routing signal + Research Notes rendering (FR-27, ADR-017,
//! docs/12 §2.5). The classifier decides whether the canvas is *extended* or
//! *shelved*, so its boundary cases are the feature's spec.

use jarvis_domain::deepdive::{
    GALLERY_IMAGE_CAP, ImageRef, MAX_PARAPHRASE_CHARS, QueryRelation, ResearchThread, SourceRef,
    ThreadError, classify_query, display_domain, is_source_handoff, is_web_url,
    render_research_notes, select_source, should_offer_promotion,
};

const TOPIC: &str = "ramen places near Kreuzberg";

#[test]
fn explicit_follow_ups_extend_the_thread() {
    // docs/12 §2.5 names these exact shapes as continuations.
    for query in [
        "tell me more",
        "Tell me more about the second one",
        "what about vegetarian options",
        "compare that to the first",
        "what else",
        "show me the references",
        "open that",
    ] {
        assert_eq!(
            classify_query(TOPIC, query),
            QueryRelation::Continuation,
            "{query:?} should extend the live thread"
        );
    }
}

#[test]
fn a_genuine_topic_change_shelves() {
    for query in [
        "what's the weather tomorrow",
        "turn on the living room lamps",
        "who won the election",
        "remind me to call the landlord at six",
    ] {
        assert_eq!(
            classify_query(TOPIC, query),
            QueryRelation::NewTopic,
            "{query:?} should shelve the canvas"
        );
    }
}

#[test]
fn sharing_the_topics_own_words_continues_it() {
    assert_eq!(
        classify_query(TOPIC, "which ramen place is open latest"),
        QueryRelation::Continuation
    );
}

#[test]
fn a_bare_back_reference_continues_the_thread() {
    // "why is it closed" introduces no new subject — it can only be about what
    // is already on the canvas.
    assert_eq!(
        classify_query(TOPIC, "why is it closed"),
        QueryRelation::Continuation
    );
    assert_eq!(
        classify_query(TOPIC, "is that far"),
        QueryRelation::Continuation
    );
}

#[test]
fn an_explicit_reset_always_wins() {
    // Even when it looks like a follow-up, "new topic" is an instruction.
    assert_eq!(
        classify_query(TOPIC, "new topic — tell me more about berlin transit"),
        QueryRelation::NewTopic
    );
    assert_eq!(classify_query(TOPIC, "never mind"), QueryRelation::NewTopic);
}

#[test]
fn with_no_live_thread_everything_is_a_new_topic() {
    assert_eq!(classify_query("", "tell me more"), QueryRelation::NewTopic);
    assert_eq!(classify_query(TOPIC, ""), QueryRelation::NewTopic);
}

#[test]
fn promotion_is_offered_past_the_threshold_and_not_twice_for_the_same_turn() {
    assert!(!should_offer_promotion(2, 3, None));
    assert!(should_offer_promotion(3, 3, None));
    // Already offered at this count: do not nag again on the same turn.
    assert!(!should_offer_promotion(3, 3, Some(3)));
    // The next follow-up may offer again.
    assert!(should_offer_promotion(4, 3, Some(3)));
    // A zero threshold disables the offer rather than offering constantly.
    assert!(!should_offer_promotion(9, 0, None));
}

fn thread() -> ResearchThread {
    ResearchThread {
        topic: "Ramen in Kreuzberg".to_owned(),
        facts: vec!["Kome opens at 12:00 and is rated 4.7".to_owned()],
        sources: vec![SourceRef {
            title: "Kome Ramen".to_owned(),
            url: "https://example.org/kome".to_owned(),
        }],
        images: vec![ImageRef {
            alt: "bowl of ramen".to_owned(),
            url: "https://cdn.example.org/ramen.jpg".to_owned(),
            source_url: "https://example.org/kome".to_owned(),
        }],
    }
}

#[test]
fn research_notes_carry_facts_sources_and_per_image_attribution() {
    let md = render_research_notes(&thread());
    assert!(md.starts_with("# Research Notes: Ramen in Kreuzberg"));
    assert!(md.contains("- Kome opens at 12:00 and is rated 4.7"));
    assert!(md.contains("[Kome Ramen](https://example.org/kome)"));
    // Each image states its own source — one shared attribution is not
    // acceptable when provenance differs (ADR-017).
    assert!(md.contains("source: https://example.org/kome"));
}

#[test]
fn untrusted_thread_text_cannot_become_markup_in_the_document() {
    let hostile = ResearchThread {
        topic: "# pwned".to_owned(),
        facts: vec![
            "<script>alert(1)</script>".to_owned(),
            "[click me](javascript:alert(1))".to_owned(),
            "line\u{202e}reversed\u{7}bell".to_owned(),
        ],
        sources: vec![SourceRef {
            title: "](https://evil.example/) [pwn".to_owned(),
            url: "javascript:alert(1)".to_owned(),
        }],
        images: vec![ImageRef {
            alt: "![nested](x)".to_owned(),
            url: "data:text/html,<script>1</script>".to_owned(),
            source_url: "https://example.org/page".to_owned(),
        }],
    };
    let md = render_research_notes(&hostile);

    // Markup-opening characters are escaped, so a fact that looks like a link
    // stays text. (The escaped form still *contains* the original bytes — what
    // matters is that markdown will not parse them as a link.)
    assert!(!md.contains("<script>"));
    assert!(
        md.contains("\\[click me\\]"),
        "link syntax in a fact must be escaped: {md}"
    );
    assert!(md.contains("\\<script\\>"));
    // A non-http(s) "source" is never emitted as a link target: the URLs this
    // module writes itself are the ones that must never carry a scheme like
    // `javascript:` or `data:`.
    // Precisely: every link target this module *emits* is http(s). A hostile
    // URL may survive as inert escaped text (the record stays honest), but it
    // never becomes something a reader can click.
    for (idx, _) in md.match_indices("](") {
        let target = &md[idx + 2..];
        // An escaped bracket (`\](`) is text, not an emitted link.
        let emitted = !md[..idx].ends_with('\\');
        if emitted {
            assert!(
                target.starts_with("http://") || target.starts_with("https://"),
                "emitted link target must be http(s), got: {}",
                &target[..target.len().min(40)]
            );
        }
    }
    // Control and bidi characters do not survive into the durable record.
    assert!(!md.contains('\u{202e}'));
    assert!(!md.contains('\u{7}'));
    // The honest bits are still there: the image keeps its real source.
    assert!(md.contains("https://example.org/page"));
}

#[test]
fn an_empty_thread_still_renders_an_honest_document() {
    let md = render_research_notes(&ResearchThread::default());
    assert!(md.contains("_No findings recorded._"));
    assert!(md.contains("_No sources consulted._"));
    // No Images section at all rather than an empty promise.
    assert!(!md.contains("## Images"));
}

// --- Source handoff: "open that / read it" (ADR-017 §3) --------------------

#[test]
fn reading_the_source_is_recognised_as_a_handoff() {
    for query in [
        "open that",
        "open it",
        "read it",
        "let me read that",
        "open the second one",
        "show me the source",
    ] {
        assert!(is_source_handoff(query), "{query:?} asks for the real page");
    }
}

#[test]
fn asking_for_more_summary_is_not_a_handoff() {
    // A continuation is not a handoff — these still get cards, not a browser.
    for query in [
        "tell me more",
        "what about vegetarian options",
        "show me the references",
        "compare that to the first",
    ] {
        assert!(!is_source_handoff(query), "{query:?} is not a handoff");
    }
}

#[test]
fn an_ordinal_picks_that_source_and_out_of_range_picks_nothing() {
    assert_eq!(select_source("open the second one", 3), Some(1));
    assert_eq!(select_source("read the third", 3), Some(2));
    // Out of range opens nothing rather than a different page than was asked for.
    assert_eq!(select_source("open the fifth one", 3), None);
    // No ordinal: the source just cited.
    assert_eq!(select_source("open that", 3), Some(0));
    // Nothing consulted yet: nothing to open.
    assert_eq!(select_source("open that", 0), None);
}

// --- Attribution labels cannot be spoofed (ADR-014/ADR-017) ---------------

#[test]
fn the_display_domain_comes_from_the_parsed_host() {
    assert_eq!(
        display_domain("https://en.wikipedia.org/wiki/Ramen").as_deref(),
        Some("en.wikipedia.org")
    );
    assert_eq!(
        display_domain("https://WWW.Example.ORG:8443/a?b#c").as_deref(),
        Some("example.org")
    );
}

#[test]
fn userinfo_cannot_dress_a_hostile_host_up_as_a_trusted_one() {
    // The classic chip spoof: everything before '@' is userinfo, not the host.
    assert_eq!(
        display_domain("https://wikipedia.org@evil.example/x").as_deref(),
        Some("evil.example")
    );
    // Taking the FIRST '@' would re-open the same hole.
    assert_eq!(
        display_domain("https://a@wikipedia.org@evil.example/x").as_deref(),
        Some("evil.example")
    );
}

#[test]
fn unlabellable_urls_yield_no_domain_at_all() {
    for url in [
        "javascript:alert(1)",
        "data:text/html,<script>1</script>",
        "file:///etc/passwd",
        "https://",
        "https:///path-only",
        // A raw Unicode host is the homograph attack; punycode is the honest form.
        "https://wikipediа.org/x",
        "https://exa mple.org/x",
    ] {
        assert_eq!(display_domain(url), None, "{url:?} must not get a chip");
    }
    // Punycode is ASCII and passes through visibly encoded.
    assert_eq!(
        display_domain("https://xn--80ak6aa92e.com/x").as_deref(),
        Some("xn--80ak6aa92e.com")
    );
}

#[test]
fn only_http_urls_are_web_urls() {
    assert!(is_web_url("http://example.org"));
    assert!(is_web_url("HTTPS://Example.org"));
    assert!(!is_web_url("javascript:alert(1)"));
    assert!(!is_web_url("//example.org"));
}

// --- The thread refuses to file a scrape as a paraphrase (ADR-017) --------

#[test]
fn a_fact_longer_than_a_paraphrase_is_rejected_not_truncated() {
    let mut thread = ResearchThread::new("Ramen");
    let scrape = "a".repeat(MAX_PARAPHRASE_CHARS + 1);
    assert_eq!(
        thread.record_fact(scrape),
        Err(ThreadError::NotAParaphrase),
        "page text must not be storable as a fact"
    );
    // A truncated scrape is still a scrape: nothing was kept.
    assert!(thread.facts.is_empty());
    assert_eq!(thread.record_fact("   "), Err(ThreadError::Empty));
    assert!(thread.record_fact("Kome opens at noon.").is_ok());
    // A thread does not repeat itself.
    assert!(thread.record_fact("Kome opens at noon.").is_ok());
    assert_eq!(thread.facts.len(), 1);
}

#[test]
fn a_source_without_an_honest_attribution_is_refused() {
    let mut thread = ResearchThread::new("Ramen");
    assert!(matches!(
        thread.record_source("Evil", "javascript:alert(1)"),
        Err(ThreadError::Unattributable(_))
    ));
    assert!(thread.sources.is_empty());
    assert!(
        thread
            .record_source("Kome", "https://example.org/kome")
            .is_ok()
    );
    // Cited once, however often it is consulted.
    assert!(
        thread
            .record_source("Kome", "https://example.org/kome")
            .is_ok()
    );
    assert_eq!(thread.sources.len(), 1);
}

#[test]
fn an_image_cannot_be_recorded_without_its_own_source() {
    let mut thread = ResearchThread::new("Ramen");
    // Image fine, provenance not: refused, because a tile with no honest badge
    // would have to borrow another image's (ADR-017 forbids exactly that).
    assert!(matches!(
        thread.record_image("bowl", "https://cdn.example.org/a.jpg", ""),
        Err(ThreadError::Unattributable(_))
    ));
    assert!(matches!(
        thread.record_image("bowl", "data:image/png;base64,AA", "https://example.org/p"),
        Err(ThreadError::Unattributable(_))
    ));
    assert!(thread.images.is_empty());
}

#[test]
fn the_gallery_is_capped_at_the_adr_017_limit() {
    let mut thread = ResearchThread::new("Ramen");
    for i in 0..(GALLERY_IMAGE_CAP + 5) {
        thread
            .record_image(
                format!("bowl {i}"),
                format!("https://cdn.example.org/{i}.jpg"),
                format!("https://example.org/page/{i}"),
            )
            .unwrap();
    }
    assert_eq!(thread.gallery_images().len(), GALLERY_IMAGE_CAP);
    // Every image the gallery shows keeps its OWN page, not a shared one.
    let sources: Vec<&str> = thread
        .gallery_images()
        .iter()
        .map(|i| i.source_url.as_str())
        .collect();
    let mut deduped = sources.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(deduped.len(), sources.len());
    // The full record (what the artifact gets) is not truncated by the cap.
    assert_eq!(thread.images.len(), GALLERY_IMAGE_CAP + 5);
}

#[test]
fn the_promoted_document_is_built_only_from_recorded_paraphrases() {
    // The document generator has no other input: what survives `record_*` is
    // exactly what the artifact can contain.
    let mut thread = ResearchThread::new("Ramen in Kreuzberg");
    thread
        .record_fact("Kome is rated 4.7 and opens at noon.")
        .unwrap();
    thread
        .record_source("Kome Ramen", "https://example.org/kome")
        .unwrap();
    thread
        .record_image(
            "bowl of ramen",
            "https://cdn.example.org/ramen.jpg",
            "https://example.org/kome",
        )
        .unwrap();
    let md = render_research_notes(&thread);
    assert!(md.contains("- Kome is rated 4.7 and opens at noon."));
    assert!(md.contains("[Kome Ramen](https://example.org/kome)"));
    assert!(md.contains("source: https://example.org/kome"));
}
