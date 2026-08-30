//! The installable release payload (F10.9).
//!
//! This module owns WHAT ships. It does not sign, checksum or archive —
//! `infra/install/release.sh` (F10.7) does that, and calls this to stage.
//!
//! One payload, signed once. The alternative considered and rejected was a
//! separate `dist` tarball beside the signed release: that leaves `install.sh`,
//! `prod.yml` and the systemd units unsigned, and a root-executed installer is
//! a better target than the binaries it installs.
//!
//! Copying shells out to `cp`, adding no dependency to a workspace where
//! dependencies are a budgeted resource (docs/09 §5).

use anyhow::{Context, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Repo-relative source path paired with its destination inside the release.
///
/// Pure and dependency-free, so a forgotten file fails a millisecond test
/// rather than an install.
pub fn staged_layout(_version: &str) -> Vec<(PathBuf, String)> {
    [
        ("target/release/jarvisd", "bin/jarvisd"),
        ("target/release/jarvis-agent", "bin/jarvis-agent"),
        ("web/dist/jarvis-shell/browser", "web"),
        ("migrations", "migrations"),
        ("infra/compose/prod.yml", "compose/prod.yml"),
        (
            "infra/compose/otel-collector.yml",
            "compose/otel-collector.yml",
        ),
        ("infra/compose/postgres-init", "compose/postgres-init"),
        (
            "infra/systemd/jarvis-deps.service",
            "systemd/jarvis-deps.service",
        ),
        ("infra/systemd/jarvisd.service", "systemd/jarvisd.service"),
        (
            "infra/systemd/jarvis-agent.service",
            "systemd/jarvis-agent.service",
        ),
        ("infra/install/install.sh", "install/install.sh"),
        ("infra/install/first-run.sh", "install/first-run.sh"),
        ("infra/install/backup.sh", "install/backup.sh"),
        ("infra/install/restore.sh", "install/restore.sh"),
        ("infra/install/update.sh", "install/update.sh"),
        (
            "infra/install/verify-release.sh",
            "install/verify-release.sh",
        ),
        // verify-release.sh SOURCES this — it holds the one definition of what
        // a release contains, shared with release.sh so the manifest's builder
        // and its checker cannot drift. Without it staged, a delivered release
        // cannot verify itself.
        (
            "infra/install/release-manifest.sh",
            "install/release-manifest.sh",
        ),
        ("infra/install/diagnostics.sh", "install/diagnostics.sh"),
        (
            "infra/install/generate-tls-cert.sh",
            "install/generate-tls-cert.sh",
        ),
        (
            "infra/install/fetch-wake-assets.sh",
            "install/fetch-wake-assets.sh",
        ),
        ("infra/jarvisd.toml.example", "jarvisd.toml.example"),
        ("README.md", "README.md"),
    ]
    .into_iter()
    .map(|(source, dest)| (PathBuf::from(source), dest.to_owned()))
    .collect()
}

/// Build everything and stage it into `dest`.
///
/// `release.sh` calls this in place of its own `cargo build`, then checksums
/// and signs whatever landed here.
pub fn stage(dest: &Path, version: Option<&str>) -> anyhow::Result<()> {
    let root = repo_root()?;
    let version = match version {
        Some(explicit) => explicit.to_owned(),
        None => workspace_version(&root)?,
    };

    println!("building release binaries...");
    sh(
        &root,
        "cargo",
        &[
            "build",
            "--release",
            "--locked",
            "-p",
            "jarvisd",
            "-p",
            "jarvis-agent",
        ],
    )?;

    println!("building web assets...");
    sh(&root.join("web"), "npm", &["ci"])?;
    sh(&root.join("web"), "npm", &["run", "build"])?;

    println!("staging into {}...", dest.display());
    std::fs::create_dir_all(dest)?;

    let layout = staged_layout(&version);
    for (source, relative) in &layout {
        let from = root.join(source);
        let to = dest.join(relative);
        if !from.exists() {
            bail!("dist source {from:?} does not exist — the layout and the tree disagree");
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_dir_all(&to);
        if from.is_dir() {
            sh(
                &root,
                "cp",
                &["-r", &from.display().to_string(), &to.display().to_string()],
            )?;
        } else {
            std::fs::copy(&from, &to).with_context(|| format!("copying {from:?}"))?;
            // Scripts must arrive executable: install.sh is run directly.
            if relative.ends_with(".sh") {
                sh(&root, "chmod", &["0755", &to.display().to_string()])?;
            }
        }
    }

    println!("staged {} entries", layout.len());
    Ok(())
}

fn repo_root() -> anyhow::Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .context("locating the repo root from crates/xtask")
}

fn workspace_version(root: &Path) -> anyhow::Result<String> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))?;
    manifest
        .lines()
        .find_map(|line| {
            line.strip_prefix("version = \"")
                .and_then(|rest| rest.split('"').next())
                .map(str::to_owned)
        })
        .context("no version key in the workspace Cargo.toml")
}

fn sh(cwd: &Path, program: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("running {program}"))?;
    if !status.success() {
        bail!("{program} {args:?} failed with {status}");
    }
    Ok(())
}
