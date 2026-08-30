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

/// The value of a systemd directive, ignoring commented-out ones.
///
/// A whole-file `contains("…")` cannot tell a live directive from a comment,
/// and the compose files' own header comments quote the very strings these
/// tests assert on — so `unit.contains("--wait")` stayed green with `--wait`
/// deleted from `ExecStart=`, matching the sentence in the header that explains
/// why `--wait` is there. Same shape for `EnvironmentFile=`: commenting the
/// directive out left the substring in place and the test passing.
fn directive<'a>(unit: &'a str, key: &str) -> Option<&'a str> {
    unit.lines()
        .map(str::trim)
        .find(|line| !line.starts_with('#') && line.starts_with(key))
}

/// Pulls `(repository, tag)` out of every `image:` line in a compose file.
///
/// This is a line-oriented scrape, not a YAML parser — deliberately, since
/// this crate takes on no new dependency to read three small files.
///
/// Every valid reference form has to come out COMPARABLE, because the drift
/// check below simply skips a repository it sees only once. `rsplit_once(':')`
/// alone dropped both of the forms that are not `repo:tag`:
///
///   * untagged (`pgvector/pgvector`) — no `:` at all, so the whole entry
///     vanished and the image looked unshared;
///   * digest-pinned (`pgvector/pgvector@sha256:…`) — split at the digest's
///     own colon, yielding a repository of `pgvector/pgvector@sha256`, which
///     matches nothing.
///
/// Either one silences the drift check for that image: prod.yml pinned by
/// digest against a dev.yml pinned by tag reads as "not shared" and nobody is
/// told. So untagged normalises to docker's implicit `latest` and digests keep
/// their `@…`, both of which compare UNEQUAL to a tag and therefore fail
/// loudly rather than going quiet.
fn extract_images(content: &str) -> Vec<(String, String)> {
    content
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("image:"))
        .map(|rest| rest.split('#').next().unwrap_or(rest).trim())
        .filter(|image_ref| !image_ref.is_empty())
        .map(|image_ref| match image_ref.split_once('@') {
            // Digest pin. Kept whole so it can never compare equal to a tag.
            Some((repository, digest)) => (repository.to_string(), format!("@{digest}")),
            None => match image_ref.rsplit_once(':') {
                // A ':' with a '/' after it is a registry port, not a tag
                // (`localhost:5000/foo`), so that reference is untagged.
                Some((repository, tag)) if !tag.contains('/') => {
                    (repository.to_string(), tag.to_string())
                }
                _ => (image_ref.to_string(), "latest".to_string()),
            },
        })
        .collect()
}

