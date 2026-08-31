use super::*;

// ---- half-configured states a first install actually reaches (F8.9) ----

/// Builds a config from TOML the way an operator's file would load.
fn load(toml: &str) -> anyhow::Result<Config> {
    Config::from_figment(Figment::new().merge(Toml::string(toml)))
}

/// The file every installed host starts from must load through the real
/// type — `deny_unknown_fields`, defaults, `validate()` and all — not
/// merely look plausible to a substring check.
///
/// The `[server]` assertions are the F10.9 blocker in miniature:
/// install.sh set `web_assets` by APPENDING a second `[server]` header,
/// which TOML forbids, so the config it produced could not be parsed at
/// all and `jarvisd migrate` died on every fresh install. The fix is a
/// commented anchor line inside the one `[server]` table for install.sh to
/// rewrite in place; these two assertions are what keep that anchor there.
/// (What the installer actually writes is checked end-to-end in
/// crates/xtask/tests/install.rs — this test guards the input to it.)
///
/// The bind/TLS assertion is weaker than it looks and deliberately so:
/// `validate()` accepts `0.0.0.0` + `[server.tls]` naming files that do
/// not exist, because paths are checked when the listener loads them, not
/// here. Whether anything generates that certificate is therefore asserted
/// where it can be observed — against the staged install, in install.rs.
#[test]
fn the_packaged_example_config_loads_and_validates() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../infra/jarvisd.toml.example")
        .canonicalize()
        .expect("infra/jarvisd.toml.example exists");
    let text = std::fs::read_to_string(&path).expect("the example config is readable");

    let config = load(&text).expect("the packaged example config must load and validate");

    // install.sh sets this by rewriting the commented line in place. If
    // the anchor disappears the installer die()s, but say so here too:
    // this is the line whose absence used to be "fixed" by appending a
    // second [server] header.
    assert!(
        text.lines()
            .any(|line| line.trim_start().starts_with("# web_assets")),
        "the example must keep a commented web_assets line inside [server] \
             for install.sh to rewrite — appending a second [server] table \
             instead is invalid TOML"
    );
    assert_eq!(
        text.matches("\n[server]").count(),
        1,
        "[server] may be declared exactly once"
    );

    // The packaged pair is loopback + plaintext: nothing generates a
    // certificate during a fresh install, and validate() would refuse a
    // non-loopback bind without one anyway.
    assert!(
        config.bind_addr().ip().is_loopback() || config.server.tls.is_some(),
        "the packaged bind {} needs TLS and the example does not configure it",
        config.server.bind
    );
}

#[test]
fn elevenlabs_without_a_voice_pipeline_is_refused() {
    let error = load(
        r#"
            [voice]
            enabled = false
            [voice.elevenlabs]
            enabled = true
            api_key_ref = "keyring:jarvis/elevenlabs"
            voice_id = "abc"
            "#,
    )
    .expect_err("must refuse");
    assert!(
        error.to_string().contains("no voice pipeline"),
        "unexpected: {error}"
    );
}

/// The one that matters most: enabling a cloud voice with no local voice
/// underneath would make an internet outage a mute house, and would let a
/// failed alarm be silent (ADR-023, ADR-033 §3).
#[test]
fn elevenlabs_without_a_local_voice_to_fall_back_to_is_refused() {
    let error = load(
        r#"
            [voice]
            enabled = true
            [voice.elevenlabs]
            enabled = true
            api_key_ref = "keyring:jarvis/elevenlabs"
            voice_id = "abc"
            "#,
    )
    .expect_err("must refuse");
    assert!(
        error.to_string().contains("fall back"),
        "unexpected: {error}"
    );
}

#[test]
fn a_literal_elevenlabs_key_in_config_is_refused() {
    let error = load(
        r#"
            [voice]
            enabled = true
            wyoming_tts = "tcp://127.0.0.1:10200"
            [voice.elevenlabs]
            enabled = true
            api_key_ref = "sk_a_real_looking_key"
            voice_id = "abc"
            "#,
    )
    .expect_err("a secret must never be a literal in config (invariant 5)");
    // The message must not echo the value back into the operator's terminal.
    assert!(!error.to_string().contains("sk_a_real_looking_key"));
}

