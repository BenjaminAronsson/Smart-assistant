//! F0.5: `jarvis_domain::secrecy::Redacted<T>` — structural secret containment
//! (CLAUDE.md invariant 5: "No secrets in prompts, logs, or CLI args ... the
//! tracing layer redacts known secret fields.").
//!
//! `Redacted<T>` must make it *structurally* impossible for a secret to reach
//! a log line or trace via `{:?}`/`{}` formatting, regardless of what `T` is,
//! and regardless of how deeply it is nested inside another `Debug`-deriving
//! type (e.g. a config struct that gets logged at startup).

use jarvis_domain::secrecy::Redacted;

// invariant 5: Debug never shows the inner value, for a plain String secret.
#[test]
fn debug_never_shows_inner_string_value() {
    let r = Redacted::new("hunter2".to_string());
    let debug = format!("{r:?}");
    assert_eq!(debug, "[REDACTED]");
    assert!(
        !debug.contains("hunter2"),
        "debug output must never contain the raw secret"
    );
}

// invariant 5: Display never shows the inner value, for a plain String secret.
#[test]
fn display_never_shows_inner_string_value() {
    let r = Redacted::new("hunter2".to_string());
    let display = format!("{r}");
    assert_eq!(display, "[REDACTED]");
    assert!(
        !display.contains("hunter2"),
        "display output must never contain the raw secret"
    );
}

/// A non-trivial payload type — Redacted<T>'s Debug/Display impls carry no
/// bound on `T`, so this must compile and redact even though `Payload` does
/// not implement `Display` (only `Debug`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Payload {
    token: String,
    scope: Vec<String>,
}

// invariant 5: Debug never shows the inner value, whatever T is — here T is a
// struct deriving Debug, not a String.
#[test]
fn debug_never_shows_inner_struct_value() {
    let payload = Payload {
        token: "sk-supersecret".to_string(),
        scope: vec!["read".to_string(), "write".to_string()],
    };
    let r = Redacted::new(payload);
    let debug = format!("{r:?}");
    assert_eq!(debug, "[REDACTED]");
    assert!(!debug.contains("sk-supersecret"));
    assert!(!debug.contains("scope"));
}

// invariant 5: Display never shows the inner value either, for the struct
// case (Redacted<T>'s Display impl has no `T: Display` bound at all).
#[test]
fn display_never_shows_inner_struct_value() {
    let payload = Payload {
        token: "sk-supersecret".to_string(),
        scope: vec!["read".to_string()],
    };
    let r = Redacted::new(payload);
    let display = format!("{r}");
    assert_eq!(display, "[REDACTED]");
    assert!(!display.contains("sk-supersecret"));
}

// `.expose()` is the single deliberate escape hatch — it must return the
// original value unchanged, so adapters can actually use the secret.
#[test]
fn expose_returns_original_string_value() {
    let r = Redacted::new("hunter2".to_string());
    assert_eq!(r.expose(), "hunter2");
    assert_eq!(r.expose().as_str(), "hunter2");
}

#[test]
fn expose_returns_original_struct_value() {
    let payload = Payload {
        token: "sk-supersecret".to_string(),
        scope: vec!["read".to_string()],
    };
    let r = Redacted::new(payload.clone());
    assert_eq!(r.expose(), &payload);
}

/// A config-shaped struct that itself derives `Debug` (as `jarvisd::config`
/// structs do) and holds a `Redacted<String>` field alongside a plain field.
/// This is the structural guarantee that actually matters in production:
/// logging the *containing* struct (e.g. via `tracing::debug!("{:?}", cfg)`)
/// must not leak the secret field, while unrelated fields still print
/// normally so the log stays useful.
#[derive(Debug)]
#[allow(dead_code)] // exercised via Debug formatting only
struct ConfigLike {
    database_url: Redacted<String>,
    max_connections: u32,
}

#[test]
fn nested_redacted_field_prints_as_redacted_in_deriving_struct() {
    let cfg = ConfigLike {
        database_url: Redacted::new("postgres://user:pw@host/db".to_string()),
        max_connections: 8,
    };
    let debug = format!("{cfg:?}");

    assert!(
        debug.contains("[REDACTED]"),
        "expected the redacted field to print as [REDACTED], got: {debug}"
    );
    assert!(
        !debug.contains("pw@host"),
        "raw secret leaked into containing struct's Debug output: {debug}"
    );
    assert!(
        debug.contains("max_connections"),
        "non-secret fields must still be visible for useful diagnostics: {debug}"
    );
    assert!(
        debug.contains('8'),
        "non-secret field value must still be visible: {debug}"
    );
}