/// `extract_images` is the drift check's only eyes, so its blind spots are the
/// drift check's blind spots. This pins the three forms directly.
#[test]
fn extract_images_reads_untagged_and_digest_pinned_references() {
    let parsed = extract_images(
        "    image: pgvector/pgvector:pg16\n\
             image: pgvector/pgvector\n\
             image: pgvector/pgvector@sha256:abc123\n\
             image: localhost:5000/pgvector/pgvector\n\
             image: otel/collector:0.1 # trailing comment\n",
    );

    assert_eq!(
        parsed,
        vec![
            ("pgvector/pgvector".into(), "pg16".into()),
            ("pgvector/pgvector".into(), "latest".into()),
            ("pgvector/pgvector".into(), "@sha256:abc123".into()),
            ("localhost:5000/pgvector/pgvector".into(), "latest".into()),
            ("otel/collector".into(), "0.1".into()),
        ],
        "every reference form must yield the SAME repository key — otherwise a \
         file pinned by digest and a file pinned by tag look like two different \
         images and the drift check goes quiet on the one pair that matters"
    );
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

    // The directive, not the file: this unit's own comment block names
    // /etc/jarvis/secrets.env twice while explaining why it is read, so a
    // whole-file substring check passes against a COMMENTED-OUT
    // `EnvironmentFile=` — and the daemon then has no JARVIS_DB_URL and
    // fail-fasts on every boot with the test still green.
    let environment_file =
        directive(&unit, "EnvironmentFile=").expect("jarvisd.service declares EnvironmentFile=");
    assert_eq!(
        environment_file, "EnvironmentFile=/etc/jarvis/secrets.env",
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

    // Parse ExecStart=, exactly as the After= test above parses its directive.
    // This unit's HEADER COMMENT says "`compose up -d --wait` blocks on the
    // healthchecks in prod.yml" — so `unit.contains("--wait")` matched the
    // explanation rather than the command, and stayed green with `--wait`
    // deleted from ExecStart=. The check that guards a property must not be
    // satisfiable by prose describing that property.
    let exec_start =
        directive(&unit, "ExecStart=").expect("jarvis-deps.service declares ExecStart=");

    assert!(
        exec_start.contains("--wait"),
        "jarvis-deps.service must use `compose up -d --wait`, or ordering after \
         it means only that compose was invoked. Got: {exec_start}"
    );
    assert!(
        exec_start.contains("/etc/jarvis/compose/prod.yml"),
        "jarvis-deps.service must point at the installed compose file. Got: {exec_start}"
    );
    // prod.yml interpolates ${JARVIS_PG_PASSWORD:?…}; without --env-file
    // compose refuses before it starts anything.
    assert!(
        exec_start.contains("--env-file /etc/jarvis/secrets.env"),
        "jarvis-deps.service must pass --env-file: prod.yml requires \
         JARVIS_PG_PASSWORD and compose refuses the whole project without it. \
         Got: {exec_start}"
    );

    assert_eq!(
        directive(&unit, "RemainAfterExit="),
        Some("RemainAfterExit=yes"),
        "a Type=oneshot unit that does not remain after exit is immediately \
         inactive, and units ordered after it lose the guarantee"
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

    // Scoped to the candidate LOOP, not the whole file: the same path appears in
    // the `bad "no compose file found — looked for …"` message. Remove it from
    // the loop and keep the message, and a whole-file `contains` stays green
    // while first-run.sh can once again only see infra/compose/dev.yml — the
    // precise regression this test exists to catch.
    // There are two candidate loops — one picks the container runtime, one picks
    // the compose file — so this must select the second by what it enumerates.
    let candidates = script
        .lines()
        .find(|line| line.trim_start().starts_with("for candidate in") && line.contains(".yml"))
        .expect("first-run.sh must resolve the compose file from a candidate list");
    assert!(
        candidates.contains("/etc/jarvis/compose/prod.yml"),
        "first-run.sh must look for the installed compose file, not only \
         infra/compose/dev.yml. Candidate line: {candidates}"
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

/// A mis-parsed argument must not silently become a real root install.
///
/// `install.sh`'s documented invocation is `sudo ./install.sh`, so DESTDIR="" is
/// not "stage nowhere" — it is the host. Two ways to reach it accidentally, both
/// verified before this fix:
///
///   * `--destdir` as the LAST argument: `DESTDIR="${2:-}"` yielded the empty
///     string, and `install.sh --skip-preflight --skip-systemd --dry-run
///     --destdir` printed `would: useradd … jarvis` and
///     `would: mkdir -p /var/lib/jarvis/artifacts`. The same happens when a
///     caller writes `--destdir "$STAGE"` with STAGE unset.
///   * an unknown flag: `*) shift ;;` swallowed it, so a typo'd
///     `--skip-systmd` silently ENABLED and STARTED units on the host.
///
/// Every case here is checked for a non-zero exit AND for not having reached
/// the point where it says what it would do, since the whole failure was that
/// it happily proceeded. `--dry-run` throughout: if the parser were still
/// broken, this test must not be able to touch the machine it runs on.
#[test]
fn install_refuses_an_argument_it_cannot_safely_interpret() {
    let script = repo_root().join("infra/install/install.sh");
    // A scratch cwd, not the repo: install.sh resolves its sources from
    // BASH_SOURCE, so cwd is irrelevant to what it reads — but if the parser
    // ever regressed, `--destdir --dry-run` would stage a tree named
    // `--dry-run/` wherever this ran. It can do that in a temp directory.
    let scratch = std::env::temp_dir().join(format!("jarvis-install-args-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch cwd");
    let run = |args: &[&str]| {
        Command::new("bash")
            .arg(&script)
            .args(args)
            .current_dir(&scratch)
            .output()
            .expect("install.sh runs")
    };

    for (case, args) in [
        (
            "--destdir as the last argument",
            vec![
                "--skip-preflight",
                "--skip-systemd",
                "--dry-run",
                "--destdir",
            ],
        ),
        (
            "--destdir with an empty value",
            vec![
                "--skip-preflight",
                "--skip-systemd",
                "--dry-run",
                "--destdir",
                "",
            ],
        ),
        (
            "--destdir= with an empty value",
            vec![
                "--skip-preflight",
                "--skip-systemd",
                "--dry-run",
                "--destdir=",
            ],
        ),
        (
            "--destdir swallowing the next flag",
            vec!["--skip-preflight", "--destdir", "--dry-run"],
        ),
        (
            "a typo'd flag that would otherwise touch systemd",
            vec!["--skip-preflight", "--skip-systmd", "--dry-run"],
        ),
    ] {
        let output = run(&args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{case}: must be a usage error (exit 2), not an install of the host.\n\
             stdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            !stdout.contains("would:"),
            "{case}: install.sh got as far as saying what it would do, which \
             means it accepted the argument.\nstdout:\n{stdout}"
        );
        assert!(
            stderr.contains("usage"),
            "{case}: the refusal must show usage.\nstderr:\n{stderr}"
        );
    }

    std::fs::remove_dir_all(&scratch).ok();
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
        // verify-release.sh sources this; a release without it cannot verify
        // itself, and release.sh's own plausibility check names it too.
        "install/release-manifest.sh",
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

/// The README ships inside the tarball, where it is the only documentation a
/// host has. It must lead with installing rather than with how the project was
/// built, and its facts must be current — a wrong instruction here costs more
/// than a wrong instruction anywhere else, because it is read before anything
/// works.
#[test]
fn readme_leads_with_installing_and_is_not_stale() {
    let readme = read("README.md");

    let install_at = readme
        .find("## Install")
        .expect("README has an Install section");
    let build_at = readme
        .find("## Building it yourself")
        .expect("README has a Building it yourself section");
    assert!(
        install_at < build_at,
        "Install must come before the build/contributor material — the README \
         ships inside the release tarball"
    );

    for stale in ["ready for M0", "Milestones M0–M8", "ADR-001 … ADR-026"] {
        assert!(
            !readme.contains(stale),
            "README still claims {stale:?}, which stopped being true milestones ago"
        );
    }

    for required in [
        "install.sh",
        "verify-release.sh",
        "systemctl",
        "backup.sh",
        "restore.sh",
    ] {
        assert!(
            readme.contains(required),
            "the README must show {required} — an owner should not have to find \
             backup and restore in docs/09 §3"
        );
    }
}

/// Stages a fresh install under a private `--destdir` and returns its root.
///
/// Shared by the tests below so each one asserts on the file the installer
/// ACTUALLY PRODUCES rather than on the example it copies from — the two
/// diverged for the entire life of this feature, and every substring check
/// against the example stayed green while no fresh install could start.
fn stage_install(tag: &str) -> std::path::PathBuf {
    let staging = std::env::temp_dir().join(format!(
        "jarvis-install-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
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
    staging
}

/// Reads the staged `jarvisd.toml` through a real TOML parser and reports the
/// three facts these tests care about, one per line.
///
/// Python's `tomllib` rather than a Rust crate because this crate takes on no
/// new dependency to read one file, and `Command` is already how this file
/// runs shell. A parser is the point: the defect below was invalid *syntax*,
/// which no substring check can see.
fn parse_staged_config(staging: &Path) -> String {
    let config = staging.join("etc/jarvis/jarvisd.toml");
    let output = Command::new("python3")
        .arg("-c")
        .arg(
            r#"
import sys, tomllib
with open(sys.argv[1], "rb") as handle:
    config = tomllib.load(handle)
server = config.get("server", {})
print("bind=%s" % server.get("bind"))
print("web_assets=%s" % server.get("web_assets"))
print("tls=%s" % ("yes" if server.get("tls") else "no"))
"#,
        )
        .arg(&config)
        .output()
        .expect("python3 runs (used here as a TOML parser)");
    assert!(
        output.status.success(),
        "the config install.sh produced is not valid TOML.\n\
         file: {config:?}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The blocker that made every fresh install abort.
///
/// install.sh appended `\n[server]\nweb_assets = …` to a config whose example
/// ALREADY declares `[server]`. TOML forbids declaring a table twice, so the
/// produced file failed to parse — and `Config::load()` runs before the
/// subcommand match in jarvisd's `main()`, so the installer's own
/// `jarvisd migrate` step died and the install ended on "migrations failed —
/// jarvisd was NOT started". Every test in this file was green throughout: they
/// all read `infra/jarvisd.toml.example`, which is fine, and none read the file
/// the installer writes.
#[test]
fn the_config_install_sh_produces_parses_and_points_at_the_web_assets() {
    let staging = stage_install("config");
    let facts = parse_staged_config(&staging);

    assert!(
        facts.contains("web_assets=/var/lib/jarvis/web"),
        "the staged config must set [server].web_assets to the installed \
         path, or the daemon serves no UI. Got:\n{facts}"
    );

    std::fs::remove_dir_all(&staging).ok();
}

/// jarvisd runs as `User=jarvis` and reads its own config.
///
/// `umask 077` was set for the secrets heredoc and never restored, so it
/// leaked into everything install.sh created afterwards: jarvisd.toml and both
/// unit files came out 0600 root:root. /etc/jarvis is 0755, so the path
/// resolves and then the read returns EACCES — an unreadable config is a fatal
/// error, and the daemon fail-fasts on every boot of the machine this feature
/// exists to set up.
///
/// The other half matters just as much: secrets.env must NOT drift open while
/// this is being fixed.
#[test]
fn the_installed_files_carry_the_modes_the_service_account_needs() {
    use std::os::unix::fs::PermissionsExt as _;

    let staging = stage_install("modes");
    let mode = |relative: &str| -> u32 {
        std::fs::metadata(staging.join(relative))
            .unwrap_or_else(|error| panic!("stat {relative}: {error}"))
            .permissions()
            .mode()
            & 0o777
    };

    assert_eq!(
        mode("etc/jarvis/secrets.env"),
        0o600,
        "secrets.env holds the Postgres password; systemd reads it as root \
         before dropping to User=jarvis, so nothing else may read it"
    );
    for readable in [
        "etc/jarvis/jarvisd.toml",
        "etc/systemd/system/jarvisd.service",
        "etc/systemd/system/jarvis-deps.service",
    ] {
        assert_eq!(
            mode(readable),
            0o644,
            "{readable} must be readable by User=jarvis — 0600 here is the \
             leaked-umask bug, and it fail-fasts the daemon on every boot"
        );
    }

    std::fs::remove_dir_all(&staging).ok();
}

/// A fresh install must be able to serve on the address it was configured for.
///
/// The shipped config had `bind = "0.0.0.0:8741"` with an ACTIVE
/// `[server.tls]` pointing at `/var/lib/jarvis/tls/{cert,key}.pem`, and
/// nothing in install.sh ever generated a certificate there. jarvisd loads the
/// certificate unconditionally and propagates the error, deliberately — so the
/// daemon refused to start before binding anything.
///
/// Both halves of the pair are checked, so either resolution passes and a
/// half-applied one does not: loopback-plaintext (the shipped default), or
/// non-loopback with TLS *and* an install.sh that generates the certificate.
/// Note the second branch is not dead — it is what makes this test still
/// correct if the default is ever flipped back.
#[test]
fn the_staged_config_never_names_a_certificate_nobody_generates() {
    let staging = stage_install("tls");
    let facts = parse_staged_config(&staging);
    let installer = read("infra/install/install.sh");

    let loopback = facts.contains("bind=127.0.0.1:") || facts.contains("bind=[::1]:");
    let tls = facts.contains("tls=yes");

    if tls {
        assert!(
            installer.contains("generate-tls-cert.sh"),
            "the staged config enables [server.tls], so install.sh must \
             generate the certificate it names — jarvisd loads it before it \
             binds and refuses to start without it. Got:\n{facts}"
        );
    } else {
        assert!(
            loopback,
            "the staged config has no [server.tls], so it may only bind \
             loopback: jarvisd refuses a non-loopback bind without TLS \
             (docs/06 §7) and would fail-fast on every boot. Got:\n{facts}"
        );
    }

    std::fs::remove_dir_all(&staging).ok();
}

/// The README is the only documentation a host has, and it tells the owner to
/// open `http://127.0.0.1:8741/`. That URL is a claim about the packaged
/// config: plaintext, on loopback. Either the config matches it or the README
/// is wrong the first time anyone reads it.
#[test]
fn the_readme_url_matches_the_packaged_bind() {
    let staging = stage_install("readme-url");
    let facts = parse_staged_config(&staging);
    let readme = read("README.md");

    if readme.contains("http://127.0.0.1:8741") {
        assert!(
            facts.contains("bind=127.0.0.1:8741") && facts.contains("tls=no"),
            "the README sends the owner to http://127.0.0.1:8741/, but the \
             packaged config does not serve plaintext there. Got:\n{facts}"
        );
    }
    assert!(
        readme.contains("generate-tls-cert.sh"),
        "the README must document how to turn TLS on — satellites cannot pair \
         without it, and the config no longer enables it by default"
    );

    std::fs::remove_dir_all(&staging).ok();
}

/// Ordering, in the one script that owns it.
///
/// install.sh used to install the new binaries and THEN call update.sh, whose
/// first step is the backup. So a failed backup aborted with "Your house is
/// untouched" over a host whose daemon was stopped and whose old binary was
/// already gone — the operator had neither a rollback point nor the binary
/// that matched the schema on disk.
///
/// This asserts on offsets in update.sh rather than on a substring anywhere in
/// it, because the defect was never a missing step: every step was present, in
/// the wrong order. Running a real upgrade needs root, systemd and a live
/// database, none of which a unit test may have — so the ordering is checked
/// where it is decided.
#[test]
fn the_upgrade_backs_up_before_it_replaces_anything() {
    let update = read("infra/install/update.sh");

    let backup_at = update
        .find("\"$HERE/backup.sh\"")
        .expect("update.sh calls backup.sh");
    let payload_at = update
        .find("install -m 0755 \"$PAYLOAD/bin/jarvisd\"")
        .expect("update.sh installs the delegated payload");
    let migrate_at = update
        .find("jarvisd migrate")
        .expect("update.sh applies migrations");

    assert!(
        backup_at < payload_at,
        "update.sh replaces the binaries before taking the backup — its own \
         abort message then says 'your house is untouched' about a host that \
         has already lost its old daemon"
    );
    assert!(
        payload_at < migrate_at,
        "update.sh migrates before installing the payload — the OLD binary's \
         embedded migration stream would run, and the health gate would judge \
         the OLD daemon: an upgrade that reports success without upgrading"
    );

    let installer = read("infra/install/install.sh");
    let upgrade_branch_at = installer
        .find("existing install detected")
        .expect("install.sh has an upgrade branch");
    let delegation_at = installer[upgrade_branch_at..]
        .find("update.sh")
        .map(|offset| upgrade_branch_at + offset)
        .expect("the upgrade branch delegates to update.sh");
    let upgrade_branch = &installer[upgrade_branch_at..delegation_at];
    assert!(
        !upgrade_branch.contains("/usr/local/bin/"),
        "install.sh writes /usr/local/bin before delegating to update.sh — the \
         payload must be handed over with --payload so it lands after the \
         verified backup. Got:\n{upgrade_branch}"
    );
    assert!(
        installer.contains("--payload"),
        "install.sh must hand the payload to update.sh with --payload"
    );
}

/// The backup has to be able to RUN, and on a stock host it could not.
///
/// backup.sh uses a host `pg_dump` unless `JARVIS_PG_CONTAINER` names a
/// container — and the README's `apt install` line installs no
/// postgresql-client, install.sh's preflight does not check for one, and
/// install.sh never set the variable. So `pg_dump` was not found, the backup
/// failed, and the upgrade aborted on a host that had already been changed.
///
/// The container name is derived from prod.yml rather than hardcoded twice:
/// compose builds it as `<project>-<service>-1`, so renaming the project in
/// prod.yml alone would silently point the backup at a container that does not
/// exist — and the symptom would again be an upgrade that cannot back up.
#[test]
fn the_upgrade_backs_up_through_the_database_container() {
    let installer = read("infra/install/install.sh");
    let prod = read("infra/compose/prod.yml");

    let project = prod
        .lines()
        .find_map(|line| line.strip_prefix("name:"))
        .map(str::trim)
        .expect("prod.yml sets a compose project name");
    let expected = format!("{project}-postgres-1");

    assert!(
        installer.contains("JARVIS_PG_CONTAINER"),
        "install.sh must pass JARVIS_PG_CONTAINER when it delegates an \
         upgrade: an installed host has no pg_dump, so backup.sh — and \
         therefore the whole upgrade — cannot run without it"
    );
    assert!(
        installer.contains(&expected),
        "install.sh must name the container compose actually creates \
         ({expected}, from prod.yml's `name: {project}` plus the postgres \
         service). Got a different name, which backup.sh would fail to exec into."
    );

    let readme = read("README.md");
    assert!(
        readme.contains("JARVIS_PG_CONTAINER"),
        "the README's backup recipe must set JARVIS_PG_CONTAINER too — run \
         verbatim on a stock host it fails on a missing pg_dump"
    );
}

/// Nothing ever restarted the daemon, so every documented upgrade failed.
///
/// update.sh's step 3 printed "start jarvisd now, then this script waits" —
/// written for a human driving it by hand. install.sh runs it
/// non-interactively after `systemctl stop jarvisd` and never started it
/// again, so `sudo ./install/install.sh` polled a stopped daemon for two
/// minutes and then printed restore instructions over a database that had
/// migrated perfectly well.
///
/// This runs the real script with stubs: a backup that succeeds, a `jarvisd`
/// on PATH that "migrates", a health URL nothing answers, and one attempt
/// instead of sixty. The start hook must have fired before the poll — proven
/// by the marker file it leaves — and the script must still fail, because the
/// daemon never came up. Nothing here touches /usr/local/bin: no --payload is
/// passed.
#[test]
fn the_upgrade_starts_the_daemon_before_it_health_gates_it() {
    let sandbox = std::env::temp_dir().join(format!("jarvis-update-start-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    let bin = sandbox.join("bin");
    let backups = sandbox.join("backups");
    std::fs::create_dir_all(&bin).expect("sandbox bin");
    std::fs::create_dir_all(&backups).expect("sandbox backups");

    // A backup that succeeds and leaves a rollback point where update.sh
    // looks for one.
    let stub_dir = sandbox.join("install");
    std::fs::create_dir_all(&stub_dir).expect("sandbox install dir");
    std::fs::copy(
        repo_root().join("infra/install/update.sh"),
        stub_dir.join("update.sh"),
    )
    .expect("copy update.sh");
    write_executable(
        &stub_dir.join("backup.sh"),
        "#!/usr/bin/env bash\nmkdir -p \"$1/jarvis-00000000T000000Z\"\nexit 0\n",
    );
    // `jarvisd migrate` succeeds; the upgrade then depends entirely on step 3.
    write_executable(&bin.join("jarvisd"), "#!/usr/bin/env bash\nexit 0\n");

    let marker = sandbox.join("started");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new("bash")
        .arg(stub_dir.join("update.sh"))
        .arg(&backups)
        .env("PATH", path)
        .env("DATABASE_URL", "postgres://jarvis:x@127.0.0.1:5432/jarvis")
        .env("JARVIS__STORAGE__ARTIFACTS_ROOT", sandbox.join("artifacts"))
        .env("JARVIS_START_CMD", format!("touch {}", marker.display()))
        // Port 1 answers nothing, so the gate fails on its first attempt.
        .env("JARVIS_HEALTH_URL", "http://127.0.0.1:1/health")
        .env("JARVIS_HEALTH_ATTEMPTS", "1")
        .output()
        .expect("update.sh runs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        marker.exists(),
        "update.sh reached its health gate without starting the daemon — the \
         exact reason every non-interactive upgrade timed out.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !output.status.success(),
        "a daemon that never reports healthy must fail the upgrade"
    );
    assert!(
        stderr.contains("restore.sh"),
        "the F10.3 failure message (how to roll back) must survive.\nstderr:\n{stderr}"
    );

    // The standalone, human-driven path this script documents must still
    // exist: with the start command explicitly empty, it asks rather than acts.
    let output = Command::new("bash")
        .arg(stub_dir.join("update.sh"))
        .arg(&backups)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("DATABASE_URL", "postgres://jarvis:x@127.0.0.1:5432/jarvis")
        .env("JARVIS__STORAGE__ARTIFACTS_ROOT", sandbox.join("artifacts"))
        .env("JARVIS_START_CMD", "")
        .env("JARVIS_HEALTH_URL", "http://127.0.0.1:1/health")
        .env("JARVIS_HEALTH_ATTEMPTS", "1")
        .output()
        .expect("update.sh runs standalone");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("start jarvisd now"),
        "the standalone path must still tell a human what to do.\nstdout:\n{stdout}"
    );

    std::fs::remove_dir_all(&sandbox).ok();
}

/// `--payload` is new argument parsing on the script that owns the upgrade, so
/// it is checked the way install.sh's parser is: by running it. A payload that
/// is not an unpacked release must be refused BEFORE the backup, since the
/// alternative is discovering it after the database has been migrated.
#[test]
fn update_refuses_a_payload_that_is_not_a_release() {
    let script = repo_root().join("infra/install/update.sh");
    let output = Command::new("bash")
        .arg(&script)
        .args(["--payload", "/nonexistent-payload", "/nonexistent-backups"])
        .env("DATABASE_URL", "postgres://jarvis:x@127.0.0.1:5432/jarvis")
        .output()
        .expect("update.sh runs");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "stderr:\n{stderr}");
    assert!(
        stderr.contains("does not look like an unpacked release"),
        "stderr:\n{stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("1/3 backup"),
        "a bad --payload must be caught before anything is touched"
    );
}

/// first-run.sh reported two false failures on every installed host.
///
/// `prod.yml` declares `POSTGRES_PASSWORD: ${JARVIS_PG_PASSWORD:?…}`, and Docker
/// Compose interpolates the whole project model for EVERY subcommand — `ps` and
/// `exec` included. With the variable unset both abort with "required variable
/// … is missing a value" before they look at a container, so the `database` and
/// `migrations` checks printed `PROBLEM: postgres is not running` and
/// `PROBLEM: could not read _sqlx_migrations` against a healthy host, with
/// `2>/dev/null` hiding why. It was invisible during installation only because
/// install.sh sources secrets.env into its own (non-subshell) environment and
/// first-run.sh inherited it — the README's own standalone
/// `sudo ./install/first-run.sh --check-only` got both.
///
/// This runs the real script with a stub `docker` that records its argv, and
/// asserts every compose invocation carries `--env-file`.
///
/// NOT covered here, and it needs saying rather than faking: that compose
/// genuinely refuses without the variable, and that `--env-file` genuinely
/// satisfies `${…:?}`, are properties of Docker Compose against a live daemon.
/// A stub cannot demonstrate them. What a stub CAN prove is the thing that was
/// actually wrong — that the flag was never passed.
#[test]
fn first_run_hands_compose_the_env_file_prod_yml_requires() {
    let sandbox = std::env::temp_dir().join(format!("jarvis-first-run-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    let bin = sandbox.join("bin");
    std::fs::create_dir_all(&bin).expect("sandbox bin");

    let log = sandbox.join("docker-argv");
    // Answers `compose version` so first-run.sh selects it, and records every
    // other invocation. The real calls fail with compose's own refusal text on
    // stderr — the message that used to be swallowed by `2>/dev/null`, leaving
    // "postgres is not running" as the only thing an owner saw.
    write_executable(
        &bin.join("docker"),
        &format!(
            "#!/usr/bin/env bash\n\
             if [[ \"$1\" == compose && \"$2\" == version ]]; then echo 'Docker Compose v2'; exit 0; fi\n\
             printf '%s\\n' \"$*\" >> {log}\n\
             echo 'stub compose refuses' >&2\n\
             exit 1\n",
            log = log.display()
        ),
    );

    let secrets = sandbox.join("secrets.env");
    std::fs::write(&secrets, "JARVIS_PG_PASSWORD=stub-not-a-real-password\n")
        .expect("stub secrets");

    let output = Command::new("bash")
        .arg(repo_root().join("infra/install/first-run.sh"))
        .arg("--check-only")
        .current_dir(repo_root())
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("JARVIS_SECRETS_FILE", &secrets)
        // Nothing answers here, so the daemon checks fail fast instead of
        // reaching out to whatever is on this machine's port 8741.
        .env("JARVIS_BASE_URL", "http://127.0.0.1:1")
        // BOTH, and pointed at an empty sandbox. first-run.sh derives $CONFIG
        // from `${XDG_CONFIG_HOME:-$HOME/.config}`, and GitHub's runners set
        // XDG_CONFIG_HOME — so overriding only HOME left the test reading the
        // real machine's config. That is what made this pass on every developer
        // box (which has a config) and fail in CI (which does not), and the
        // thing it was failing on was a genuine bug: under `set -o pipefail`,
        // `sed` on a missing $CONFIG killed the whole script at "provider
        // workdir". A fresh host is the no-config case, so the test must be too.
        .env("HOME", &sandbox)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .output()
        .expect("first-run.sh runs");

    let stdout_early = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stdout_early.contains("== database"),
        "first-run.sh --check-only stopped before the database step on a host with \
         no config file — which is every fresh host, and the one invocation the \
         README gives an owner for checking a new install.\nstdout:\n{stdout_early}"
    );

    let recorded = std::fs::read_to_string(&log).unwrap_or_else(|error| {
        panic!(
            "first-run.sh never invoked the compose stub ({error}).\nstdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    });

    let compose_calls: Vec<&str> = recorded
        .lines()
        .filter(|line| line.starts_with("compose "))
        .collect();
    assert!(
        compose_calls.iter().any(|line| line.contains(" ps ")),
        "the database check must still run `compose ps`. Recorded:\n{recorded}"
    );
    assert!(
        compose_calls.iter().any(|line| line.contains(" exec ")),
        "the migrations check must still run `compose exec … psql`. Recorded:\n{recorded}"
    );
    for call in &compose_calls {
        assert!(
            call.contains("--env-file"),
            "every compose call must pass --env-file, or prod.yml's \
             ${{JARVIS_PG_PASSWORD:?…}} aborts it and first-run.sh reports a \
             false failure on a healthy host. Got: {call}"
        );
    }

    // And the reason must survive into the output: `2>/dev/null` on these calls
    // is what turned a diagnosable refusal into "postgres is not running".
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("compose said:") || stdout.contains("it said:"),
        "when compose fails, first-run.sh must print what it said rather than \
         discarding it.\nstdout:\n{stdout}"
    );

    std::fs::remove_dir_all(&sandbox).ok();
}

/// Invariant 5: no secrets in prompts, logs, or CLI args.
///
/// backup.sh passed the whole `DATABASE_URL` — password included — as an
/// argument to `pg_dump` and `psql`. `/proc/<pid>/cmdline` is world-readable, so
/// the production credential was visible to every local account for the life of
/// the dump. F10.9 is what makes it matter: install.sh's upgrade path now feeds
/// backup.sh the real generated production password on every upgrade, and the
/// README documents a nightly timer doing the same.
///
/// This runs the real script with stub `pg_dump`/`psql` that record their argv,
/// and asserts the password appears in NONE of it. The stubs also assert the
/// tools can still find the database — the fix must move the password to
/// PGPASSWORD, not simply drop it.
#[test]
fn backup_never_puts_the_database_password_in_argv() {
    let sandbox = std::env::temp_dir().join(format!("jarvis-backup-argv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    let bin = sandbox.join("bin");
    let backups = sandbox.join("backups");
    std::fs::create_dir_all(&bin).expect("sandbox bin");
    std::fs::create_dir_all(&backups).expect("sandbox backups");

    let password = "correct-horse-battery-staple";
    let argv_log = sandbox.join("argv");
    let env_log = sandbox.join("env");

    // Both stubs record argv and whether PGPASSWORD reached them, then answer
    // plausibly enough for backup.sh to run to completion.
    let recorder = format!(
        "printf '%s\\n' \"$0 $*\" >> {argv}\n\
         printf '%s=%s\\n' \"$(basename \"$0\")\" \"${{PGPASSWORD:-<unset>}}\" >> {env}\n",
        argv = argv_log.display(),
        env = env_log.display()
    );
    write_executable(
        &bin.join("pg_dump"),
        &format!(
            "#!/usr/bin/env bash\n{recorder}\
             if [[ \"$1\" == --version ]]; then echo 'pg_dump (PostgreSQL) 16.2'; exit 0; fi\n\
             printf 'PGDMP-stub'\nexit 0\n"
        ),
    );
    write_executable(
        &bin.join("psql"),
        &format!(
            "#!/usr/bin/env bash\n{recorder}\
             for arg in \"$@\"; do\n\
             \x20 if [[ \"$arg\" == *server_version* ]]; then echo '16.2'; exit 0; fi\n\
             done\n\
             exit 0\n"
        ),
    );

    let output = Command::new("bash")
        .arg(repo_root().join("infra/install/backup.sh"))
        .arg(&backups)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env(
            "DATABASE_URL",
            format!("postgres://jarvis:{password}@127.0.0.1:5432/jarvis"),
        )
        .env("JARVIS__STORAGE__ARTIFACTS_ROOT", sandbox.join("artifacts"))
        .output()
        .expect("backup.sh runs");
    assert!(
        output.status.success(),
        "backup.sh failed against the stubs:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let argv = std::fs::read_to_string(&argv_log).expect("the stubs recorded argv");
    assert!(
        !argv.contains(password),
        "the Postgres password reached a command line — /proc/<pid>/cmdline is \
         world-readable (invariant 5). Recorded argv:\n{argv}"
    );
    // Not simply dropped: the tools must still be able to authenticate.
    let env = std::fs::read_to_string(&env_log).expect("the stubs recorded PGPASSWORD");
    assert!(
        env.lines().all(|line| line.ends_with(password)),
        "the password must reach the client tools through PGPASSWORD in the \
         ENVIRONMENT — removing it from argv without that would break every \
         backup. Recorded:\n{env}"
    );
    // The DSN itself must still name the database, or the tools connect nowhere.
    assert!(
        argv.contains("127.0.0.1:5432/jarvis"),
        "the non-secret half of the DSN must survive. Recorded argv:\n{argv}"
    );

    std::fs::remove_dir_all(&sandbox).ok();
}

/// Both scripts, one property: the password never becomes an argument.
///
/// restore.sh had the same shape as backup.sh, and it is the script that runs on
/// the worst day an operator has. Running it end to end needs a live server (it
/// pg_restores), so this checks where the decision is made — no `$DATABASE_URL`
/// is handed to a client tool.
#[test]
fn restore_passes_no_database_url_to_a_client_tool() {
    for script in ["infra/install/backup.sh", "infra/install/restore.sh"] {
        let source = read(script);
        for line in source.lines() {
            let code = line.trim_start();
            if code.starts_with('#') || !code.contains("\"$DATABASE_URL\"") {
                continue;
            }
            assert!(
                // The only permitted uses are splitting it and testing it.
                code.starts_with("DSN=")
                    || code.starts_with("if [[ \"$DATABASE_URL\" =~")
                    || code.starts_with(": \"${DATABASE_URL"),
                "{script} passes $DATABASE_URL — which carries the password — to \
                 a command line. Use $DSN (password stripped) plus PGPASSWORD in \
                 the environment. Got: {code}"
            );
        }
        assert!(
            source.contains("export PGPASSWORD"),
            "{script} must pass the password through the environment"
        );
    }
}

/// The installer's own verification could never change its verdict.
///
/// `"$HERE/first-run.sh" --check-only || true` meant install.sh printed every
/// check as PROBLEM and then `done.`, exiting 0. An installer that reports
/// success over its own failed checks is the false-assurance failure this
/// feature exists to remove, one layer up.
///
/// The two outcomes must stay distinguishable, because they need different
/// actions — so this asserts on the SHAPE of the code (the `|| true` is gone,
/// the status is captured and consulted, and the failing path is its own exit
/// code). Exercising it for real needs root, systemd and a live database, which
/// a unit test may not have: with `--destdir` or `--skip-systemd` the verify
/// block does not run at all, and that is exactly the branch a test can reach.
#[test]
fn the_installer_exit_status_reflects_its_own_verification() {
    let installer = read("infra/install/install.sh");

    let verify_at = installer
        .find("\"$HERE/first-run.sh\" --check-only")
        .expect("install.sh runs its own first-run check");
    let verify_line = installer[verify_at..]
        .lines()
        .next()
        .expect("the check is on a line");
    assert!(
        !verify_line.contains("|| true"),
        "install.sh discards its own verification result with `|| true`, so it \
         exits 0 having printed every check as PROBLEM. Got: {verify_line}"
    );
    assert!(
        verify_line.contains("VERIFY_STATUS=$?"),
        "install.sh must capture the check's exit status. Got: {verify_line}"
    );
    assert!(
        installer.contains("(( VERIFY_STATUS != 0 ))"),
        "install.sh must act on the captured status"
    );
    // "installed but unhealthy" (3) has to be tellable from "install failed"
    // (1, from die()) — they need different responses from an owner.
    assert!(
        installer.contains("exit 3"),
        "a failed verification over a completed install must have its own exit \
         code, distinct from die()'s 1"
    );
    assert!(
        installer.contains("installed but unhealthy") || installer.contains("installed, but"),
        "the message must say the files ARE in place, or an owner reruns the \
         installer instead of fixing the health problem"
    );
}

fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, contents).unwrap_or_else(|error| panic!("writing {path:?}: {error}"));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("chmod {path:?}: {error}"));
}

/// Verifying must come before installing, in that order, on the page.
///
/// install.sh runs as root. A README that shows the install command first and
/// mentions verification afterwards has already lost: the reader ran the
/// script. F10.7 built verify-release.sh precisely so this step exists.
#[test]
fn readme_verifies_before_it_installs() {
    let readme = read("README.md");
    let verify = readme
        .find("verify-release.sh")
        .expect("README shows verification");
    let install = readme
        .find("sudo ./install/install.sh")
        .expect("README shows the install command");
    assert!(
        verify < install,
        "the README tells an owner to run install.sh as root before verifying \
         the signature over it"
    );
}

/// A root install of an artifact nobody checked.
///
/// `install.sh` sits inside the release it installs, next to `SHA256SUMS`,
/// `SIGNED-PAYLOAD.sig` and `verify-release.sh` — and it never looked at any of
/// them. The only thing enforcing "verify first" was the ORDER OF TWO LINES IN
/// THE README, which `readme_verifies_before_it_installs` checks: that test
/// asserts the documentation is in the right order, not that anything verifies.
/// An owner who skipped a line, or who read the output rather than `$?`, got a
/// root install of a tarball that had failed its own check.
#[test]
fn install_refuses_a_release_that_does_not_verify() {
    let sandbox =
        std::env::temp_dir().join(format!("jarvis-install-verify-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    let release = sandbox.join("jarvis-0.0.0");
    let staging = sandbox.join("stage");
    std::fs::create_dir_all(release.join("bin")).expect("release tree");
    std::fs::create_dir_all(release.join("install")).expect("install dir");

    // `bin/` is what install.sh uses to recognise the tarball layout, and
    // SHA256SUMS is what makes it a release rather than a build directory.
    std::fs::write(release.join("bin/jarvisd"), "not a real binary").expect("stub binary");
    std::fs::write(release.join("SHA256SUMS"), "0000  bin/jarvisd\n").expect("stub manifest");
    std::fs::copy(
        repo_root().join("infra/install/install.sh"),
        release.join("install/install.sh"),
    )
    .expect("install.sh copied");
    // The verifier's verdict is the whole point, so it is stubbed to refuse:
    // this test is about what install.sh does with a NO, not about signatures.
    write_executable(
        &release.join("install/verify-release.sh"),
        "#!/usr/bin/env bash\necho 'PROBLEM: stub verifier refuses' >&2\nexit 1\n",
    );

    let output = Command::new("bash")
        .arg(release.join("install/install.sh"))
        .arg("--destdir")
        .arg(&staging)
        .arg("--skip-preflight")
        .arg("--skip-systemd")
        .current_dir(&sandbox)
        .output()
        .expect("install.sh runs");

    assert!(
        !output.status.success(),
        "install.sh installed a release whose verification FAILED.\nstdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !staging.exists() || std::fs::read_dir(&staging).into_iter().flatten().count() == 0,
        "install.sh copied files out of a release that did not verify"
    );

    // And the escape hatch still works, because CI verifies out of band and
    // then installs — but it has to be asked for explicitly.
    let skipped = Command::new("bash")
        .arg(release.join("install/install.sh"))
        .arg("--destdir")
        .arg(&staging)
        .arg("--skip-preflight")
        .arg("--skip-systemd")
        .arg("--skip-verify")
        .current_dir(&sandbox)
        .output()
        .expect("install.sh runs");
    let said = String::from_utf8_lossy(&skipped.stdout);
    assert!(
        said.contains("SKIPPED (--skip-verify)"),
        "--skip-verify must say out loud that nothing was checked.\nstdout:\n{said}"
    );

    std::fs::remove_dir_all(&sandbox).ok();
}

/// `--skip-systemd` is about systemd, and nothing else.
///
/// The `chown -R jarvis:jarvis /var/lib/jarvis` used to live inside the same
/// branch, so `--skip-systemd` on a real host left the artifact store
/// root-owned — and `User=jarvis` cannot write it, the first time anyone starts
/// the unit. `update.sh` guards the same thing explicitly on the upgrade path.
#[test]
fn ownership_is_set_even_when_systemd_is_skipped() {
    let output = Command::new("bash")
        .arg(repo_root().join("infra/install/install.sh"))
        .arg("--dry-run")
        .arg("--skip-preflight")
        .arg("--skip-systemd")
        .current_dir(repo_root())
        .output()
        .expect("install.sh runs");

    let said = String::from_utf8_lossy(&output.stdout);
    assert!(
        said.contains("chown -R jarvis:jarvis /var/lib/jarvis"),
        "--skip-systemd skipped the ownership fix too, so the daemon cannot \
         write its own artifact store.\nstdout:\n{said}"
    );
}

/// The operator who typed `--signers` is the one who cared about authenticity.
///
/// The old parser was `[[ "${2:-}" == "--signers" ]] && SIGNERS="${3:-}"`: every
/// malformed form — the file forgotten, `--signers=path`, a typo — silently left
/// SIGNERS empty and exited 0 with "release verified", downgrading the check to
/// integrity-only without saying so.
#[test]
fn verify_release_refuses_a_signers_argument_it_cannot_use() {
    let script = repo_root().join("infra/install/verify-release.sh");
    let sandbox = std::env::temp_dir().join(format!("jarvis-verify-args-{}", std::process::id()));
    std::fs::create_dir_all(&sandbox).expect("sandbox");

    for args in [
        vec![sandbox.display().to_string(), "--signers".to_owned()],
        vec![
            sandbox.display().to_string(),
            "--signer".to_owned(),
            "/nope".to_owned(),
        ],
        vec![
            sandbox.display().to_string(),
            "--signers".to_owned(),
            "--other".to_owned(),
        ],
        vec![sandbox.display().to_string(), "--signers=".to_owned()],
    ] {
        let output = Command::new("bash")
            .arg(&script)
            .args(&args)
            .output()
            .expect("verify-release.sh runs");
        assert_eq!(
            output.status.code(),
            Some(2),
            "verify-release.sh {args:?} must refuse with a usage error, not \
             continue with authenticity checking silently disabled.\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    std::fs::remove_dir_all(&sandbox).ok();
}

/// The three things a fresh person needs and the tarball cannot give them.
///
/// Each of these was missing from the README while the code that requires it
/// shipped: the wake-word models are provisioned at install time by ADR-032 and
/// are deliberately not in the payload, so a node without them pairs and answers
/// to nothing; the reasoning provider spawns `claude` **as the service user**, so
/// a login in the operator's own shell leaves the daemon healthy and unable to
/// answer; and `first-run.sh` checks for both, at the end, which is after the
/// point where knowing would have helped.
#[test]
fn the_readme_names_what_the_tarball_cannot_provide() {
    let readme = read("README.md");

    assert!(
        readme.contains("fetch-wake-assets.sh"),
        "the README must tell a node operator to provision the wake-word models. \
         They are not in the tarball by design (ADR-032), so a node that skips \
         this pairs successfully and then answers to nothing."
    );
    assert!(
        readme.contains("sudo -u jarvis claude login"),
        "the README must say the Claude CLI is authenticated AS THE SERVICE USER. \
         jarvisd spawns it as `jarvis`; a login in the operator's own shell leaves \
         a daemon that starts, reports healthy, and cannot answer anything."
    );
}