#[test]
fn elevenlabs_missing_its_voice_id_or_budget_is_refused() {
    let base = |extra: &str| {
        format!(
            r#"
                [voice]
                enabled = true
                wyoming_tts = "tcp://127.0.0.1:10200"
                [voice.elevenlabs]
                enabled = true
                api_key_ref = "keyring:jarvis/elevenlabs"
                {extra}
                "#
        )
    };
    assert!(load(&base("")).is_err(), "no voice_id");
    assert!(
        load(&base("voice_id = \"abc\"\ncharacter_budget = 0"))
            .expect_err("zero budget")
            .to_string()
            .contains("greater than zero")
    );
    // …and the fully configured version is accepted.
    assert!(load(&base("voice_id = \"abc\"")).is_ok());
}

/// A disabled block is never validated: an operator leaving a half-filled
/// `[voice.elevenlabs]` behind with `enabled = false` must still be able to
/// start the daemon.
#[test]
fn a_disabled_elevenlabs_block_is_not_validated() {
    assert!(
        load(
            r#"
                [voice.elevenlabs]
                enabled = false
                voice_id = ""
                character_budget = 0
                "#,
        )
        .is_ok()
    );
}

#[test]
fn defaults_carry_the_documented_claude_cli_config() {
    let config = Config::from_figment(Figment::new()).expect("defaults are valid");
    let cli = config.providers.claude_cli;
    assert_eq!(cli.binary, "claude");
    assert_eq!(cli.workdir, PathBuf::from("/var/lib/jarvis/claude-work"));
    assert!(cli.reasoning_disable_builtin_tools);
    assert_eq!(cli.idle_timeout_secs, 60);
}

#[test]
fn the_ui_section_defaults_to_the_documented_values() {
    // docs/09 §1 `[ui]`.
    let config = Config::from_figment(Figment::new()).expect("defaults are valid");
    assert_eq!(config.ui.background, "none");
    assert_eq!(config.ui.panel_ttl_hours, 2);
    assert_eq!(config.ui.deepdive_promote_after, 3);
    assert_eq!(config.ui.motion, "auto");
}

#[test]
fn the_documented_ui_block_is_accepted_verbatim() {
    // `Config` denies unknown fields, so an operator pasting the block from
    // docs/09 §1 must parse — every documented key is modelled.
    let figment = Figment::new().merge(Toml::string(
        r#"
            [ui]
            background = "photo"
            background_photo = "/var/lib/jarvis/wall.jpg"
            panel_ttl_hours = 4
            deepdive_promote_after = 0
            motion = "reduced"
            "#,
    ));
    let config = Config::from_figment(figment).expect("the documented block parses");
    assert_eq!(config.ui.background, "photo");
    // Zero is the documented "never offer" setting, not an invalid value.
    assert_eq!(config.ui.deepdive_promote_after, 0);
    assert_eq!(config.ui.panel_ttl_hours, 4);
}

#[test]
fn maps_are_off_by_default_and_an_empty_path_stays_off() {
    // The safe default: no archive ⇒ no map endpoints at all (F3b.5).
    let config = Config::from_figment(Figment::new()).expect("defaults are valid");
    assert!(config.maps.archive_path().is_none());
    assert!(config.maps.attribution_override().is_none());

    // docs/09 §1 documents `pmtiles_path = ""` as the off state — it must not
    // read as a path to the working directory.
    let config =
        Config::from_figment(Figment::new().merge(Toml::string("[maps]\npmtiles_path = \"\"\n")))
            .expect("an empty path is valid config");
    assert!(config.maps.archive_path().is_none());
}

