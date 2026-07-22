//! CF-3 tool-result sanitizer (docs/06 §5 tool-result smuggling). The domain
//! owns the single validator that neutralizes untrusted tool output before the
//! orchestrator folds it into a model prompt: control bytes stripped (except
//! `\n`/`\t`), length hard-capped on a UTF-8 boundary. These are the domain-level
//! properties; the orchestrator wiring is proven in the application layer.

use jarvis_domain::tools::{MAX_RESULT_PROMPT_BYTES, sanitize_result_content};

#[test]
fn plain_text_passes_through_unchanged_and_untruncated() {
    let out = sanitize_result_content("hello jarvis", MAX_RESULT_PROMPT_BYTES);
    assert_eq!(out.text, "hello jarvis");
    assert!(!out.truncated);
}

#[test]
fn newlines_and_tabs_are_preserved() {
    let out = sanitize_result_content("a\tb\nc", MAX_RESULT_PROMPT_BYTES);
    assert_eq!(out.text, "a\tb\nc");
    assert!(!out.truncated);
}

#[test]
fn control_characters_are_stripped() {
    // A terminal escape (ESC = 0x1b), a NUL, a bell, DEL, and a C1 control —
    // none may survive into a model prompt.
    let smuggled = "safe\u{1b}[31mred\u{0}\u{7}text\u{7f}\u{9b}end";
    let out = sanitize_result_content(smuggled, MAX_RESULT_PROMPT_BYTES);
    assert_eq!(out.text, "safe[31mredtextend");
    assert!(!out.truncated, "stripping controls is not truncation");
}

#[test]
fn stripping_alone_does_not_set_truncated() {
    let out = sanitize_result_content("\u{0}\u{0}\u{0}", 8);
    assert_eq!(out.text, "");
    assert!(!out.truncated);
}

#[test]
fn over_cap_content_is_truncated() {
    let big = "x".repeat(100);
    let out = sanitize_result_content(&big, 10);
    assert_eq!(out.text, "x".repeat(10));
    assert!(out.truncated);
}

#[test]
fn truncation_respects_utf8_char_boundaries() {
    // '€' is 3 bytes. With a 4-byte cap only one fits; the second must not be
    // split into an invalid partial code unit.
    let out = sanitize_result_content("€€€", 4);
    assert_eq!(out.text, "€");
    assert!(out.truncated);
    // The output is valid UTF-8 by construction (it is a String); assert the
    // byte length is a whole char, never a mid-code-unit cut.
    assert_eq!(out.text.len(), 3);
}

#[test]
fn a_char_that_exactly_fills_the_cap_is_kept() {
    let out = sanitize_result_content("€", 3);
    assert_eq!(out.text, "€");
    assert!(!out.truncated);
}

#[test]
fn strips_bidi_override_and_zero_width_format_chars() {
    // CF-13: a right-to-left override before a domain, plus zero-width joiners,
    // are stripped — the visible/spoofed form cannot survive into the prompt or
    // a HUD source-link chip. Ordinary text and \n/\t are untouched.
    let hostile = "see \u{202E}gro.elpmaxe\u{202C}\u{200B} at exam\u{200D}ple.org";
    let out = sanitize_result_content(hostile, MAX_RESULT_PROMPT_BYTES);
    assert!(
        !out.text.contains('\u{202E}'),
        "RLO not stripped: {:?}",
        out.text
    );
    assert!(!out.text.contains('\u{202C}'), "PDF not stripped");
    assert!(!out.text.contains('\u{200B}'), "ZWSP not stripped");
    assert!(!out.text.contains('\u{200D}'), "ZWJ not stripped");
    assert_eq!(out.text, "see gro.elpmaxe at example.org");
}
