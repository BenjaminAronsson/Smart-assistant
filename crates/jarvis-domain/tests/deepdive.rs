//! F3b.6 deep-dive routing signal + Research Notes rendering (FR-27, ADR-017,
//! docs/12 §2.5). The classifier decides whether the canvas is *extended* or
//! *shelved*, so its boundary cases are the feature's spec.

use jarvis_domain::deepdive::{
    GALLERY_IMAGE_CAP, MAX_IMAGE_ALT_CHARS, MAX_PARAPHRASE_CHARS, MAX_SOURCE_TITLE_CHARS,
    MAX_THREAD_FACTS, MAX_THREAD_IMAGES, MAX_THREAD_SOURCES, MAX_TOPIC_CHARS, MAX_URL_CHARS,
    QueryRelation, ResearchThread, ThreadError, classify_query, display_domain, is_source_handoff,
    is_web_url, render_research_notes, select_source, should_offer_promotion,
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

/// A thread can only be built by *recording* into it — the fields are private,
/// so there is no struct literal that could put page text or an unattributable
/// URL where a checked paraphrase belongs (ADR-017).
fn thread() -> ResearchThread {
    let mut thread = ResearchThread::new("Ramen in Kreuzberg");
    thread
        .record_fact("Kome opens at 12:00 and is rated 4.7")
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
    thread
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
    let mut hostile = ResearchThread::new("# pwned");
    for fact in [
        "<script>alert(1)</script>",
        "[click me](javascript:alert(1))",
        "line\u{202e}reversed\u{7}bell",
    ] {
        hostile.record_fact(fact).unwrap();
    }
    // Hostile *text* is recorded and neutralised at render time; a hostile
    // *URL* never gets in at all (see the recorder tests below), which is why
    // these two carry real pages and only their labels are hostile.
    hostile
        .record_source("](https://evil.example/) [pwn", "https://example.org/page")
        .unwrap();
    hostile
        .record_image(
            "![nested](x)",
            "https://cdn.example.org/a.jpg",
            "https://example.org/page",
        )
        .unwrap();
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
    // Every heading in the document is one this module wrote: the four we emit
    // and nothing an untrusted string introduced.
    let headings: Vec<&str> = md.lines().filter(|l| l.starts_with('#')).collect();
    assert_eq!(
        headings,
        [
            "# Research Notes: \\# pwned",
            "## What I found",
            "## Sources",
            "## Images"
        ],
        "the document's structure is ours: {md}"
    );
}

#[test]
fn a_url_that_could_inject_document_structure_is_refused_by_the_recorders() {
    // The escaper protects the link *text*; this protects the link
    // *destination*. A newline inside a fetched page's URL used to survive into
    // `[label](…)`, where CommonMark refuses the destination and the injected
    // tail becomes a heading — or a real, clickable anchor pointing anywhere
    // the page likes. `display_domain` cut the authority at the first `/` and
    // attributed such a URL happily, so nothing downstream caught it either.
    let mut thread = ResearchThread::new("Ramen");
    for hostile in [
        "https://a.example/x\n# Owned heading\n",
        "https://a.example/\n[Reset your password](https://evil.example)",
        "https://a.example/x\r\n> quote",
        "https://a.example/x\ttab",
        "https://a.example/spaced out",
        "https://a.example/x\\y",
        "https://a.example/\u{202e}gpj.exe",
    ] {
        assert!(
            !is_web_url(hostile),
            "{hostile:?} is not a URL this system will emit or navigate to"
        );
        assert_eq!(
            display_domain(hostile),
            None,
            "{hostile:?} must not get an attribution chip either"
        );
        assert!(
            matches!(
                thread.record_source("Fine title", hostile),
                Err(ThreadError::Unattributable)
            ),
            "{hostile:?} must not be recordable as a source"
        );
        assert!(matches!(
            thread.record_image("alt", hostile, "https://example.org/page"),
            Err(ThreadError::Unattributable)
        ));
        assert!(matches!(
            thread.record_image("alt", "https://cdn.example.org/a.jpg", hostile),
            Err(ThreadError::Unattributable)
        ));
    }
    assert!(thread.sources().is_empty());
    assert!(thread.images().is_empty());
    // Nothing reached the document, so nothing had to be caught at render time.
    assert!(!render_research_notes(&thread).contains("a.example"));
}

#[test]
fn a_thread_stops_accumulating_before_it_becomes_a_scrape() {
    // The per-fact paraphrase cap bounds one entry; without a bound on the
    // *number* of entries, a page body filed in 400-character chunks is still a
    // page body (docs/06 §5, artifact size limits).
    let mut thread = ResearchThread::new("Ramen");
    for i in 0..MAX_THREAD_FACTS {
        thread.record_fact(format!("finding number {i}")).unwrap();
    }
    assert_eq!(
        thread.record_fact("one chunk too many"),
        Err(ThreadError::FactsFull)
    );
    assert_eq!(thread.facts().len(), MAX_THREAD_FACTS);

    let mut thread = ResearchThread::new("Ramen");
    for i in 0..MAX_THREAD_SOURCES {
        thread
            .record_source("page", format!("https://example.org/{i}"))
            .unwrap();
    }
    assert_eq!(
        thread.record_source("page", "https://example.org/one-more"),
        Err(ThreadError::SourcesFull)
    );
    assert_eq!(thread.sources().len(), MAX_THREAD_SOURCES);

    let mut thread = ResearchThread::new("Ramen");
    for i in 0..MAX_THREAD_IMAGES {
        thread
            .record_image(
                "bowl",
                format!("https://cdn.example.org/{i}.jpg"),
                "https://example.org/page",
            )
            .unwrap();
    }
    assert_eq!(
        thread.record_image(
            "bowl",
            "https://cdn.example.org/one-more.jpg",
            "https://example.org/page"
        ),
        Err(ThreadError::ImagesFull)
    );
    assert_eq!(thread.images().len(), MAX_THREAD_IMAGES);
}

#[test]
fn untrusted_labels_are_capped_to_a_label_length() {
    // A title and an alt text are display labels from a fetched page — bounded
    // like every other piece of untrusted display text (cf. `MAX_ITEM_TEXT_BYTES`).
    let mut thread = ResearchThread::new("t".repeat(MAX_TOPIC_CHARS * 3));
    assert_eq!(thread.topic().chars().count(), MAX_TOPIC_CHARS);
    thread
        .record_source(
            "s".repeat(MAX_SOURCE_TITLE_CHARS * 3),
            "https://example.org/a",
        )
        .unwrap();
    assert_eq!(
        thread.sources()[0].title().chars().count(),
        MAX_SOURCE_TITLE_CHARS
    );
    thread
        .record_image(
            "a".repeat(MAX_IMAGE_ALT_CHARS * 3),
            "https://cdn.example.org/a.jpg",
            "https://example.org/a",
        )
        .unwrap();
    assert_eq!(
        thread.images()[0].alt().chars().count(),
        MAX_IMAGE_ALT_CHARS
    );
}

#[test]
fn a_url_longer_than_the_ceiling_is_refused_rather_than_truncated() {
    // S1. The title and the alt text were bounded and the fact was bounded, but
    // the URL — the one field with no natural length — was stored verbatim at
    // any size. Reachable from an authenticated request body, held per thread,
    // and cloned into every published canvas envelope: a denial-of-resources
    // primitive (docs/06 §5), not a cosmetic gap.
    let mut thread = ResearchThread::new("Ramen");
    let long = format!("https://example.org/{}", "a".repeat(MAX_URL_CHARS));
    assert!(long.len() > MAX_URL_CHARS);

    // Refused, not shortened: a truncated URL is a *different* URL — it would
    // still parse, still earn a chip, and point somewhere nobody chose.
    assert_eq!(
        thread.record_source("Fine title", &long),
        Err(ThreadError::UrlTooLong)
    );
    assert_eq!(
        thread.record_image("alt", &long, "https://example.org/page"),
        Err(ThreadError::UrlTooLong)
    );
    // The provenance URL costs the thread just as much as the image URL does.
    assert_eq!(
        thread.record_image("alt", "https://cdn.example.org/a.jpg", &long),
        Err(ThreadError::UrlTooLong)
    );
    assert!(thread.sources().is_empty());
    assert!(thread.images().is_empty());

    // And the bound belongs to the URL rule itself, so nothing downstream of a
    // recorder can be handed one either.
    assert!(!is_web_url(&long));
    assert_eq!(display_domain(&long), None);

    // Exactly at the ceiling is still a URL — the bound is generous on purpose.
    let at_ceiling = format!(
        "https://example.org/{}",
        "a".repeat(MAX_URL_CHARS - "https://example.org/".len())
    );
    assert_eq!(at_ceiling.len(), MAX_URL_CHARS);
    assert!(thread.record_source("Fine title", &at_ceiling).is_ok());
    assert_eq!(thread.sources().len(), 1);
}

#[test]
fn a_refusal_never_echoes_the_callers_own_input_back_at_it() {
    // S4. `Unattributable` used to carry the offending URL, and these strings
    // reach a problem body and a log line — so an arbitrary-length, unsanitized,
    // attacker-chosen URL was being reflected straight back out. Same convention
    // as `ListError`: "small and content-free".
    let mut thread = ResearchThread::new("Ramen");
    let hostile = "javascript:alert('marker-9f3a')";
    let long = format!("https://example.org/{}", "marker9f3a".repeat(500));

    for error in [
        thread.record_source("t", hostile).unwrap_err(),
        thread.record_source("t", &long).unwrap_err(),
        thread
            .record_image("a", hostile, "https://example.org/p")
            .unwrap_err(),
        thread
            .record_fact("f".repeat(MAX_PARAPHRASE_CHARS + 1))
            .unwrap_err(),
        thread.record_fact("  ").unwrap_err(),
    ] {
        let rendered = error.to_string();
        assert!(
            !rendered.contains("marker-9f3a") && !rendered.contains("marker9f3a"),
            "the refusal quoted its input back: {rendered}"
        );
        assert!(!rendered.contains("javascript:"), "{rendered}");
        // A fixed reason, not a container for whatever arrived.
        assert!(rendered.len() < 120, "{rendered}");
    }
}

#[test]
fn a_title_or_alt_text_cannot_carry_a_bidi_override_onto_a_card() {
    // S2. A source title renders inline next to the honestly-computed domain
    // chip — the one surface whose whole purpose is truthful attribution — and
    // alt text is spoken by TTS. `U+202E` in a fetched page's title reverses
    // what a human reads there. The repo already strips exactly these characters
    // for list lines and folds them in `markdown::escape`; this closes the one
    // display path that missed the treatment, and it does it in the *recorder*
    // so no projection has to remember.
    let mut thread = ResearchThread::new("Ramen \u{202e}gnimaerts\u{202c}");
    assert!(!thread.topic().contains('\u{202e}'), "{}", thread.topic());

    let hostile = "Wikipedia\u{202e}gpj.exe\u{202c} \u{200b}hidden\u{0007}\tand\nlines";
    thread
        .record_source(hostile, "https://example.org/a")
        .unwrap();
    thread
        .record_image(
            hostile,
            "https://cdn.example.org/a.jpg",
            "https://example.org/a",
        )
        .unwrap();

    for label in [thread.sources()[0].title(), thread.images()[0].alt()] {
        for forbidden in ['\u{202e}', '\u{202c}', '\u{200b}', '\u{0007}', '\n', '\t'] {
            assert!(
                !label.contains(forbidden),
                "{forbidden:?} survived into {label:?}"
            );
        }
        // Stripped, not emptied: the words a human needs are still there, and
        // the label stays one line with no double spaces.
        assert!(label.contains("Wikipedia"), "{label:?}");
        assert!(label.contains("hidden"), "{label:?}");
        assert!(!label.contains("  "), "{label:?}");
        assert_eq!(label.trim(), label);
    }
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
    assert!(thread.facts().is_empty());
    assert_eq!(thread.record_fact("   "), Err(ThreadError::Empty));
    assert!(thread.record_fact("Kome opens at noon.").is_ok());
    // A thread does not repeat itself.
    assert!(thread.record_fact("Kome opens at noon.").is_ok());
    assert_eq!(thread.facts().len(), 1);
}

#[test]
fn a_source_without_an_honest_attribution_is_refused() {
    let mut thread = ResearchThread::new("Ramen");
    assert!(matches!(
        thread.record_source("Evil", "javascript:alert(1)"),
        Err(ThreadError::Unattributable)
    ));
    assert!(thread.sources().is_empty());
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
    assert_eq!(thread.sources().len(), 1);
}

#[test]
fn an_image_cannot_be_recorded_without_its_own_source() {
    let mut thread = ResearchThread::new("Ramen");
    // Image fine, provenance not: refused, because a tile with no honest badge
    // would have to borrow another image's (ADR-017 forbids exactly that).
    assert!(matches!(
        thread.record_image("bowl", "https://cdn.example.org/a.jpg", ""),
        Err(ThreadError::Unattributable)
    ));
    assert!(matches!(
        thread.record_image("bowl", "data:image/png;base64,AA", "https://example.org/p"),
        Err(ThreadError::Unattributable)
    ));
    assert!(thread.images().is_empty());
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
        .map(|i| i.source_url())
        .collect();
    let mut deduped = sources.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(deduped.len(), sources.len());
    // The full record (what the artifact gets) is not truncated by the cap.
    assert_eq!(thread.images().len(), GALLERY_IMAGE_CAP + 5);
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