#[test]
fn a_relative_map_archive_path_is_rejected_at_startup() {
    // A relative path would resolve against whatever directory the service
    // started in — fail fast rather than serve a different file later.
    let error = Config::from_figment(Figment::new().merge(Toml::string(
        "[maps]\npmtiles_path = \"maps/region.pmtiles\"\n",
    )))
    .expect_err("a relative archive path must be refused");
    assert!(
        error.to_string().contains("absolute"),
        "unexpected error: {error}"
    );

    let config = Config::from_figment(Figment::new().merge(Toml::string(
        "[maps]\npmtiles_path = \"/var/lib/jarvis/maps/region.pmtiles\"\nattribution = \"  \"\n",
    )))
    .expect("an absolute archive path is valid");
    assert_eq!(
        config.maps.archive_path(),
        Some(std::path::Path::new("/var/lib/jarvis/maps/region.pmtiles"))
    );
    // A blank override is no override — the archive/default attribution wins.
    assert!(config.maps.attribution_override().is_none());
}

#[test]
fn kebab_section_overrides_and_tolerates_unwired_f17_keys() {
    // `[providers.claude-cli]` is kebab-cased in TOML (docs/09 §1); the
    // still-unwired F1.7 keys (`timeout_secs`, `single_flight`, `backoff_*`)
    // must not fail the parse.
    let toml = r#"
            [providers.claude-cli]
            binary = "claude-test"
            workdir = "/tmp/jarvis-work"
            reasoning_disable_builtin_tools = false
            idle_timeout_secs = 90
            timeout_secs = 300
            single_flight = true
            backoff_initial_secs = 30
        "#;
    let config = Config::from_figment(Figment::new().merge(Toml::string(toml)))
        .expect("documented block parses");
    let adapter = config.providers.claude_cli.to_adapter();
    assert_eq!(adapter.binary, "claude-test");
    assert_eq!(adapter.workdir, PathBuf::from("/tmp/jarvis-work"));
    assert!(!adapter.disable_builtin_tools);
    assert_eq!(adapter.idle_timeout, std::time::Duration::from_secs(90));
}

/// **F7.3, the rule with no override (docs/06 §7).** A daemon that serves
/// device tokens in the clear on a LAN is the one configuration mistake
/// with no recovery — the credential is gone the moment it is used — so
/// the refusal happens at startup, not at first request.
#[test]
fn a_non_loopback_bind_without_tls_refuses_to_start() {
    let figment = Figment::new().merge(figment::providers::Serialized::defaults(
        serde_json::json!({ "server": { "bind": "0.0.0.0:8080" } }),
    ));
    let error = Config::from_figment(figment)
        .expect_err("a public bind without TLS must not start")
        .to_string();
    assert!(
        error.contains("server.tls"),
        "the error must name the fix: {error}"
    );

    // The same bind IS allowed once TLS is configured.
    let figment = Figment::new().merge(figment::providers::Serialized::defaults(
        serde_json::json!({
            "server": {
                "bind": "0.0.0.0:8080",
                "tls": { "cert_path": "/etc/jarvis/cert.pem", "key_path": "/etc/jarvis/key.pem" }
            }
        }),
    ));
    Config::from_figment(figment).expect("a TLS-configured public bind is legal");
}

#[test]
fn loopback_still_needs_no_tls_and_tls_paths_must_be_absolute() {
    let figment = Figment::new().merge(figment::providers::Serialized::defaults(
        serde_json::json!({ "server": { "bind": "127.0.0.1:8080" } }),
    ));
    Config::from_figment(figment).expect("loopback plaintext is the M0–M6 shape");

    for bad in ["cert.pem", "./certs/cert.pem"] {
        let figment = Figment::new().merge(figment::providers::Serialized::defaults(
            serde_json::json!({
                "server": {
                    "bind": "127.0.0.1:8080",
                    "tls": { "cert_path": bad, "key_path": "/etc/jarvis/key.pem" }
                }
            }),
        ));
        let error = Config::from_figment(figment)
            .expect_err("a relative TLS path must be refused")
            .to_string();
        assert!(error.contains("absolute"), "{error}");
    }
}
