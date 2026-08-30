//! `jarvisd migrate` applies the embedded migration stream (F10.9).
//!
//! The host has no sqlx-cli and no Rust toolchain, so the binary must be able
//! to migrate itself. These tests cover argument handling only; that the
//! stream itself applies is already covered by every `#[sqlx::test]` in the
//! workspace, which runs the same `MIGRATOR`.

#[test]
fn migrator_is_reachable_and_non_empty() {
    // The subcommand is worth nothing if it runs an empty stream. 21 files
    // exist in migrations/ today; assert only that it is non-empty, so adding
    // a migration does not fail this test.
    assert!(
        !jarvis_infra::MIGRATOR.migrations.is_empty(),
        "embedded migration stream is empty — `jarvisd migrate` would be a no-op"
    );
}

#[test]
fn unknown_subcommand_is_rejected() {
    let usage = jarvisd::cli::parse(["jarvisd", "frobnicate"].into_iter().map(String::from));
    assert!(
        matches!(usage, Err(ref message) if message.contains("usage")),
        "an unknown subcommand must produce usage text, got {usage:?}"
    );
}

#[test]
fn no_argument_means_serve() {
    assert_eq!(
        jarvisd::cli::parse(["jarvisd"].into_iter().map(String::from)),
        Ok(jarvisd::cli::Command::Serve)
    );
}

#[test]
fn migrate_argument_is_recognised() {
    assert_eq!(
        jarvisd::cli::parse(["jarvisd", "migrate"].into_iter().map(String::from)),
        Ok(jarvisd::cli::Command::Migrate)
    );
}

/// `jarvisd migrate --dry-run` must not perform a real migration.
///
/// The parser used to take the first word and discard the rest, so this exact
/// command line — typed by an operator who believed they had asked for a no-op
/// — returned `Command::Migrate` and applied an irreversible schema change to a
/// production database. There is no `--dry-run`; the only safe answer to a flag
/// this binary does not implement is to refuse, the same way an unknown
/// subcommand already does.
#[test]
fn a_flag_jarvisd_does_not_implement_is_refused_not_ignored() {
    for argv in [
        vec!["jarvisd", "migrate", "--dry-run"],
        vec!["jarvisd", "migrate", "--help"],
        vec!["jarvisd", "migrate", "extra"],
        vec!["jarvisd", "--dry-run"],
    ] {
        let parsed = jarvisd::cli::parse(argv.iter().copied().map(String::from));
        assert!(
            matches!(parsed, Err(ref message) if message.contains("usage")),
            "{argv:?} must produce a usage error rather than silently running \
             the subcommand, got {parsed:?}"
        );
    }
}
