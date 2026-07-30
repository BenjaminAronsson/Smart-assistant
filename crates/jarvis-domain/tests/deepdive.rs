//! F3b.6 deep-dive routing signal + Research Notes rendering (FR-27, ADR-017,
//! docs/12 §2.5). The classifier decides whether the canvas is *extended* or
//! *shelved*, so its boundary cases are the feature's spec.

use jarvis_domain::deepdive::{
    ImageRef, QueryRelation, ResearchThread, SourceRef, classify_query, render_research_notes,
    should_offer_promotion,
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
