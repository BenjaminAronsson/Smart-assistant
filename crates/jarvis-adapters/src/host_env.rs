//! Keeping host secrets out of the processes the daemon spawns (invariant 5).
//!
//! Until F10.9 the database credential was a keyring reference resolved at
//! startup into a `Redacted<String>` — it never existed as an environment
//! variable, so nothing could inherit it. An installed host cannot use the
//! keyring (a system unit has no login session, so no D-Bus session bus, so no
//! Secret Service), and `jarvisd.service` therefore takes an `EnvironmentFile`.
//! That put the plaintext production DSN into the daemon's process image.
//!
//! `Command` inherits the parent's environment by default, so without this the
//! credential is handed to every child the daemon starts — including `claude`,
//! the one process most exposed to model output and fetched web pages, and the
//! app-builder worker, which runs with network enabled when no worker image is
//! configured. "Secrets are resolved at the adapter boundary" has to mean the
//! adapter boundary is where they *stop*.
//!
//! This is deliberately not `env_clear()`: the Claude CLI needs `HOME`, `PATH`
//! and its own credential paths, and a child spawned with an empty environment
//! fails in ways that look like anything but a security control.

/// Environment variables that carry, or can carry, a host secret.
///
/// `JARVIS_DB_URL` is the DSN, password included. `JARVIS_PG_PASSWORD` is the
/// same password on its own; jarvisd has no use for it at all, but
/// `EnvironmentFile=` loads the whole file, so it is in the process image too.
pub const HOST_SECRET_VARS: &[&str] = &["JARVIS_DB_URL", "JARVIS_PG_PASSWORD"];

/// Remove every [`HOST_SECRET_VARS`] entry from a child's environment.
///
/// Call this on EVERY `Command` the daemon spawns, before `spawn()`. It is
/// cheap, it is idempotent, and the test in this module fails the build if a
/// spawn site in `jarvisd` or `jarvis-adapters` forgets it.
pub fn scrub_secrets(command: &mut tokio::process::Command) -> &mut tokio::process::Command {
    for name in HOST_SECRET_VARS {
        command.env_remove(name);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property, exercised through a real child process: a spawned program
    /// cannot read the DSN out of its own environment.
    #[tokio::test]
    async fn a_scrubbed_child_cannot_see_the_database_credential() {
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("printenv JARVIS_DB_URL; printenv JARVIS_PG_PASSWORD; true")
            // Set explicitly rather than relying on this test process's
            // environment: the point is that scrub_secrets removes what IS
            // there, not that the variable happened to be absent.
            .env(
                "JARVIS_DB_URL",
                "postgres://jarvis:hunter2@127.0.0.1/jarvis",
            )
            .env("JARVIS_PG_PASSWORD", "hunter2");
        scrub_secrets(&mut command);

        let out = command.output().await.expect("sh runs");
        let seen = String::from_utf8_lossy(&out.stdout);
        assert!(
            !seen.contains("hunter2"),
            "the child could still read the credential: {seen:?}"
        );
    }

    /// Structural, and the reason this helper exists rather than two inline
    /// `env_remove` calls: the leak is reintroduced by ADDING a spawn site, and
    /// nothing about a new `Command::new` looks wrong. Every one of them in the
    /// daemon and its adapters must scrub within a few lines.
    ///
    /// Skipped when the sources are not on disk (an installed binary running
    /// its own test suite is not a thing, but a vendored build is).
    #[test]
    fn every_spawn_site_in_the_daemon_scrubs_the_environment() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/")
            .to_path_buf();
        let mut checked = 0usize;
        let mut offenders = Vec::new();

        for crate_name in ["jarvis-adapters", "jarvisd"] {
            let src = root.join(crate_name).join("src");
            let Ok(files) = walk(&src) else { continue };
            for file in files {
                let text = std::fs::read_to_string(&file).unwrap_or_default();
                let lines: Vec<&str> = text.lines().collect();
                for (i, line) in lines.iter().enumerate() {
                    // The helper's own definition and its tests are not spawn
                    // sites of the daemon's.
                    if file.ends_with("host_env.rs") {
                        continue;
                    }
                    // `Command::new(`, not `process::Command::new(`: timer_alert
                    // imports the type, and matching only the fully-qualified
                    // form is how a spawn site hides from a test like this one.
                    if !line.contains("Command::new(") {
                        continue;
                    }
                    checked += 1;
                    let window = lines[i..(i + 30).min(lines.len())].join("\n");
                    if !window.contains("scrub_secrets") {
                        offenders.push(format!("{}:{}", file.display(), i + 1));
                    }
                }
            }
        }

        assert!(
            checked > 0,
            "found no spawn sites at all — this test stopped testing anything"
        );
        assert!(
            offenders.is_empty(),
            "these spawn sites do not call host_env::scrub_secrets, so the child \
             inherits the database credential (invariant 5): {offenders:#?}"
        );
    }

    fn walk(dir: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                out.extend(walk(&path)?);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
        Ok(out)
    }
}
