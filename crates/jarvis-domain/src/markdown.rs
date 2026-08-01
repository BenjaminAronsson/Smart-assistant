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
//! [`safe_link`] is the matching rule for URLs, and it is the same rule twice
//! over: a link target is emitted only when it is plainly `http(s)` **and**
//! plainly a URL — every character ASCII-graphic. Escaping the link *text* buys
//! nothing if the link *destination* can carry a newline, because CommonMark
//! refuses such a destination and the injected tail becomes document structure
//! (a heading, or a second, real anchor). So a `javascript:` "source" and a
//! `https://` URL with a newline in it are both rendered as inert text rather
//! than as an activatable link. [`is_web_url`] is that rule's single definition;
//! [`crate::deepdive::display_domain`] and the thread recorders go through it
//! too, so a URL that cannot be linked also cannot be badged or navigated to.

use crate::tools::is_bidi_or_zero_width;

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

/// A character that is folded to a space rather than carried through.
///
/// Three families, one reason each:
///
/// * C0/C1 controls ([`char::is_control`]) — newlines and tabs above all.
///   Untrusted text never introduces line structure, because a line break is how
///   a nested block would escape the line it was placed on.
/// * Bidi controls and zero-width format characters
///   ([`crate::tools::is_bidi_or_zero_width`]) — category `Cf`, which
///   `is_control` does **not** cover. Shared with the tool-result validator so
///   the two sanitizers cannot drift into differently-safe.
/// * `U+2028`/`U+2029`, the Unicode line and paragraph separators — line
///   structure by another name. (The tool-result validator keeps `\n`, so it
///   has no reason to fold these; a single markdown line does.)
fn is_folded(ch: char) -> bool {
    ch.is_control() || is_bidi_or_zero_width(ch) || matches!(ch, '\u{2028}' | '\u{2029}')
}

/// Neutralise untrusted text for inclusion in a markdown document.
///
/// Everything [`is_folded`] covers becomes a space — untrusted text never
/// introduces line structure. Every character in [`OPENERS`] is
/// backslash-escaped, as is the `.` or `)` that would turn a leading numeral
/// into an ordered-list marker (`1. …`). The result is trimmed.
///
/// The ordered-list guard is applied to the **trimmed** result, not while
/// scanning: leading whitespace (or a character folded to a space) would
/// otherwise clear the "still in the leading digits" state and then be trimmed
/// away again, handing the marker back its authority.
pub fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if is_folded(ch) {
            out.push(' ');
        } else if OPENERS.contains(&ch) {
            out.push('\\');
            out.push(ch);
        } else {
            out.push(ch);
        }
    }

    let trimmed = out.trim();
    // A run of digits at the very start is the only place a `.`/`)` can become
    // an ordered-list marker, so it is the only place one is escaped — "Dr. Who"
    // and "v1.2" keep their dots. Digits are ASCII, so the count is also the
    // byte offset.
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && matches!(trimmed.as_bytes().get(digits), Some(b'.' | b')')) {
        let mut guarded = String::with_capacity(trimmed.len() + 1);
        guarded.push_str(&trimmed[..digits]);
        guarded.push('\\');
        guarded.push_str(&trimmed[digits..]);
        return guarded;
    }
    trimmed.to_owned()
}

/// Characters an `http(s)` URL may never contain, on top of everything that is
/// not ASCII-graphic. RFC 3986 excludes all of them from a URI; each is also a
/// way for a "URL" to stop behaving like one — `<`/`>` open raw HTML, `` ` ``
/// and `|` open markdown constructs, `\` is the escape character, and
/// `"`/`^`/`{`/`}` are the remaining unwise delimiters.
const FORBIDDEN_IN_URL: &[char] = &['<', '>', '"', '`', '\\', '^', '{', '}', '|'];

