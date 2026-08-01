//! Z4-safe markdown rendering helpers (docs/06 §2, FR-08).
//!
//! Two promotion paths render untrusted text into a versioned markdown
//! artifact: Research Notes ([`crate::deepdive::render_research_notes`], ADR-017)
//! and a promoted list ([`crate::lists::ItemList::to_markdown`], ADR-024). Both
//! face the same problem, so both use the same escaper rather than each growing
//! its own near-miss copy.
//!
//! The rule is one sentence: **the document's structure is ours, its content is
//! data**. A fact, a source title, or a shopping-list line may say
//! `# Owned`, `- [x] already done`, `<script>`, or `[click](https://evil)` — none
//! of it may become a heading, a list marker, a tag, or a link. [`escape`]
//! guarantees that by backslash-escaping every character that could *start* a
//! markdown construct (CommonMark defines a backslash before any ASCII
//! punctuation as rendering that literal character, so the escape is always
//! valid and always inert) and by folding control and bidi characters to spaces.
//! Nothing is deleted: the words survive, only their authority does not.
//!
//! [`safe_link`] is the matching rule for URLs: a link target is emitted only
//! when it is plainly `http(s)`, so a `javascript:` or `data:` "source" is
//! rendered as inert text instead of an activatable link.

/// Characters that open a markdown construct and are therefore never allowed
/// through unescaped.
///
/// * `\` — the escape itself.
/// * `` ` `` `*` `_` `~` — code spans/fences, emphasis, strikethrough.
/// * `[` `]` `!` — links and images (with `[` escaped no `](…)` target can form).
/// * `<` `>` — raw HTML and block quotes.
/// * `#` — ATX headings.
/// * `|` — tables.
/// * `-` `+` — bullet-list markers.
const OPENERS: &[char] = &[
    '\\', '`', '*', '_', '~', '[', ']', '!', '<', '>', '#', '|', '-', '+',
];

/// Neutralise untrusted text for inclusion in a markdown document.
///
/// Control characters (including newlines, tabs and the bidi overrides) become
/// spaces — untrusted text never introduces line structure, because a line break
/// is how a nested block would escape the line it was placed on. Every character
/// in [`OPENERS`] is backslash-escaped, as is the `.` or `)` that would turn a
/// leading numeral into an ordered-list marker (`1. …`). The result is trimmed.
pub fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    // Tracks a run of digits from the very start of the string: only there can
    // a `.`/`)` become an ordered-list marker, so only there is it escaped —
    // "Dr. Who" and "v1.2" keep their dots.
    let mut leading_digits = true;
    for ch in raw.chars() {
        match ch {
            // Control characters (including the bidi overrides) never survive.
            c if c.is_control() => {
                leading_digits = false;
                out.push(' ');
            }
            '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                leading_digits = false;
                out.push(' ');
            }
            c if OPENERS.contains(&c) => {
                leading_digits = false;
                out.push('\\');
                out.push(c);
            }
            '.' | ')' if leading_digits && !out.is_empty() => {
                leading_digits = false;
                out.push('\\');
                out.push(ch);
            }
            c => {
                leading_digits = leading_digits && c.is_ascii_digit();
                out.push(c);
            }
        }
    }
    out.trim().to_owned()
}

/// A URL is only emitted as a link target if it is plainly http(s) — a
/// `javascript:` or `data:` "source" is rendered as inert text instead.
pub fn safe_link(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        // Parentheses would close the markdown link target early.
        Some(trimmed.replace('(', "%28").replace(')', "%29"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every construct-opening character in `escaped` that is NOT preceded by a
    /// backslash — i.e. every character that could still become markup. The
    /// escaper's whole contract is that this is empty for any input.
    fn unescaped_openers(escaped: &str) -> Vec<char> {
        let chars: Vec<char> = escaped.chars().collect();
        chars
            .iter()
            .enumerate()
            .filter(|(idx, ch)| {
                OPENERS.contains(ch)
                    && **ch != '\\'
                    && idx.checked_sub(1).and_then(|p| chars.get(p)) != Some(&'\\')
            })
            .map(|(_, ch)| *ch)
            .collect()
    }

    #[test]
    fn every_construct_opener_is_escaped() {
        let escaped = escape("# Owned <script>alert(1)</script> [link](https://evil.example) `x`");
        assert!(!escaped.starts_with("# "));
        assert!(!escaped.contains("<script>"));
        assert!(!escaped.contains("`x`"));
        // The link's closing bracket survives only in its inert `\]` form, so
        // no `](…)` target can form.
        assert!(escaped.contains("\\]("), "{escaped}");
        assert!(unescaped_openers(&escaped).is_empty(), "{escaped}");
        // Escaping, not deletion: the words are all still there.
        assert!(escaped.contains("Owned"));
        assert!(escaped.contains("alert"));
        for opener in OPENERS {
            let out = escape(&format!("a{opener}b"));
            assert!(
                out.contains(&format!("\\{opener}")),
                "{opener:?} must be escaped, got {out:?}"
            );
        }
    }

    #[test]
    fn line_structure_cannot_be_smuggled_in() {
        let escaped = escape("milk\n# Heading\r\n- item\u{202e}");
        assert!(!escaped.contains('\n'), "no line breaks survive: {escaped:?}");
        assert!(!escaped.contains('\r'));
        assert!(!escaped.contains('\u{202e}'));
        assert!(escaped.contains("Heading"));
    }

    #[test]
    fn a_leading_numeral_cannot_become_an_ordered_list_marker() {
        assert_eq!(escape("1. first"), "1\\. first");
        assert_eq!(escape("12) second"), "12\\) second");
        // Mid-text punctuation stays readable — this is not blanket escaping.
        assert_eq!(escape("Dr. Who (v1.2)"), "Dr. Who (v1.2)");
    }

    #[test]
    fn only_http_urls_become_link_targets() {
        assert_eq!(
            safe_link(" https://example.com/a "),
            Some("https://example.com/a".to_owned())
        );
        assert_eq!(
            safe_link("https://example.com/a(b)"),
            Some("https://example.com/a%28b%29".to_owned())
        );
        for hostile in ["javascript:alert(1)", "data:text/html,x", "file:///etc", ""] {
            assert_eq!(safe_link(hostile), None, "{hostile:?} must not link");
        }
    }
}
