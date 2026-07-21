//! Synthesis primitives (F2.10, FR-29/30, ADR-016/020, docs/02 §11d). Pure text
//! shaping the orchestrator applies when composing an answer — no I/O, no LLM.
//! Two behaviours:
//!
//! * **Ambiguity clarification (ADR-016):** a genuinely ambiguous query yields
//!   *one fluent clarifying question*, never a multi-option picker
//!   ([`clarifying_question`]).
//! * **Contested-topic framing (FR-30, ADR-020):** for contested/political/
//!   conflict topics ([`is_contested_topic`]), claims are **attributed to their
//!   source** rather than asserted as fact, and presented even-handedly
//!   ([`frame_contested`]). Quality weighting (ADR-016) selects *which* sources;
//!   this governs *how* their claims are voiced.
//!
//! These are the primitives; the routing signal that decides *when* to clarify or
//! frame (and the live synthesis) is orchestrator wiring (F2.11 golden traces).

/// Join interpretations into one conversational "a, b, or c" phrase (Oxford
/// "or"). Never a bulleted list — this is spoken/caption text, not a picker.
fn join_or(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [a, b] => format!("{a} or {b}"),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
    }
}

/// One fluent clarifying question for a genuinely ambiguous query (ADR-016,
/// docs/12 §2.4): a single conversational sentence naming the distinct
/// interpretations, **never** a multi-option picker. Returns `None` when there
/// are fewer than two *distinct* interpretations — a merely broad or
/// under-specified query is not ambiguous and must not trigger a question
/// (don't over-ask). The result is guaranteed single-line (no `\n`), so a caller
/// cannot accidentally render it as a list.
pub fn clarifying_question(interpretations: &[&str]) -> Option<String> {
    let mut distinct: Vec<String> = Vec::new();
    for candidate in interpretations {
        let trimmed = candidate.trim();
        if !trimmed.is_empty() && !distinct.iter().any(|d| d.eq_ignore_ascii_case(trimmed)) {
            distinct.push(trimmed.to_owned());
        }
    }
    if distinct.len() < 2 {
        return None;
    }
    // A single spoken sentence — the whitespace-collapse guarantees no newline
    // survives from an interpretation, so it can never become a picker.
    let question = format!("Did you mean {}?", join_or(&distinct));
    Some(question.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Markers of a contested / political / conflict topic (ADR-020). Deliberately
/// about *kinds* of topic (elections, war, protests, sanctions…), not named
/// parties, so the framing rule applies even-handedly by subject rather than by
/// who is involved. Conservative — the rule adds attribution, so a false positive
/// only makes a benign topic slightly more hedged, never less safe.
const CONTESTED_MARKERS: &[&str] = &[
    "election",
    "war",
    "conflict",
    "invasion",
    "protest",
    "sanction",
    "airstrike",
    "ceasefire",
    "coup",
    "genocide",
    "casualt", // casualty / casualties
    "militant",
    "insurgen",
    "referendum",
    "impeach",
    "uprising",
    "occupation",
];

/// Whether a topic is contested/political/conflict-related (case-insensitive
/// substring match on [`CONTESTED_MARKERS`]) and should therefore be voiced with
/// source attribution and even-handedness (FR-30, ADR-020).
pub fn is_contested_topic(topic: &str) -> bool {
    let lower = topic.to_lowercase();
    CONTESTED_MARKERS.iter().any(|m| lower.contains(m))
}

/// A claim from the news, tied to the source that made it. The source is what
/// lets the synthesis **attribute** rather than assert (FR-30).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcedClaim {
    pub source: String,
    pub claim: String,
}

impl SourcedClaim {
    pub fn new(source: impl Into<String>, claim: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            claim: claim.into(),
        }
    }
}

/// Frame contested claims (FR-30, ADR-020): every claim is **attributed to its
/// source** ("According to <source>, <claim>") — never voiced as bare fact — so
/// the hedging present in good reporting is preserved and contested points from
/// different sources sit side by side (even-handed). The caller supplies quality-
/// weighted sources (ADR-016); this only governs voicing. Empty input yields an
/// empty string (nothing to say), never a fabricated claim.
///
/// Avoiding sensationalised graphic detail in the *spoken* summary (ADR-020) is a
/// synthesis-prompt constraint on the LLM that generates the claims, not a
/// property this text-shaper can enforce after the fact; it is documented at the
/// synthesis wiring (F2.11).
pub fn frame_contested(claims: &[SourcedClaim]) -> String {
    claims
        .iter()
        .map(|c| format!("According to {}, {}", c.source.trim(), c.claim.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_interpretations_yield_one_single_line_question() {
        let q = clarifying_question(&["the medical condition microcondia", "the band Microcondia"])
            .expect("ambiguous → a question");
        assert!(q.starts_with("Did you mean "), "{q}");
        assert!(q.ends_with('?'));
        // Single fluent sentence, never a picker: no newline, no bullet.
        assert!(!q.contains('\n'), "must be one line, not a picker: {q}");
        assert!(!q.contains(" - ") && !q.contains('•'));
        assert!(q.contains("microcondia") && q.contains("Microcondia"));
    }

    #[test]
    fn three_interpretations_use_an_oxford_or() {
        let q = clarifying_question(&["a", "b", "c"]).unwrap();
        assert_eq!(q, "Did you mean a, b, or c?");
    }

    #[test]
    fn fewer_than_two_distinct_interpretations_is_not_ambiguous() {
        assert!(clarifying_question(&[]).is_none());
        assert!(clarifying_question(&["only one"]).is_none());
        // Duplicates (case-insensitive) collapse — not genuinely ambiguous.
        assert!(clarifying_question(&["Paris", "paris", "  paris "]).is_none());
    }

    #[test]
    fn classifies_contested_topics() {
        assert!(is_contested_topic("the latest on the Iran sanctions"));
        assert!(is_contested_topic("election results"));
        assert!(is_contested_topic("ceasefire talks"));
        // Not contested: everyday topics stay unframed.
        assert!(!is_contested_topic("best pasta recipe"));
        assert!(!is_contested_topic("who won the football match"));
    }

    #[test]
    fn contested_framing_attributes_every_claim_never_asserts() {
        let claims = [
            SourcedClaim::new("Reuters", "talks resumed on Tuesday"),
            SourcedClaim::new("AP", "no agreement was reached"),
        ];
        let framed = frame_contested(&claims);
        // Every claim carries its source — attribution, not assertion.
        assert!(framed.contains("According to Reuters, talks resumed on Tuesday"));
        assert!(framed.contains("According to AP, no agreement was reached"));
        // Both sides present (even-handed): two distinct sources voiced.
        assert_eq!(framed.matches("According to ").count(), 2);
    }

    #[test]
    fn empty_claims_frame_to_empty_never_fabricated() {
        assert_eq!(frame_contested(&[]), "");
    }
}