/// Whether a URL is a plain `http(s)` URL — the only kind this system will emit
/// as a link target, navigate to, or badge with a domain. `javascript:`,
/// `data:`, and `file:` never qualify.
///
/// This is the **single** definition of that rule: [`safe_link`] and
/// [`crate::deepdive::display_domain`] both go through it, so a URL that may be
/// shown as a chip is exactly a URL that may be emitted as a link.
///
/// Two conditions, and the second matters as much as the first:
///
/// * the scheme is `http`/`https`, case-insensitively, and
/// * every character of the trimmed string is **ASCII-graphic** and not one of
///   [`FORBIDDEN_IN_URL`] — so no control characters, no whitespace, nothing
///   non-ASCII, and none of the delimiters RFC 3986 excludes from a URI anyway.
///
/// A URL is ASCII by construction (RFC 3986); anything else has to arrive
/// percent-encoded or punycoded, which is also the only honest rendering. The
/// characters this rejects are exactly the ones that let a fetched page's "URL"
/// stop being a URL: a newline in a link destination breaks the destination and
/// turns the tail into document structure, `<`/`>` open raw HTML, and a bidi
/// override in a path makes the link say something other than where it goes.
pub fn is_web_url(url: &str) -> bool {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return false;
    }
    trimmed
        .chars()
        .all(|c| c.is_ascii_graphic() && !FORBIDDEN_IN_URL.contains(&c))
}

/// A URL is only emitted as a link target if [`is_web_url`] accepts it — a
/// `javascript:` or `data:` "source", or an `https:` URL carrying a newline,
/// is rendered as inert text instead of an activatable link.
pub fn safe_link(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if !is_web_url(trimmed) {
        return None;
    }
    // Parentheses would close the markdown link target early.
    Some(trimmed.replace('(', "%28").replace(')', "%29"))
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
        assert!(
            !escaped.contains('\n'),
            "no line breaks survive: {escaped:?}"
        );
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
    fn leading_whitespace_cannot_smuggle_an_ordered_list_marker_past_the_guard() {
        // The trailing trim used to remove exactly the whitespace that had
        // cleared the "still in the leading digits" flag, so `  1. Buy milk`
        // came out with its marker intact and became a real ordered list.
        assert_eq!(escape("  1. Buy milk"), "1\\. Buy milk");
        assert_eq!(escape("\t2) Then this"), "2\\) Then this");
        // A character *folded* to a space is the same door.
        assert_eq!(escape("\u{202e}3. and this"), "3\\. and this");
    }

    #[test]
    fn the_format_characters_the_domain_already_refuses_are_folded_here_too() {
        // `char::is_control` is category Cc only, so these passed straight
        // through a function whose doc claims to fold "control and bidi"
        // characters. U+2028/U+2029 are line structure by another name;
        // U+061C is a bidi mark; U+2060/U+FEFF are zero-width.
        for hostile in [
            '\u{2028}', '\u{2029}', '\u{061c}', '\u{2060}', '\u{feff}', '\u{200b}', '\u{202e}',
        ] {
            let out = escape(&format!("a{hostile}b"));
            assert!(!out.contains(hostile), "{hostile:?} survived: {out:?}");
            assert_eq!(out, "a b", "{hostile:?} must fold to a space");
        }
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

    #[test]
    fn a_url_that_could_break_out_of_the_link_destination_is_never_emitted() {
        // The whole point of escaping the *text* is lost if the *destination*
        // can carry document structure. A newline inside a fetched page's URL
        // used to survive into `[label](…)`; CommonMark then refuses the
        // destination and the injected tail becomes a heading — or a real,
        // clickable anchor pointing anywhere the page likes.
        for hostile in [
            "https://a.example/x\n# Owned heading\n",
            "https://a.example/\n[Reset your password](https://evil.example)",
            "https://a.example/x\r\n> quote",
            "https://a.example/x\ttab",
            "https://a.example/spaced out",
            "https://a.example/<script>",
            "https://a.example/x\\y",
            "https://a.example/x>y",
            "https://a.example/\u{202e}gpj.exe",
            "https://a.example/x\u{0}",
        ] {
            assert_eq!(
                safe_link(hostile),
                None,
                "{hostile:?} must not become a link target"
            );
        }
    }

    #[test]
    fn the_http_scheme_rule_has_exactly_one_definition() {
        // `safe_link` and `is_web_url` used to spell the same rule twice; a
        // link target is emitted for exactly the URLs `is_web_url` accepts.
        for url in [
            "https://example.org/a",
            "HTTP://Example.org",
            "javascript:alert(1)",
            "data:text/html,x",
            "//example.org",
            "https://a.example/x\n# Owned",
            "",
        ] {
            assert_eq!(
                safe_link(url).is_some(),
                is_web_url(url),
                "{url:?}: safe_link and is_web_url must agree"
            );
        }
    }
}
