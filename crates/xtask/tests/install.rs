//! Packaging and install regressions (F10.9).
//!
//! These assert on the *shipped files* rather than on behaviour, because the
//! failures they guard are configuration failures that appear only on a host
//! nobody has yet — and each one has already happened or was one boot away
//! from happening.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

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

/// first-run.sh is the install's own verification, and it is shipped INSIDE
/// the tarball — where `infra/compose/dev.yml` does not exist. Checking a
/// hardcoded dev path reports "postgres is not running" against a perfectly
/// healthy installed database: a false failure, which this script's own header
/// argues is as bad as a false pass.
#[test]
fn first_run_finds_the_compose_file_in_an_installed_tree() {
    let script = read("infra/install/first-run.sh");

    assert!(
        script.contains("/etc/jarvis/compose/prod.yml"),
        "first-run.sh must look for the installed compose file, not only \
         infra/compose/dev.yml"
    );
    assert!(
        script.contains("COMPOSE_FILE"),
        "first-run.sh must resolve the compose file into one variable rather \
         than repeating a path it can only be right about in a source tree"
    );
}

/// The "migrations" check reported `ok` from `command -v jarvisd` alone. On
/// an installed host `jarvisd` is present BY CONSTRUCTION — it IS the
/// installation — so that check could never fail: a green "migrations ok"
/// heading that verified nothing about migration state. It replaced a `sqlx
/// migrate info` call that read real state, so it was strictly weaker
/// evidence than what it displaced, which is exactly the false-assurance
/// failure mode this script's own header warns about.
///
/// This asserts the check actually queries sqlx's bookkeeping table
/// (`_sqlx_migrations`, via a real `psql` query through compose) and that the
/// `command -v jarvisd` branch — kept only as an informational note — cannot
/// by itself produce an `ok`. Reverting to
/// `if command -v jarvisd; then ok "..."` fails the second assertion: that
/// branch would print `ok` again from tool presence alone.
#[test]
fn first_run_migrations_check_queries_real_migration_state() {
    let script = read("infra/install/first-run.sh");

    let step_start = script
        .find("step \"migrations\"")
        .expect("first-run.sh has a migrations step");
    let step_end = script[step_start..]
        .find("step \"daemon health\"")
        .map(|offset| step_start + offset)
        .expect("first-run.sh has a step after migrations");
    let migrations_step = &script[step_start..step_end];

    assert!(
        migrations_step.contains("_sqlx_migrations"),
        "the migrations step must query sqlx's own _sqlx_migrations \
         bookkeeping table for real state, not merely check that a binary is \
         on PATH. Got:\n{migrations_step}"
    );
    assert!(
        migrations_step.contains("psql"),
        "the migrations step must run a real query against Postgres (e.g. \
         via compose exec psql), not merely detect tool presence. \
         Got:\n{migrations_step}"
    );

    let jarvisd_check_start = migrations_step.find("if command -v jarvisd").expect(
        "migrations step should still note that jarvisd is on PATH \
             (informational), just not gate success on it",
    );
    let jarvisd_block = &migrations_step[jarvisd_check_start..];
    let jarvisd_block_end = jarvisd_block
        .find("\nfi")
        .map(|offset| offset + "\nfi".len())
        .unwrap_or(jarvisd_block.len());
    let jarvisd_block = &jarvisd_block[..jarvisd_block_end];

    assert!(
        !jarvisd_block.contains("ok \""),
        "a `command -v jarvisd` branch must not itself call `ok` — jarvisd \
         is present on every installed host BY CONSTRUCTION (it is the \
         installation), so a check gated only on its presence can never \
         fail. This is the exact defect the migrations check must not \
         reintroduce. Got:\n{jarvisd_block}"
    );
}

