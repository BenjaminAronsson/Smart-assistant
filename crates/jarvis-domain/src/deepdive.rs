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
//! Part two adds the pure decisions the cards and the browser handoff rest on:
//! [`display_domain`] (the attribution label, computed from a *parsed* host so a
//! userinfo or homograph URL cannot spoof a chip), [`select_source`] (which
//! consulted page "open the second one" means), and the thread's guarded
//! recorders ([`ResearchThread::record_fact`] and friends) that keep a scrape
//! from being filed as a paraphrase.
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

/// Whether the user explicitly told the thread to end ("new topic", "never
/// mind"). Exposed because callers that *override* the classifier — the source
/// handoff, which knows the query refers to a cited page — still have to let an
/// explicit instruction win (docs/12 §2.5).
pub fn is_explicit_reset(query: &str) -> bool {
    let q = normalize(query);
    NEW_TOPIC_MARKERS.iter().any(|m| q.contains(m))
}

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
    if is_explicit_reset(query) {
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

// ---------------------------------------------------------------------------
// Source handoff — "open that / read it" (ADR-017 §3)
// ---------------------------------------------------------------------------

/// Phrases that mean "stop summarising and put the real page in front of me".
/// Reading a source is a **browser handoff** (FR-15), never HUD re-rendering:
/// the HUD reproducing full page content would be both a scope and a copyright
/// boundary violation (docs/12 §2.5).
const HANDOFF_OPENERS: &[&str] = &[
    "open that",
    "open it",
    "open the",
    "open this",
    "read it",
    "read that",
    "read the",
    "let me read",
    "i want to read",
    "show me the source",
    "show me the page",
    "go to the",
];

/// Ordinals a user says when picking among cited sources. Index is the position
/// in the sources list; the words are matched as whole tokens.
const ORDINALS: &[(&str, usize)] = &[
    ("first", 0),
    ("1st", 0),
    ("second", 1),
    ("2nd", 1),
    ("third", 2),
    ("3rd", 2),
    ("fourth", 3),
    ("4th", 3),
    ("fifth", 4),
    ("5th", 4),
    ("sixth", 5),
    ("6th", 5),
    ("seventh", 6),
    ("7th", 6),
    ("eighth", 7),
    ("8th", 7),
];

/// Whether this utterance asks to *read the source itself* rather than hear more
/// about it (ADR-017 §3). Recognising it is all this function does — it grants
/// nothing. The caller turns a `true` into a **proposal** for the browser
/// worker, which `policy::evaluate` still has to authorize (invariant #1).
pub fn is_source_handoff(query: &str) -> bool {
    let q = normalize(query);
    if q.is_empty() {
        return false;
    }
    HANDOFF_OPENERS
        .iter()
        .any(|opener| q == *opener || q.starts_with(opener) || q.contains(opener))
}

/// Which consulted source "open that / open the second one" refers to.
///
/// An explicit ordinal wins and must be in range — "open the fifth one" against
/// three sources resolves to nothing rather than silently opening a different
/// page than the one asked for. With no ordinal it is the **first** source,
/// which is the one just cited; if that guess is wrong the cost is one visible
/// browser tab and one more utterance, and the user can name the ordinal.
/// Returns `None` when there is nothing to open.
pub fn select_source(query: &str, source_count: usize) -> Option<usize> {
    if source_count == 0 {
        return None;
    }
    let q = normalize(query);
    for word in q.split(' ') {
        if let Some((_, index)) = ORDINALS.iter().find(|(name, _)| *name == word) {
            return (*index < source_count).then_some(*index);
        }
    }
    Some(0)
}

// ---------------------------------------------------------------------------
// Attribution — the label on a source chip (ADR-014/ADR-017)
// ---------------------------------------------------------------------------

/// The largest gallery the HUD will show (docs/12 §2.3, ADR-017: "capped at
/// 6-8"). The cap is not cosmetic — each tile is its own search+fetch against a
/// single-flight CLI budget (ADR-011), so an uncapped gallery is a real quota
/// and latency cost. Enforced where the card is built, not left to the producer
/// to remember.
pub const GALLERY_IMAGE_CAP: usize = 8;

/// Whether a URL is a plain `http(s)` URL — the only kind this system will emit
/// as a link target, navigate to, or badge with a domain. `javascript:`,
/// `data:`, and `file:` never qualify.
pub fn is_web_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// The display domain for a source chip ("wikipedia.org ↗"), computed **once,
/// host-side, from the parsed host** so the client never derives trusted-looking
/// text from an untrusted URL (docs/12 §2.3).
///
/// This is an anti-spoofing function, which is why it is fussy:
///
/// * **Userinfo is discarded, not read as the host.**
///   `https://wikipedia.org@evil.example/x` labels `evil.example` — labelling it
///   `wikipedia.org` would hand an attacker a trusted-looking chip pointing at
///   their page. The host is what follows the *last* `@`.
/// * **Non-ASCII hosts are refused** (`None`). A raw Unicode host is the
///   homograph attack (`wikipediа.org` with a Cyrillic а); punycode (`xn--…`)
///   is ASCII and passes through visibly encoded, which is the honest rendering.
/// * **Only `http(s)`**, and only a conservative host character set. Anything
///   else yields `None`, and a source with no computable domain is dropped
///   rather than shown with a fabricated or partial label.
///
/// `www.` is stripped and the result is lowercased; the port, path, query and
/// fragment are not part of the label.
pub fn display_domain(url: &str) -> Option<String> {
    if !is_web_url(url) {
        return None;
    }
    let after_scheme = url.trim().split_once("//")?.1;
    // Authority ends at the first path/query/fragment delimiter.
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    // Userinfo precedes the LAST '@' — taking the first would let
    // `a@b@evil.example` re-open the very spoof this guards against.
    let host_port = match authority.rsplit_once('@') {
        Some((_userinfo, host)) => host,
        None => authority,
    };
    // IPv6 literals are bracketed; everything else cuts at the port separator.
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        let (inside, _) = rest.split_once(']')?;
        inside
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || !host.is_ascii() {
        return None;
    }
    // A hostname (or IP literal) and nothing else: no spaces, no markup, no
    // control characters that could dress the chip up as something it is not.
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
    {
        return None;
    }
    Some(host.strip_prefix("www.").unwrap_or(&host).to_owned())
}

// ---------------------------------------------------------------------------
// The accumulating thread
// ---------------------------------------------------------------------------

/// The paraphrase budget for one recorded fact (ADR-017: facts are
/// "paraphrased, not scraped").
///
/// A paraphrase is a sentence or two that Jarvis composed; a scrape is a page.
/// The distinction is not decidable from the text, but the *size* separates them
/// well enough to be worth enforcing: this cap makes "just store the extracted
/// page text" fail loudly at the boundary instead of quietly producing a
/// copyright-shaped artifact. Deliberately generous, so a legitimate long
/// summary still fits.
pub const MAX_PARAPHRASE_CHARS: usize = 400;

/// Why a thread refused to record something.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ThreadError {
    /// The "fact" is longer than a paraphrase: ADR-017 requires the thread to
    /// accumulate Jarvis's own summary, never fetched page text.
    #[error(
        "a recorded fact must be a paraphrase of at most {MAX_PARAPHRASE_CHARS} characters, not fetched page text"
    )]
    NotAParaphrase,
    /// Empty after trimming — nothing to record.
    #[error("nothing to record")]
    Empty,
    /// A source or image URL that is not a plain `http(s)` URL, or whose host
    /// cannot be turned into an honest attribution label.
    #[error("not an attributable http(s) source: {0}")]
    Unattributable(String),
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

