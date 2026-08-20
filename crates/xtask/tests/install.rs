//! Packaging and install regressions (F10.9).
//!
//! These assert on the *shipped files* rather than on behaviour, because the
//! failures they guard are configuration failures that appear only on a host
//! nobody has yet — and each one has already happened or was one boot away
//! from happening.

use std::path::Path;

fn repo_root() -> &'static Path {
    // CARGO_MANIFEST_DIR is crates/xtask; the root is two levels up.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/xtask has a grandparent")
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {path:?}: {error}"))
}

/// The blocker this feature was nearly shipped with.
///
/// `jarvisd.service` is a SYSTEM unit: `User=jarvis`, `ProtectHome=true`, no
/// login session, no D-Bus session bus, no unlocked Secret Service collection.
/// A `keyring:` database reference therefore cannot resolve, and an
/// unresolvable secret reference is a fatal config error by design
/// (crates/jarvisd/src/config.rs). The daemon would fail fast on every boot of
/// the machine this feature exists to set up.
#[test]
fn shipped_config_resolves_the_database_url_from_the_environment() {
    let example = read("infra/jarvisd.toml.example");
    let database_section = example
        .split_once("[database]")
        .expect("the example config has a [database] section")
        .1;
    let url_secret = database_section
        .lines()
        .find(|line| line.trim_start().starts_with("url_secret"))
        .expect("[database] sets url_secret");

    assert!(
        url_secret.contains("env:JARVIS_DB_URL"),
        "the packaged database reference must be env:JARVIS_DB_URL — a keyring \
         reference cannot resolve in a system service with no session bus. Got: {url_secret}"
    );
}