/// The installer must be testable without root and without a container
/// runtime, or it will only ever be exercised on the one machine it is aimed
/// at — which is how an installer becomes a thing nobody dares re-run.
#[test]
fn install_script_stages_the_full_layout_under_a_destdir() {
    let staging = std::env::temp_dir().join(format!("jarvis-install-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);

    let script = repo_root().join("infra/install/install.sh");
    let output = Command::new("bash")
        .arg(&script)
        .arg("--destdir")
        .arg(&staging)
        .arg("--skip-preflight")
        .arg("--skip-systemd")
        .current_dir(repo_root())
        .output()
        .expect("install.sh runs");

    assert!(
        output.status.success(),
        "install.sh --destdir failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    for expected in [
        "etc/jarvis/jarvisd.toml",
        "etc/jarvis/secrets.env",
        "etc/jarvis/compose/prod.yml",
        "etc/systemd/system/jarvis-deps.service",
        "etc/systemd/system/jarvisd.service",
        "var/lib/jarvis/artifacts",
        "var/lib/jarvis/claude-work",
    ] {
        assert!(
            staging.join(expected).exists(),
            "install.sh did not produce {expected} under --destdir"
        );
    }

    let secrets = std::fs::read_to_string(staging.join("etc/jarvis/secrets.env"))
        .expect("secrets.env is readable");
    assert!(
        secrets.contains("JARVIS_PG_PASSWORD=") && secrets.contains("JARVIS_DB_URL=postgres://"),
        "secrets.env must define both names the compose file and jarvisd.toml \
         reference, got:\n{secrets}"
    );
    assert!(
        !secrets.contains("jarvis-dev-only") && !secrets.contains("changeme"),
        "install.sh must generate a real password, not ship a placeholder"
    );

    // Re-running must not rotate the password: that would orphan the existing
    // Postgres volume, whose password was set at initdb time and is never
    // re-read. The database would simply stop authenticating after an upgrade.
    let output = Command::new("bash")
        .arg(&script)
        .arg("--destdir")
        .arg(&staging)
        .arg("--skip-preflight")
        .arg("--skip-systemd")
        .current_dir(repo_root())
        .output()
        .expect("install.sh re-runs");
    assert!(output.status.success(), "install.sh is not idempotent");

    let after = std::fs::read_to_string(staging.join("etc/jarvis/secrets.env"))
        .expect("secrets.env is still readable");
    assert_eq!(
        secrets, after,
        "install.sh rotated the Postgres password on re-run — the existing \
         pgdata volume would stop authenticating"
    );

    // `cp -r SRC DEST` nests SRC inside DEST when DEST already exists as a
    // directory, instead of overwriting in place. The web-assets copy avoids
    // this with `rm -rf` first; the migrations and postgres-init copies did
    // not, so a second run of THIS SAME script (the one just above) produced
    // migrations/migrations/ and postgres-init/postgres-init/ nested one
    // level too deep. Assert directly on the doubled-up path, not just on
    // the plain one existing, since the plain one exists either way.
    assert!(
        !staging
            .join("var/lib/jarvis/migrations/migrations")
            .exists(),
        "install.sh nested migrations/migrations/ on re-run — cp -r into an \
         existing destination directory nests instead of overwriting"
    );
    assert!(
        !staging
            .join("etc/jarvis/compose/postgres-init/postgres-init")
            .exists(),
        "install.sh nested postgres-init/postgres-init/ on re-run — cp -r \
         into an existing destination directory nests instead of overwriting"
    );

    std::fs::remove_dir_all(&staging).ok();
}

/// A live bug, not a hypothetical: an earlier `for arg in "$@"` argument
/// parser desynced from the real positional parameters as soon as a
/// `--destdir` (which `shift`s an extra word for its value) was followed by
/// another argument — the loop's borrowed copy of `$@` no longer matched
/// what `shift` had actually consumed. The concrete failure was
/// `--destdir /a --skip-preflight --destdir /b` resolving `DESTDIR` to the
/// empty string instead of `/b`, which is exactly backwards for a flag whose
/// whole contract is "last one wins" and exactly the kind of thing that
/// looks fine in a two-argument manual test and breaks the moment a real
/// invocation adds one more flag.
///
/// This test is entirely `--dry-run`: nothing under `run()` executes, so an
/// old, still-broken parser landing on an empty (i.e. real-system) `DESTDIR`
/// cannot make this test touch the host it runs on — it only ever inspects
/// what install.sh *says* it would do.
#[test]
fn install_script_uses_the_last_destdir_flag() {
    let script = repo_root().join("infra/install/install.sh");
    let output = Command::new("bash")
        .arg(&script)
        .args([
            "--destdir",
            "/a",
            "--skip-preflight",
            "--destdir",
            "/b",
            "--skip-systemd",
            "--dry-run",
        ])
        .current_dir(repo_root())
        .output()
        .expect("install.sh runs");

    assert!(
        output.status.success(),
        "install.sh --dry-run failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("would: mkdir -p /b/etc/jarvis/compose"),
        "install.sh must stage under the LAST --destdir (/b) — a desynced \
         parser resolves DESTDIR to empty or to the first value instead.\n\
         stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("would: mkdir -p /etc/jarvis/compose"),
        "install.sh staged under the bare (unprefixed) path — DESTDIR came \
         out empty, which is the exact failure this test guards against.\n\
         stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("/a/etc/jarvis/compose"),
        "install.sh staged under the FIRST --destdir (/a) instead of the \
         last one.\nstdout:\n{stdout}"
    );
}

/// The layout is a pure function so it can be tested in milliseconds. Staging
/// for real needs a release build plus an npm build — minutes, and wrong for a
/// unit test. This asserts nothing was forgotten; CI (Task 8) stages the real
/// thing and installs it.
#[test]
fn the_release_payload_carries_everything_a_host_needs() {
    let layout = xtask::dist::staged_layout("0.1.0");
    let destinations: Vec<&str> = layout.iter().map(|(_, dest)| dest.as_str()).collect();

    for required in [
        "bin/jarvisd",
        "bin/jarvis-agent",
        "web",
        "migrations",
        "compose/prod.yml",
        "compose/otel-collector.yml",
        "compose/postgres-init",
        "systemd/jarvis-deps.service",
        "systemd/jarvisd.service",
        "systemd/jarvis-agent.service",
        "install/install.sh",
        "install/first-run.sh",
        "install/backup.sh",
        "install/restore.sh",
        "install/update.sh",
        "install/verify-release.sh",
        "jarvisd.toml.example",
        "README.md",
    ] {
        assert!(
            destinations.contains(&required),
            "the release payload is missing {required}; a host cannot be \
             installed without it. Present: {destinations:#?}"
        );
    }
}

/// Catches a file renamed in one place and not the other — months later, at
/// the moment someone is trying to cut a release.
#[test]
fn every_staged_source_path_exists() {
    for (source, dest) in xtask::dist::staged_layout("0.1.0") {
        if source.starts_with("target") || source.starts_with("web/dist") {
            continue; // build outputs; absent until a release build has run
        }
        assert!(
            repo_root().join(&source).exists(),
            "staged_layout maps {source:?} -> {dest}, but that path does not exist"
        );
    }
}
