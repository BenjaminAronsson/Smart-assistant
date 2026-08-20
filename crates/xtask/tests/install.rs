//! Packaging and install regressions (F10.9).
//!
//! These assert on the *shipped files* rather than on behaviour, because the
//! failures they guard are configuration failures that appear only on a host
//! nobody has yet — and each one has already happened or was one boot away
//! from happening.

use std::collections::BTreeMap;
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

/// Pulls `(repository, tag)` out of every `image:` line in a compose file.
///
/// This is a line-oriented scrape, not a YAML parser — deliberately, since
/// this crate takes on no new dependency to read three small files. It
/// splits on the *last* `:` so a registry-with-port reference (unused here,
/// but valid compose syntax) still separates into the right repository and
/// tag.
fn extract_images(content: &str) -> Vec<(String, String)> {
    content
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("image:"))
        .map(|rest| rest.split('#').next().unwrap_or(rest).trim())
        .filter_map(|image_ref| image_ref.rsplit_once(':'))
        .map(|(repository, tag)| (repository.trim().to_string(), tag.trim().to_string()))
        .collect()
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

/// A live bug, not a hypothetical: jarvisd.service orders itself after
/// `postgresql.service`, which does not exist on a host whose Postgres is a
/// container. systemd silently ignores an ordering dependency on an absent
/// unit, so the daemon starts before the database and fail-fasts every boot.
///
/// This parses the `After=` directive itself rather than grepping the whole
/// file: a whole-file substring check stays green even if a later edit trims
/// `jarvis-deps.service` back out of `After=`, as long as the token survives
/// somewhere else in the file (a comment, say) — which is exactly the
/// scenario this test exists to catch.
#[test]
fn jarvisd_orders_after_the_dependency_unit_not_a_host_postgres() {
    let unit = read("infra/systemd/jarvisd.service");

    let after = unit
        .lines()
        .find(|line| line.trim_start().starts_with("After="))
        .expect("jarvisd.service declares After=");

    assert!(
        !after.contains("postgresql.service"),
        "jarvisd.service orders after postgresql.service, which does not exist \
         when Postgres is a container — systemd ignores an ordering dependency on \
         an absent unit silently, so the daemon starts before its database. Got: {after}"
    );
    assert!(
        after.contains("jarvis-deps.service"),
        "jarvisd.service must order after jarvis-deps.service, the unit that runs \
         `compose up -d --wait` and therefore does not return until Postgres answers \
         pg_isready. Got: {after}"
    );
}

/// The daemon cannot read its own database URL without this line, and the
/// resulting failure is a fatal config error at startup.
#[test]
fn jarvisd_reads_the_secrets_file() {
    let unit = read("infra/systemd/jarvisd.service");
    assert!(
        unit.contains("EnvironmentFile=/etc/jarvis/secrets.env"),
        "jarvisd.service must read /etc/jarvis/secrets.env — that is where \
         JARVIS_DB_URL lives (F10.9)"
    );
}

/// `up -d` alone returns as soon as containers are RUNNING. Postgres running
/// is not Postgres accepting connections, so without `--wait` the ordering
/// guarantee this unit exists to provide does not exist.
#[test]
fn the_dependency_unit_waits_for_healthchecks() {
    let unit = read("infra/systemd/jarvis-deps.service");

    assert!(
        unit.contains("--wait"),
        "jarvis-deps.service must use `compose up -d --wait`, or ordering after \
         it means only that compose was invoked"
    );
    assert!(
        unit.contains("RemainAfterExit=yes"),
        "a Type=oneshot unit that does not remain after exit is immediately \
         inactive, and units ordered after it lose the guarantee"
    );
    assert!(
        unit.contains("/etc/jarvis/compose/prod.yml"),
        "jarvis-deps.service must point at the installed compose file"
    );
}

/// `prod.yml` deliberately duplicates image tags out of `dev.yml` and
/// `voice.yml` rather than including them (owner: prod.yml must stay
/// self-contained and readable, because an installed host has `prod.yml` and
/// not `dev.yml`). Nothing else keeps the three files in sync, so a version
/// bump applied to only one of them is invisible: a security fix pushed to
/// `voice.yml` alone (say, bumping wyoming-piper past a CVE) leaves the
/// production host, which runs `prod.yml` only, on the old image forever,
/// and nothing before this test said so.
#[test]
fn compose_files_agree_on_shared_image_tags() {
    let files = [
        ("infra/compose/dev.yml", read("infra/compose/dev.yml")),
        ("infra/compose/voice.yml", read("infra/compose/voice.yml")),
        ("infra/compose/prod.yml", read("infra/compose/prod.yml")),
    ];

    // image repository -> [(file, tag), ...]
    let mut by_repository: BTreeMap<String, Vec<(&str, String)>> = BTreeMap::new();
    for (file, content) in &files {
        for (repository, tag) in extract_images(content) {
            by_repository
                .entry(repository)
                .or_default()
                .push((file, tag));
        }
    }

    for (image, occurrences) in &by_repository {
        // A new service added to only one file is not drift — only a SHARED
        // image with mismatched tags is.
        if occurrences.len() < 2 {
            continue;
        }
        let first_tag = &occurrences[0].1;
        let mismatched: Vec<&(&str, String)> = occurrences
            .iter()
            .filter(|(_, tag)| tag != first_tag)
            .collect();
        assert!(
            mismatched.is_empty(),
            "image {image} has mismatched tags across compose files: {occurrences:?} — \
             a version bump applied to one file alone leaves the others (including \
             prod.yml, which is what an installed host actually runs) on the old image"
        );
    }
}
