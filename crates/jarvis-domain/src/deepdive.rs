//! Deep-dive thread continuity and the Research Notes document (FR-27,
//! ADR-017, docs/12 §2.5).
//!
//! A deep dive is a *thread*, not a series of unrelated queries. Two pure
//! decisions live here, in the same place and shape as the other routing
//! signals (ADR-015 location, ADR-016 ambiguity — `crate::synthesis`):
//!
//! * [`classify_query`] — **continuation vs. new topic.** A continuation
//!   *extends* the canvas (append cards, prior cards stay); only a genuine
//!   topic change shelves it (FR-24 unchanged for that case). Getting this
//!   wrong is cheap and reversible — "new topic" by voice, or Restore — which
//!   is precisely why it is a deterministic classifier and not a model call.
//! * [`render_research_notes`] — the markdown of a promoted thread (FR-08):
//!   accumulated **paraphrased** facts, every source consulted, and referenced
//!   images.
//!
//! No I/O, no clock, no allocation of authority: this module decides *shape*,
//! never side effects.

/// How a new query relates to the live thread (ADR-017). Exhaustive: a third
/// answer would change the canvas lifecycle, which is a spec decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryRelation {
    /// Extends the live thread — append cards, do not shelve.
    Continuation,
    /// A genuine topic change — shelve the canvas (FR-24).
    NewTopic,
}

/// Explicit continuation markers (docs/12 §2.5 names these exact shapes).
/// Matched on a normalized prefix/substring basis, so "Tell me more about the
/// second one" counts and "More Ramen in Berlin" does not.
const CONTINUATION_OPENERS: &[&str] = &[
    "tell me more",
    "more about",
    "what about",
    "how about",
    "compare that",
    "compare them",
    "compare it",
    "and what",
    "what else",
    "anything else",
    "go on",
    "keep going",
    "why is that",
    "why does that",
    "who is that",
    "show me the references",
    "show me more",
    "open that",
    "read it",
    "read that",
];

/// Back-reference pronouns. On their own they are weak evidence; combined with
/// the absence of a fresh subject they are what makes "why is it closed?" a
/// follow-up rather than a new question.
const BACK_REFERENCES: &[&str] = &[
    "that", "those", "it", "them", "they", "this", "these", "there", "its", "their",
];

/// An explicit reset always wins — the user saying "new topic" is not a signal
/// to be weighed, it is an instruction (docs/12 §2.5: correcting a
/// misclassification costs one utterance).
const NEW_TOPIC_MARKERS: &[&str] = &[
    "new topic",
    "forget that",
    "never mind",
    "different question",
];

fn normalize(text: &str) -> String {
    text.trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Content words of the live thread's topic, used for overlap scoring.
fn topic_terms(topic: &str) -> Vec<String> {
    normalize(topic)
        .split(' ')
        .filter(|w| w.len() > 3 && !is_stop_word(w))
        .map(str::to_owned)
        .collect()
}

fn is_stop_word(word: &str) -> bool {
    matches!(
        word,
        "the"
            | "and"
            | "for"
            | "with"
            | "from"
            | "about"
            | "what"
            | "when"
            | "where"
            | "which"
            | "that"
            | "this"
            | "near"
            | "your"
            | "mine"
            | "into"
            | "over"
            | "than"
            | "then"
    )
}

/// Classify a query against the live thread's topic (ADR-017).
///
/// With no live thread (`active_topic` empty) everything is a new topic — there
/// is nothing to continue. Otherwise, in order: an explicit reset is a new
/// topic; an explicit continuation opener is a continuation; a query that
/// shares a content word with the live topic is a continuation; a short query
/// that leans on a back-reference and introduces no new content word is a
/// continuation. Everything else is a new topic.
///
/// The bias is deliberate: shelving is reversible with one keystroke, while
/// appending unrelated cards quietly corrupts a thread. So the classifier
/// requires *evidence* of continuity rather than assuming it.
pub fn classify_query(active_topic: &str, query: &str) -> QueryRelation {
    let q = normalize(query);
    if q.is_empty() || normalize(active_topic).is_empty() {
        return QueryRelation::NewTopic;
    }
    if NEW_TOPIC_MARKERS.iter().any(|m| q.contains(m)) {
        return QueryRelation::NewTopic;
    }
    if CONTINUATION_OPENERS
        .iter()
        .any(|opener| q == *opener || q.starts_with(opener) || q.contains(opener))
    {
        return QueryRelation::Continuation;
    }

    let terms = topic_terms(active_topic);
    let query_words: Vec<&str> = q.split(' ').collect();
    if query_words.iter().any(|w| {
        terms
            .iter()
            .any(|t| t == w || (w.len() > 4 && t.starts_with(w)))
    }) {
        return QueryRelation::Continuation;
    }

    // A short query resting on a back-reference ("why is it closed?", "is that
    // far?") can only be about what is already on the canvas — the pronoun has
    // no other referent. Predicate words ("closed", "far") do not make it a new
    // subject, which an earlier stricter rule got wrong: it sent every such
    // follow-up to shelve the canvas the pronoun was pointing at.
    let has_back_reference = query_words.iter().any(|w| BACK_REFERENCES.contains(w));
    if has_back_reference && query_words.len() <= 8 {
        return QueryRelation::Continuation;
    }

    QueryRelation::NewTopic
}

/// Whether to offer promoting the thread to a Research Notes artifact
/// (`[ui] deepdive_promote_after`, default 3 — ADR-017).
///
/// "Offer" is the operative word: the offer is spoken in Jarvis's normal voice
/// (docs/12 §2.5), never a modal, and it is made once per threshold crossing —
/// `follow_ups` counts follow-ups, so a caller that has already offered passes
/// the count it offered at and gets `false` until the next one.
pub fn should_offer_promotion(
    follow_ups: u32,
    threshold: u32,
    already_offered_at: Option<u32>,
) -> bool {
    if threshold == 0 || follow_ups < threshold {
        return false;
    }
    already_offered_at != Some(follow_ups)
}

/// One source consulted during a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRef {
    pub title: String,
    pub url: String,
}

