//! **The mechanisable half of the docs/12 §9 HUD acceptance checklist** (F3b.9).
//!
//! §9 lists nine acceptance items for HUD work. Four of them are properties of
//! the *source tree* rather than of a running browser, and those are the ones
//! that rot silently — a new card type shipped without a renderer, a component
//! that reaches for `innerHTML` because a library wanted a string, an `<img>`
//! added straight to a template instead of through the attributed component, a
//! second state painted amber. Each is checked here, so they fail on a plain
//! `cargo test --workspace` on a machine with no browser at all.
//!
//! This lives in `xtask` for the same reason `arch-test` does: it is a
//! structural rule about the repository, checked with the repository's own dev
//! tooling, and xtask already depends on `jarvis-contracts` — which lets the
//! first test below compare the Angular card switch against the **actual**
//! contract union rather than against a hand-copied list.
//!
//! What this file deliberately does **not** claim: it is not a substitute for
//! the Angular suite. Keyboard walkthrough, reduced-motion behaviour, contrast
//! over the wallpapers and the panel lifecycle all need a real browser (see
//! `docs/milestones/M3b-acceptance.md` §3). A grep proves a sink is absent; it
//! cannot prove a surface is usable.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask lives at <root>/crates/xtask")
        .to_path_buf()
}

/// Every file under `dir` whose extension is in `extensions`, sorted.
fn files_under(dir: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", current.display()));
        for entry in entries {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| extensions.contains(&e))
            {
                found.push(path);
            }
        }
    }
    found.sort();
    assert!(
        !found.is_empty(),
        "no {extensions:?} files under {} — the scan would pass vacuously",
        dir.display()
    );
    found
}