impl ResearchThread {
    /// Start a thread on a topic.
    pub fn new(topic: impl Into<String>) -> ResearchThread {
        ResearchThread {
            topic: topic.into(),
            ..ResearchThread::default()
        }
    }

    /// Record one **paraphrased** fact (ADR-017).
    ///
    /// The guard is the point: anything over [`MAX_PARAPHRASE_CHARS`] is
    /// rejected rather than truncated, because a truncated scrape is still a
    /// scrape. A caller holding page text has to summarise it first — which is
    /// exactly the behaviour the ADR asks for, made non-optional.
    pub fn record_fact(&mut self, fact: impl Into<String>) -> Result<(), ThreadError> {
        let text = fact.into().trim().to_owned();
        if text.is_empty() {
            return Err(ThreadError::Empty);
        }
        if text.chars().count() > MAX_PARAPHRASE_CHARS {
            return Err(ThreadError::NotAParaphrase);
        }
        if !self.facts.contains(&text) {
            self.facts.push(text);
        }
        Ok(())
    }

    /// Record a consulted page. The URL must be `http(s)` with an attributable
    /// host, so every entry in the bibliography can carry a real link. Repeats
    /// of the same URL are ignored — a thread cites a page once.
    pub fn record_source(
        &mut self,
        title: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<(), ThreadError> {
        let url = url.into().trim().to_owned();
        if display_domain(&url).is_none() {
            return Err(ThreadError::Unattributable(url));
        }
        if self.sources.iter().any(|s| s.url == url) {
            return Ok(());
        }
        self.sources.push(SourceRef {
            title: title.into().trim().to_owned(),
            url,
        });
        Ok(())
    }

    /// Record a referenced image **with its own provenance** (ADR-017). Both the
    /// image URL and the page it came from must be attributable `http(s)`; there
    /// is no path here that stores an image without its individual source, which
    /// is what stops a gallery from sharing one badge across pages.
    pub fn record_image(
        &mut self,
        alt: impl Into<String>,
        url: impl Into<String>,
        source_url: impl Into<String>,
    ) -> Result<(), ThreadError> {
        let url = url.into().trim().to_owned();
        let source_url = source_url.into().trim().to_owned();
        for candidate in [&url, &source_url] {
            if display_domain(candidate).is_none() {
                return Err(ThreadError::Unattributable(candidate.clone()));
            }
        }
        if self.images.iter().any(|i| i.url == url) {
            return Ok(());
        }
        self.images.push(ImageRef {
            alt: alt.into().trim().to_owned(),
            url,
            source_url,
        });
        Ok(())
    }

    /// The images a gallery card may show — the ADR-017 cap applied once, here,
    /// rather than trusted to each producer (docs/12 §2.3).
    pub fn gallery_images(&self) -> &[ImageRef] {
        &self.images[..self.images.len().min(GALLERY_IMAGE_CAP)]
    }

    /// Whether this thread has accumulated anything worth keeping. A topic
    /// alone is not: promoting a bare heading would produce a document that
    /// says nothing, and shelving one is not a loss worth reporting.
    pub fn has_content(&self) -> bool {
        !self.facts.is_empty() || !self.sources.is_empty() || !self.images.is_empty()
    }
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
    if is_web_url(trimmed) {
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