/// One image referenced by the thread, with its own provenance — images from
/// different pages must not share one attribution (ADR-017).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub alt: String,
    pub url: String,
    pub source_url: String,
}

/// The accumulated thread, ready to become a versioned markdown artifact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResearchThread {
    pub topic: String,
    /// **Paraphrased** facts (ADR-017: paraphrased, not scraped). The caller is
    /// responsible for the paraphrasing; this type carries the intent in its
    /// name so a future contributor does not quietly start storing page text.
    pub facts: Vec<String>,
    pub sources: Vec<SourceRef>,
    pub images: Vec<ImageRef>,
}

/// Neutralise untrusted text for markdown output.
///
/// Facts, titles and alt text originate in fetched pages (Z4). The artifact
/// renderer already refuses to execute markup, but a promoted document is also
/// read by humans and other tools, so nothing here is allowed to *become*
/// markup: control characters go, and the characters that open markup or a
/// link are escaped rather than stripped, keeping the text readable.
fn escape_markdown(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            // Control characters (including the bidi overrides) never survive.
            c if c.is_control() => out.push(' '),
            '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                out.push(' ')
            }
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|' => {
                out.push('\\');
                out.push(ch);
            }
            c => out.push(c),
        }
    }
    out.trim().to_owned()
}

/// A URL is only emitted as a link target if it is plainly http(s) — a
/// `javascript:` or `data:` "source" is rendered as inert text instead.
fn safe_link(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        // Parentheses would close the markdown link target early.
        Some(trimmed.replace('(', "%28").replace(')', "%29"))
    } else {
        None
    }
}

/// Render a thread as the Research Notes markdown document (FR-08, ADR-017):
/// paraphrased facts, every source consulted, and referenced images — each
/// image individually attributed.
pub fn render_research_notes(thread: &ResearchThread) -> String {
    let mut md = String::new();
    let topic = escape_markdown(&thread.topic);
    md.push_str("# Research Notes: ");
    md.push_str(if topic.is_empty() {
        "untitled thread"
    } else {
        &topic
    });
    md.push_str("\n\n");

    md.push_str("## What I found\n\n");
    if thread.facts.is_empty() {
        md.push_str("_No findings recorded._\n");
    } else {
        for fact in &thread.facts {
            let text = escape_markdown(fact);
            if !text.is_empty() {
                md.push_str("- ");
                md.push_str(&text);
                md.push('\n');
            }
        }
    }

    md.push_str("\n## Sources\n\n");
    if thread.sources.is_empty() {
        md.push_str("_No sources consulted._\n");
    } else {
        for source in &thread.sources {
            let title = escape_markdown(&source.title);
            let label = if title.is_empty() { "untitled" } else { &title };
            match safe_link(&source.url) {
                Some(url) => md.push_str(&format!("- [{label}]({url})\n")),
                // Not a link we will render as one — kept as text so the record
                // is still complete and honest.
                None => md.push_str(&format!("- {label} ({})\n", escape_markdown(&source.url))),
            }
        }
    }

    if !thread.images.is_empty() {
        md.push_str("\n## Images\n\n");
        for image in &thread.images {
            let alt = escape_markdown(&image.alt);
            let label = if alt.is_empty() { "image" } else { &alt };
            // Every image carries its OWN source link (ADR-017) — one shared
            // attribution is not acceptable when provenance differs.
            match (safe_link(&image.url), safe_link(&image.source_url)) {
                (Some(url), Some(source)) => {
                    md.push_str(&format!("- ![{label}]({url}) — source: {source}\n"))
                }
                (_, Some(source)) => md.push_str(&format!("- {label} — source: {source}\n")),
                _ => md.push_str(&format!("- {label} (source unknown)\n")),
            }
        }
    }

    md
}