/// Strip `//` line comments, `/* … */` block comments and `<!-- … -->` HTML
/// comments so a *prose explanation* of a forbidden pattern does not read as a
/// use of it. The HUD sources explain at length why they avoid markup sinks;
/// those explanations must not be what trips the check (nor be able to hide a
/// real use, which is why stripping is done rather than line-skipping).
///
/// A `//` preceded by `:` is a URL scheme, not a comment — otherwise a template
/// containing `https://…` would swallow the rest of its line and could hide a
/// sink behind a link. [`the_comment_stripper_cannot_swallow_a_sink`] pins that.
fn strip_comments(source: &str) -> String {
    let bytes: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < bytes.len() {
        let rest_is = |needle: &str| source_starts_at(&bytes, i, needle);
        if rest_is("//") && i > 0 && bytes[i - 1] == ':' {
            out.push(bytes[i]);
            i += 1;
        } else if rest_is("//") {
            while i < bytes.len() && bytes[i] != '\n' {
                i += 1;
            }
        } else if rest_is("/*") {
            i += 2;
            while i < bytes.len() && !source_starts_at(&bytes, i, "*/") {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else if rest_is("<!--") {
            i += 4;
            while i < bytes.len() && !source_starts_at(&bytes, i, "-->") {
                i += 1;
            }
            i = (i + 3).min(bytes.len());
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// The scans above are only as good as the stripper: it must hide prose and
/// nothing else. A stripper that ate too much would turn every grep in this
/// file into a silent pass, which is worse than having no grep.
#[test]
fn the_comment_stripper_cannot_swallow_a_sink() {
    // Prose is removed, in all three comment syntaxes…
    assert!(!strip_comments("// never use innerHTML here").contains("innerHTML"));
    assert!(!strip_comments("/* innerHTML is forbidden */").contains("innerHTML"));
    assert!(!strip_comments("<!-- no innerHTML -->").contains("innerHTML"));
    // …code is not…
    assert!(strip_comments("el.innerHTML = card.name;").contains("innerHTML"));
    // …and a URL does not start a comment, so nothing hides behind a link.
    assert!(
        strip_comments("<a href=\"https://x.example\">x</a> el.innerHTML = y;")
            .contains("innerHTML"),
        "a `//` inside a URL must not swallow the rest of the line"
    );
    // A comment that is never closed still ends the scan cleanly rather than
    // panicking on an out-of-range slice.
    assert_eq!(strip_comments("a /* unterminated"), "a ");
}

fn source_starts_at(chars: &[char], at: usize, needle: &str) -> bool {
    needle
        .chars()
        .enumerate()
        .all(|(offset, c)| chars.get(at + offset) == Some(&c))
}

/// Every registered card type, read from the **contract's own JSON Schema** —
/// the same export `cargo xtask codegen` writes — so this can never drift from
/// `HudCardDto`.
fn registered_card_types() -> BTreeSet<String> {
    let schema = jarvis_contracts::schema::export();
    let definitions = schema
        .get("definitions")
        .or_else(|| schema.get("$defs"))
        .and_then(|d| d.as_object())
        .expect("the exported schema has a definitions object");
    let card = definitions
        .get("HudCardDto")
        .expect("HudCardDto is exported");
    let variants = card
        .get("oneOf")
        .and_then(|v| v.as_array())
        .expect("HudCardDto is a tagged union");
    let types: BTreeSet<String> = variants
        .iter()
        .filter_map(|variant| {
            variant
                .get("properties")?
                .get("type")?
                .get("const")?
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(
        types.len(),
        variants.len(),
        "every card variant carries a constant `type` discriminator"
    );
    types
}

// ---------------------------------------------------------------------------
// §9: "card grammar only (no free-form model HTML)"
// ---------------------------------------------------------------------------

/// Every registered card type has a renderer, and the renderer switch admits
/// nothing else.
///
/// This is the structural form of "card grammar only": the set the client will
/// render is exactly the set the contract registers. Adding a variant to
/// `HudCardDto` without an Angular arm fails here rather than silently
/// degrading every instance of the new card to the error placeholder in
/// production.
#[test]
fn the_client_renders_exactly_the_registered_card_types() {
    let root = workspace_root();
    let switch = std::fs::read_to_string(root.join("web/src/app/hud/cards/hud-card.ts"))
        .expect("the card switch component exists");
    let code = strip_comments(&switch);

    let registered = registered_card_types();
    assert!(
        registered.len() >= 13,
        "the union should not have shrunk unnoticed: {registered:?}"
    );
    for card_type in &registered {
        assert!(
            code.contains(&format!("'{card_type}'")),
            "hud-card.ts has no arm for the registered card type `{card_type}` — a \
             card the server can send would degrade to the error placeholder \
             (docs/12 §2.3/§9)"
        );
    }

    // …and nothing the contract does not register. Any `'card.*'` literal in the
    // switch must be a registered type.
    for literal in code.split('\'').skip(1).step_by(2) {
        if literal.starts_with("card.") {
            assert!(
                registered.contains(literal),
                "hud-card.ts narrows on `{literal}`, which `HudCardDto` does not \
                 register — the client must not invent card types"
            );
        }
    }
}

/// No markup sink anywhere on the HUD face.
///
/// Invariant #1 and docs/12 §9: the model proposes card *content* through narrow
/// typed fields; it never proposes layout or HTML. Every renderer must therefore
/// treat every field as plain text. A single `[innerHTML]`, `bypassSecurityTrust*`
/// or `insertAdjacentHTML` on this surface would reopen the whole class of
/// model-authored-markup attacks, so the absence is asserted rather than
/// remembered — the same technique as
/// `jarvis_application::timers::tests::the_timer_path_never_reaches_a_model`.
#[test]
fn the_hud_face_contains_no_markup_sink() {
    let root = workspace_root();
    let hud = root.join("web/src/app/hud");
    // Sinks that turn a string into live DOM, plus the two escapes from Angular's
    // sanitizer. `.spec.ts` files are included on purpose: a test that builds a
    // sink to "prove" a component is safe would itself be the hazard.
    const SINKS: &[&str] = &[
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "bypassSecurityTrust",
        "document.write",
        "createContextualFragment",
        "new Function(",
    ];

    let mut scanned = 0usize;
    for path in files_under(&hud, &["ts", "html"]) {
        let source = std::fs::read_to_string(&path).expect("readable source");
        let code = strip_comments(&source);
        for sink in SINKS {
            assert!(
                !code.contains(sink),
                "{}: `{sink}` on the HUD face — card grammar only, no free-form \
                 model HTML (docs/12 §9, invariant #1)",
                path.strip_prefix(&root).unwrap_or(&path).display()
            );
        }
        scanned += 1;
    }
    assert!(scanned >= 20, "only {scanned} HUD sources scanned");
}

// ---------------------------------------------------------------------------
// §9: "every web-sourced image on a card shows its source link"
// ---------------------------------------------------------------------------

/// No HUD template paints an `<img>` except through the attributed component.
///
/// `SourcedImageDto` makes attribution unavoidable *on the wire*
/// (`jarvis-contracts`), and the producers cannot emit an unattributed image
/// (`jarvisd/tests/m3b_acceptance.rs`). This closes the last gap: a template
/// that dropped a bare `<img [src]="…">` in would render a web image with no
/// chip, whatever the wire type said.
///
/// Exactly two templates may own an `<img>`, and both are justified in the
/// contract's own doc comment:
/// * `sourced-image.html` — *the* attributed image component; the chip is part
///   of it, so an image cannot be rendered without one.
/// * `now-playing-card.html` — the player's own album art, not third-party web
///   content, so no source chip is owed (same treatment as the media bar).
#[test]
fn every_image_on_a_hud_card_goes_through_the_attributed_component() {
    let root = workspace_root();
    let allowed = [
        "web/src/app/hud/cards/sourced-image.html",
        "web/src/app/hud/cards/now-playing-card.html",
    ];

    let mut with_images = Vec::new();
    for path in files_under(&root.join("web/src/app/hud"), &["html"]) {
        let source = std::fs::read_to_string(&path).expect("readable template");
        if strip_comments(&source).contains("<img") {
            let relative = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            with_images.push(relative);
        }
    }
    with_images.sort();

    let mut expected: Vec<String> = allowed.iter().map(|s| (*s).to_owned()).collect();
    expected.sort();
    assert_eq!(
        with_images, expected,
        "a HUD template renders an <img> outside the attributed component — every \
         web-sourced image on a card shows its source link (docs/12 §9, FR-25/ADR-014)"
    );

    // And the attributed component genuinely renders the chip next to the image.
    let sourced = std::fs::read_to_string(root.join("web/src/app/hud/cards/sourced-image.html"))
        .expect("the attributed image component exists");
    assert!(
        sourced.contains("app-source-chip"),
        "sourced-image.html paints an image without mounting its source chip"
    );
}

// ---------------------------------------------------------------------------
// §9: "both wallpapers pass contrast audit"
// ---------------------------------------------------------------------------

/// WCAG 2.1 relative luminance of an `#rrggbb` colour.
fn relative_luminance(rgb: [u8; 3]) -> f64 {
    let channel = |raw: u8| {
        let c = f64::from(raw) / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
}

/// WCAG 2.1 contrast ratio, always ≥ 1.
fn contrast_ratio(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// "Source over" in straight alpha — what a `--glass-bg` panel does to the
/// wallpaper behind it.
fn composite(fg: [u8; 3], bg: [u8; 3], alpha: f64) -> [u8; 3] {
    let mix = |f: u8, b: u8| (f64::from(f) * alpha + f64::from(b) * (1.0 - alpha)).round() as u8;
    [mix(fg[0], bg[0]), mix(fg[1], bg[1]), mix(fg[2], bg[2])]
}

fn parse_hex(hex: &str) -> [u8; 3] {
    let value = hex.trim().trim_start_matches('#');
    assert_eq!(value.len(), 6, "expected #rrggbb, got {hex:?}");
    let byte = |at: usize| {
        u8::from_str_radix(&value[at..at + 2], 16)
            .unwrap_or_else(|e| panic!("{hex:?} is not a hex colour: {e}"))
    };
    [byte(0), byte(2), byte(4)]
}

/// Pull `needle` followed by a `#rrggbb` literal out of `source`. Parsing the
/// real file rather than restating the value is the point: if a token is renamed
/// or moved, this fails loudly instead of auditing a stale copy.
fn hex_after(source: &str, needle: &str, what: &str) -> [u8; 3] {
    let at = source.find(needle).unwrap_or_else(|| {
        panic!("{what}: `{needle}` not found — the audit cannot read its input")
    });
    let rest = &source[at + needle.len()..];
    let start = rest
        .find('#')
        .unwrap_or_else(|| panic!("{what}: no colour literal after `{needle}`"));
    parse_hex(&rest[start..start + 7])
}

/// Pull `needle` followed by a decimal number out of `source`.
fn number_after(source: &str, needle: &str, what: &str) -> f64 {
    let at = source
        .find(needle)
        .unwrap_or_else(|| panic!("{what}: `{needle}` not found"));
    let rest = &source[at + needle.len()..];
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits
        .parse()
        .unwrap_or_else(|e| panic!("{what}: `{needle}` is not followed by a number: {e}"))
}

/// **The glass-contrast audit, headless** (docs/12 §5/§9: *"both wallpapers pass
/// contrast audit"*).
///
/// The audit is arithmetic, not an eyeball judgement: composite the glass panel
/// over each bundled wallpaper's **worst-case pixel** and measure the ink
/// against WCAG AA. Passing on the extreme pixel means passing everywhere on
/// that wallpaper.
///
/// `contrast.spec.ts` does exactly this in the browser suite. It is duplicated
/// here on purpose and the duplication is safe, because **both sides read the
/// same source files** — this test parses the tokens out of `backgrounds.ts` and
/// `styles.scss` rather than restating them, so a token change moves both
/// audits together or breaks the parse. The payoff is that the §9 contrast item
/// is verifiable on a machine with no browser, which is where CI and most
/// development actually happen.
#[test]
fn both_bundled_wallpapers_pass_the_wcag_aa_contrast_audit() {
    const AA_BODY: f64 = 4.5;
    const AA_LARGE: f64 = 3.0;

    let root = workspace_root();
    let backgrounds = std::fs::read_to_string(root.join("web/src/app/hud/backgrounds.ts"))
        .expect("the background/glass token module exists");
    let styles = std::fs::read_to_string(root.join("web/src/styles.scss")).expect("styles.scss");

    let ink = hex_after(&styles, "--ink:", "styles.scss");
    let glass_white = parse_hex("#ffffff");

    // The two glass columns of the docs/12 §5 table.
    let plain_alpha = number_after(&backgrounds, "GLASS_PLAIN", "backgrounds.ts");
    let wallpaper_alpha = number_after(&backgrounds, "GLASS_WALLPAPER", "backgrounds.ts");
    let wallpaper_ink_dim = hex_after(
        backgrounds
            .split("GLASS_WALLPAPER")
            .nth(1)
            .expect("the wallpaper column exists"),
        "inkDim:",
        "backgrounds.ts GLASS_WALLPAPER",
    );
    let plain_ink_dim = hex_after(
        backgrounds
            .split("GLASS_PLAIN")
            .nth(1)
            .expect("the plain column exists"),
        "inkDim:",
        "backgrounds.ts GLASS_PLAIN",
    );
    assert!(
        wallpaper_alpha > plain_alpha,
        "a wallpaper must make the glass denser, not thinner (docs/12 §5)"
    );

    // The worst-case pixel of each bundled wallpaper, and the asset it came from.
    let extremes: Vec<(String, [u8; 3])> = backgrounds
        .split("asset: '")
        .skip(1)
        .map(|chunk| {
            let asset = chunk.split('\'').next().expect("an asset path").to_owned();
            (asset, hex_after(chunk, "extreme:", "a bundled wallpaper"))
        })
        .collect();
    assert_eq!(
        extremes.len(),
        2,
        "docs/12 §9 audits exactly two worst-case wallpapers, found {extremes:?}"
    );

    for (asset, extreme) in &extremes {
        // The asset must actually be shipped — an audited wallpaper nobody can
        // load is not an audited wallpaper.
        let path = root.join("web/public").join(asset);
        assert!(
            path.exists(),
            "audited wallpaper {asset} is not in web/public — nothing ships it"
        );

        let panel = composite(glass_white, *extreme, wallpaper_alpha);
        let body = contrast_ratio(ink, panel);
        let dim = contrast_ratio(wallpaper_ink_dim, panel);
        assert!(
            body >= AA_BODY,
            "body text over {asset} is {body:.2}:1, below WCAG AA {AA_BODY} (docs/12 §5/§9)"
        );
        assert!(
            dim >= AA_BODY,
            "secondary text over {asset} is {dim:.2}:1, below WCAG AA {AA_BODY}"
        );
        assert!(
            body >= AA_LARGE,
            "large caption over {asset} is {body:.2}:1, below WCAG AA large {AA_LARGE}"
        );
        println!("  contrast audit — {asset}: body {body:.2}:1, secondary {dim:.2}:1");
    }

    // The "no background" column sits on the page gradient; its lightest stop is
    // the worst case for dark ink.
    let plain_panel = composite(glass_white, parse_hex("#e9ebef"), plain_alpha);
    assert!(contrast_ratio(ink, plain_panel) >= AA_BODY);
    assert!(contrast_ratio(plain_ink_dim, plain_panel) >= AA_BODY);

    // Sanity anchors, so a broken luminance implementation cannot pass the audit
    // by accident.
    assert!((contrast_ratio(parse_hex("#000000"), parse_hex("#ffffff")) - 21.0).abs() < 0.05);
    assert!((contrast_ratio(ink, ink) - 1.0).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// §9: "amber-exclusivity grep"
// ---------------------------------------------------------------------------

/// Amber marks one thing on the HUD: a human decision is wanted.
///
/// docs/12 §2.1 — *"Amber exclusivity survives the HUD pivot: it appears only
/// when a human decision is wanted"* — and §9 asks for it as a grep. This is
/// that grep, as a test: the amber token may be referenced only where a decision
/// is actually owed.
///
/// Scope is the HUD face and its approval surface, which is what §9's checklist
/// covers. Each allowed file states, in its own comment, why it qualifies.
#[test]
fn amber_is_reserved_for_surfaces_that_want_a_human_decision() {
    let root = workspace_root();
    // The single hue registry (`PRESENCE_HUE`, waiting → --c-wait) and the two
    // surfaces that ask for a decision: an undecided approval, and a ringing
    // timer waiting to be dismissed.
    const ALLOWED: &[&str] = &[
        "web/src/app/hud/hud-state.service.ts",
        "web/src/app/hud/timers/timer-card.scss",
    ];

    let mut users = Vec::new();
    for path in files_under(&root.join("web/src/app/hud"), &["ts", "html", "scss"]) {
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        // Spec files describe the rule; they are the browser-side half of this
        // very check and are not a surface.
        if relative.ends_with(".spec.ts") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("readable source");
        if strip_comments(&source).contains("--c-wait") {
            users.push(relative);
        }
    }
    users.sort();

    let mut expected: Vec<String> = ALLOWED.iter().map(|s| (*s).to_owned()).collect();
    expected.sort();
    assert_eq!(
        users, expected,
        "amber (--c-wait) appears on a HUD surface that is not asking for a human \
         decision (docs/12 §2.1 amber exclusivity, §9)"
    );

    // The hue registry itself must bind amber to exactly one presence state.
    let hue_map = std::fs::read_to_string(root.join("web/src/app/hud/hud-state.service.ts"))
        .expect("the hue registry exists");
    let code = strip_comments(&hue_map);
    assert_eq!(
        code.matches("'--c-wait'").count(),
        1,
        "exactly one presence state may be amber, and it is `waiting`"
    );
    assert!(
        code.contains("waiting: '--c-wait'"),
        "amber must be bound to the `waiting` state, nothing else"
    );
}
