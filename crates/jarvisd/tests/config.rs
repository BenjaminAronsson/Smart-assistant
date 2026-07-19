//! F0.5: `jarvisd::config::Config` — layered config validation (docs/09 §1),
//! loopback-only bind enforcement (docs/06 §7), and secret-reference-only
//! secrets (CLAUDE.md invariant 5: "No secrets in prompts, logs, or CLI
//! args. Secrets are keyring references resolved at the adapter boundary.").
//!
//! Config layering order per docs/09 §1: `/etc/jarvis/jarvisd.toml` →
//! `~/.config/jarvis/jarvisd.toml` → environment (`JARVIS__…`) → keyring
//! references. `Config::from_figment` is the layer-agnostic core that
//! `Config::load` wires the real layers into; these tests drive it directly
//! with figments built in-process so they never touch the real filesystem
//! or environment layering (fixture-driven, docs/07 discipline).

use figment::Figment;
use figment::providers::Serialized;
use jarvisd::config::{Config, resolve_secret_ref};

fn empty_figment() -> Figment {
    Figment::new()
}

fn figment_with(json: serde_json::Value) -> Figment {
    Figment::from(Serialized::defaults(json))
}

// docs/09 §1: "Validated at startup; invalid config is fail-fast with a
// precise error." — the flip side is that an empty figment (no file, no env)
// must still produce a usable, documented default configuration.
#[test]
fn defaults_bind_loopback_8741() {
    let config = Config::from_figment(empty_figment()).expect("defaults must validate");
    assert_eq!(config.server.bind, "127.0.0.1:8741");
}

#[test]
fn defaults_database_max_connections_is_8() {
    let config = Config::from_figment(empty_figment()).expect("defaults must validate");
    assert_eq!(config.database.max_connections, 8);
}

#[test]
fn defaults_database_url_secret_is_env_jarvis_db_url() {
    let config = Config::from_figment(empty_figment()).expect("defaults must validate");
    assert_eq!(config.database.url_secret, "env:JARVIS_DB_URL");
}

#[test]
fn defaults_observability_otlp_endpoint_is_none() {
    let config = Config::from_figment(empty_figment()).expect("defaults must validate");
    assert_eq!(config.observability.otlp_endpoint, None);
}

// docs/06 §7: "Network: bind loopback for M0–M2." — a non-loopback bind must
// be rejected at startup, not silently accepted and only fail later when a
// LAN client connects.
#[test]
fn non_loopback_bind_is_rejected() {
    let figment = figment_with(serde_json::json!({
        "server": { "bind": "0.0.0.0:8741" }
    }));
    let err = Config::from_figment(figment).expect_err("non-loopback bind must be rejected");
    let message = err.to_string();
    assert!(
        message.to_lowercase().contains("loopback"),
        "error message must mention loopback, got: {message}"
    );
}

// A second non-loopback address (a real LAN/public IP, not just
// "all interfaces") must also be rejected — the check is "is loopback",
// not "is not 0.0.0.0".
#[test]
fn non_loopback_specific_address_is_rejected() {
    let figment = figment_with(serde_json::json!({
        "server": { "bind": "192.168.1.50:8741" }
    }));
    let err = Config::from_figment(figment).expect_err("LAN bind must be rejected");
    assert!(err.to_string().to_lowercase().contains("loopback"));
}

// Malformed input: a bind value that cannot even parse as a socket address
// must fail validation (fail-fast, not panic, not silently default).
#[test]
fn unparseable_bind_address_is_rejected() {
    let figment = figment_with(serde_json::json!({
        "server": { "bind": "not-an-addr" }
    }));
    assert!(Config::from_figment(figment).is_err());
}

// invariant 5: a literal connection string (with an embedded password) in
// config must never be accepted — only a reference to where the secret
// lives is allowed on disk/in env.
#[test]
fn literal_database_url_is_rejected() {
    let figment = figment_with(serde_json::json!({
        "database": { "url_secret": "postgres://user:pw@host/db" }
    }));
    let err = Config::from_figment(figment).expect_err("literal secret value must be rejected");
    let message = err.to_string();
    assert!(
        message.contains("reference"),
        "error message must explain that secrets must be references, got: {message}"
    );
    assert!(
        message.contains("env:") && message.contains("keyring:"),
        "error message must name the accepted reference schemes, got: {message}"
    );
}

#[test]
fn env_secret_reference_is_accepted() {
    let figment = figment_with(serde_json::json!({
        "database": { "url_secret": "env:MY_VAR" }
    }));
    let config = Config::from_figment(figment).expect("env: reference must be accepted");
    assert_eq!(config.database.url_secret, "env:MY_VAR");
}

#[test]
fn keyring_secret_reference_is_accepted() {
    let figment = figment_with(serde_json::json!({
        "database": { "url_secret": "keyring:jarvis/db-url" }
    }));
    let config = Config::from_figment(figment).expect("keyring: reference must be accepted");
    assert_eq!(config.database.url_secret, "keyring:jarvis/db-url");
}

// --- resolve_secret_ref -----------------------------------------------

// Happy path: an `env:` reference to a variable that is actually set
// resolves to a Redacted value exposing the original secret. Lookup is
// injected (`resolve_secret_ref_with`) so no test mutates process-global
// env — `std::env::set_var` is `unsafe` in Rust 2024 and stays banned.
#[test]
fn resolve_secret_ref_env_set_exposes_value() {
    let lookup = |var: &str| (var == "JARVIS_TEST_SECRET").then(|| "s3cr3t-value".to_string());
    let resolved = jarvisd::config::resolve_secret_ref_with("env:JARVIS_TEST_SECRET", lookup)
        .expect("set env var must resolve");
    assert_eq!(resolved.expose(), "s3cr3t-value");
}

// Malformed/missing input: an `env:` reference to a variable that is not set
// must fail rather than silently resolving to an empty string.
#[test]
fn resolve_secret_ref_env_unset_is_err() {
    let result = jarvisd::config::resolve_secret_ref_with("env:JARVIS_TEST_UNSET", |_| None);
    assert!(result.is_err());
    // And the real env-backed path agrees for a variable that cannot exist.
    assert!(resolve_secret_ref("env:JARVIS_TEST_SECRET_ENV_NOT_SET_XYZZY").is_err());
}

// `keyring:` references are recognized as valid config (see
// keyring_secret_reference_is_accepted above) but resolution is not yet
// implemented — this must be a clear, actionable error, not a panic or a
// silent stub value.
#[test]
fn resolve_secret_ref_keyring_is_not_yet_available() {
    let err = resolve_secret_ref("keyring:jarvis/x")
        .expect_err("keyring resolution is not implemented yet");
    let message = err.to_string();
    assert!(
        message.contains("keyring") && message.to_lowercase().contains("not"),
        "error must explain keyring resolution isn't available yet, got: {message}"
    );
    assert!(
        message.contains("env:"),
        "error must point developers at the env: workaround, got: {message}"
    );
}

// Malformed input: neither an `env:` nor a `keyring:` prefix — must be
// rejected rather than treated as a literal value.
#[test]
fn resolve_secret_ref_bogus_scheme_is_err() {
    assert!(resolve_secret_ref("bogus").is_err());
}
