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
