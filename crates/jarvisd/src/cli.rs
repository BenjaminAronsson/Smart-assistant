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
    /// Print usage and exit 0. Asking for help is not an error.
    Help,
}

pub const USAGE: &str = "usage: jarvisd [migrate]";

/// Parse a full argv, including argv[0].
///
/// Returns usage text as the error so the caller decides how to report it.
///
/// Trailing arguments are REJECTED rather than ignored. `jarvisd` has no flags,
/// so an operator who types `jarvisd migrate --dry-run` is asking for something
/// this binary does not offer — and silently discarding the tail turns that
/// request into its exact opposite: a real, irreversible schema change on a
/// production database, run by someone who believed they had asked for a no-op.
/// An unknown subcommand already refuses; an unknown flag must refuse the same
/// way, for the same reason.
pub fn parse(argv: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = argv.into_iter().skip(1);
    let command = match args.next().as_deref() {
        None => Command::Serve,
        Some("migrate") => Command::Migrate,
        // `-h`/`--help` is the one argument that is not a mistake, and it should
        // not be reported like one: usage on stdout, exit 0. Every other script
        // in this release (install.sh, verify-release.sh) already does that.
        Some("-h") | Some("--help") => Command::Help,
        Some(other) => return Err(format!("unknown subcommand {other:?}\n{USAGE}")),
    };
    if let Some(extra) = args.next() {
        return Err(format!(
            "unexpected argument {extra:?} — jarvisd takes no flags, and \
             ignoring this one would run a real migration\n{USAGE}"
        ));
    }
    Ok(command)
}
