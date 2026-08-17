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

// keyring 3 has no platform store enabled by default: on Linux it silently
// falls back to an in-memory mock. A config parser test cannot catch that,
// because `keyring:` is valid syntax under either backend. Assert the concrete
// credential selected for jarvisd instead, without touching a real secret or
// requiring a D-Bus session in CI.
#[cfg(target_os = "linux")]
#[test]
fn jarvisd_uses_the_linux_secret_service_not_the_mock_keyring() {
    let entry = keyring::Entry::new("jarvis-backend-test", "unused")
        .expect("constructing a Secret Service credential does not access the service");

    assert!(
        entry
            .get_credential()
            .is::<keyring::secret_service::SsCredential>(),
        "jarvisd must never compile keyring references against the in-memory mock backend"
    );
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

#[tokio::test(flavor = "current_thread")]
async fn async_secret_resolution_preserves_fail_closed_errors() {
    // Production uses this path for both env and keyring references. The env
    // case is deterministic in CI and proves the awaited spawn-blocking seam
    // completes without mutating process-global state and preserves failures.
    let resolved =
        jarvisd::config::resolve_secret_ref_async("env:JARVIS_TEST_SECRET_ENV_NOT_SET_XYZZY")
            .await
            .expect_err("the deliberately absent variable must still fail closed");

    assert!(
        resolved.to_string().contains("is not set"),
        "the async seam must preserve the resolver's actionable error"
    );
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

// A valid-shaped keyring reference fails closed when this test environment has
// no provisioned entry; no secret or backend detail may escape.
#[test]
fn resolve_secret_ref_keyring_unavailable_is_generic() {
    let err =
        resolve_secret_ref("keyring:jarvis/x").expect_err("test keyring entry is not provisioned");
    let message = err.to_string();
    assert!(
        message.contains("keyring") || message.contains("could not"),
        "error must stay generic, got: {message}"
    );
}

// Malformed input: neither an `env:` nor a `keyring:` prefix — must be
// rejected rather than treated as a literal value.
#[test]
fn resolve_secret_ref_bogus_scheme_is_err() {
    assert!(resolve_secret_ref("bogus").is_err());
}

// --- M5 integrations: Home Assistant and Spotify ----------------------
//
// Both are opt-in and both hold real-world authority — HA over *physical*
// devices, Spotify over the owner's account — so the defaults and the
// enabled-path validation are part of the security surface, not ergonomics.

// Disabled by default: turning nothing on must not grant home authority.
#[test]
fn m5_integrations_are_disabled_by_default() {
    let config = Config::from_figment(empty_figment()).expect("defaults must validate");
    assert!(!config.integrations.home_assistant.enabled);
    assert!(!config.integrations.spotify.enabled);
}

// An enabled HA section with empty allowlists controls nothing. This is the
// fail-closed property: authority comes from explicitly listing entities,
// never from flipping `enabled`.
#[test]
fn enabling_home_assistant_alone_allowlists_no_entity() {
    let figment = figment_with(serde_json::json!({
        "integrations": { "home_assistant": {
            "enabled": true,
            "base_url": "https://ha.example.test:8123",
        }}
    }));
    let config = Config::from_figment(figment).expect("https + default secret ref validates");
    let ha = &config.integrations.home_assistant;
    assert!(ha.readable.is_empty() && ha.lights.is_empty());
    assert!(ha.scenes.is_empty() && ha.scripts.is_empty());
}

// docs/06 §7: a long-lived bearer token rides on every HA request, so plain
// http is refused outright rather than warned about — the common LAN setup
// needs TLS in front of HA, which is a deployment decision, not a default.
#[test]
fn plain_http_home_assistant_is_refused() {
    let figment = figment_with(serde_json::json!({
        "integrations": { "home_assistant": {
            "enabled": true,
            "base_url": "http://ha.example.test:8123",
        }}
    }));
    let message = Config::from_figment(figment)
        .expect_err("http:// must be rejected when HA is enabled")
        .to_string();
    assert!(
        message.contains("https://"),
        "error must name the required scheme, got: {message}"
    );
}

// Invariant 5 at the config boundary: the HA token and the Spotify refresh
// token are references, never literals. The rejection must not echo the
// pasted value.
#[test]
fn m5_literal_credentials_are_rejected_without_echoing_them() {
    let cases = [
        (
            "home_assistant",
            serde_json::json!({
                "enabled": true,
                "base_url": "https://ha.example.test:8123",
                "token_secret": "eyJhbGciOiJIUzI1NiJ9.SECRETVALUE",
            }),
        ),
        (
            "spotify",
            serde_json::json!({
                "enabled": true,
                "client_id": "client-abc",
                "refresh_token_secret": "AQC-SECRETVALUE",
            }),
        ),
    ];
    for (section, section_body) in cases {
        let figment = figment_with(serde_json::json!({
            "integrations": { section: section_body }
        }));
        let message = Config::from_figment(figment)
            .expect_err("a literal credential must be rejected")
            .to_string();
        assert!(
            !message.contains("SECRETVALUE"),
            "the rejected credential must never appear in the error, got: {message}"
        );
        assert!(
            message.contains("env:") && message.contains("keyring:"),
            "error must name the accepted schemes, got: {message}"
        );
    }
}

// M5 audit S3 residual: a *colon-containing* literal secret. The earlier fix
// covered "no colon at all"; a pasted password like `Summer:2026!` still has a
// first-colon prefix, and echoing it leaked a usable fragment of the secret into
// stderr/journald. Note that shape is no defence here — `Summer` is a perfectly
// well-formed URI scheme — so only a scheme name the code itself knows may be
// echoed (invariant 5).
#[test]
fn a_colon_containing_literal_password_is_rejected_without_echoing_any_fragment() {
    let cases = [
        // A password that happens to contain a colon, whose prefix is
        // indistinguishable from a scheme by shape alone.
        ("Summer:2026!", ["Summer", "2026"]),
        // A prefix that is not even scheme-shaped.
        ("9Hunter2:rest-of-it", ["9Hunter2", "rest-of-it"]),
        // A long opaque token that merely contains a colon somewhere.
        (
            "AQCxxxxxxxxxxxxxxxxxxxxxxxx:tail",
            ["AQCxxxxxxxxxxxxxxxxxxxxxxxx", "tail"],
        ),
    ];
    for (literal, forbidden) in cases {
        let figment = figment_with(serde_json::json!({
            "integrations": { "home_assistant": {
                "enabled": true,
                "base_url": "https://ha.example.test:8123",
                "token_secret": literal,
            }}
        }));
        let message = Config::from_figment(figment)
            .expect_err("a literal credential must be rejected")
            .to_string();
        for fragment in forbidden {
            assert!(
                !message.contains(fragment),
                "no fragment of the pasted secret may appear in the error; \
                 {fragment:?} leaked from {literal:?}: {message}"
            );
        }
        assert!(
            message.contains("<withheld>"),
            "the withheld marker must stand in for the value, got: {message}"
        );
        assert!(
            message.contains("env:") && message.contains("keyring:"),
            "error must still name the accepted schemes, got: {message}"
        );
    }
}

// The counterpart: a genuine mistyped *scheme* is still echoed, because that is
// the operator-actionable half of the message and a scheme is not a secret.
#[test]
fn a_genuine_but_unsupported_scheme_is_still_named() {
    let figment = figment_with(serde_json::json!({
        "database": { "url_secret": "vault:jarvis/db-url" }
    }));
    let message = Config::from_figment(figment)
        .expect_err("an unsupported scheme must be rejected")
        .to_string();
    assert!(
        message.contains("vault"),
        "a real scheme is safe to echo and tells the operator what to fix, got: {message}"
    );
}

// The volume cap is the R1/R2 boundary for hearing protection (docs/02 §11a),
// so a nonsensical cap is a startup failure, not a value clamped at runtime.
#[test]
fn spotify_volume_cap_must_be_a_real_percentage() {
    for cap in [0u8, 101] {
        let figment = figment_with(serde_json::json!({
            "integrations": { "spotify": {
                "enabled": true,
                "client_id": "client-abc",
                "max_volume_pct": cap,
            }}
        }));
        assert!(
            Config::from_figment(figment).is_err(),
            "max_volume_pct {cap} must be rejected"
        );
    }
}
