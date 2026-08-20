//! Argument parsing for the daemon binary (F10.9).
//!
//! Deliberately hand-rolled and tiny. `jarvisd` has exactly two modes and no
//! flags; a clap dependency would be a resident-size cost (docs/09 §5) for
//! parsing one word.

/// What the binary was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Run the daemon. The default, and what systemd's `ExecStart` invokes.
    Serve,
    /// Apply the embedded forward migrations, then exit.
    ///
    /// Exists so a host needs no `sqlx-cli`, and therefore no Rust toolchain:
    /// the migration stream is already compiled into `jarvis-infra`.
    Migrate,
}

/// Parse a full argv, including argv[0].
///
/// Returns usage text as the error so the caller decides how to report it.
pub fn parse(argv: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = argv.into_iter().skip(1);
    match args.next().as_deref() {
        None => Ok(Command::Serve),
        Some("migrate") => Ok(Command::Migrate),
        Some(other) => Err(format!(
            "unknown subcommand {other:?}\nusage: jarvisd [migrate]"
        )),
    }
}
