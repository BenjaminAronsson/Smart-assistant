//! F9.1: the tracked tree contains no worktree gitlinks.
//!
//! Eighteen agent worktrees under `.claude/worktrees/` were tracked as bare
//! `160000` (gitlink) entries with **no `.gitmodules`**. Git records a gitlink as
//! a commit id in someone else's repository; with nothing naming that repository,
//! a fresh `git clone` produces eighteen empty directories it has no way to
//! populate, and `git status` reports permanent phantom modifications for any of
//! them whose HEAD has since moved.
//!
//! It is regenerated the moment an agent creates a worktree and someone runs
//! `git add -A`, which is why this is a test and not a one-time cleanup.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn no_gitlink_is_tracked() {
    let root = repo_root();
    // A build from a source tarball has no .git; that is not a failure.
    let Some(listing) = git(&root, &["ls-files", "-s"]) else {
        eprintln!("SKIP: no git repository here");
        return;
    };

    let gitlinks: Vec<&str> = listing
        .lines()
        .filter(|line| line.starts_with("160000"))
        .collect();

    assert!(
        gitlinks.is_empty(),
        "these gitlinks are tracked, so a fresh clone gets empty directories it \
         cannot populate. If an agent worktree was committed by a `git add -A`, \
         remove it with `git rm --cached <path>` — .gitignore already covers \
         .claude/worktrees/:\n{gitlinks:#?}"
    );
}

#[test]
fn the_worktree_directory_is_ignored() {
    let root = repo_root();
    let ignore = std::fs::read_to_string(root.join(".gitignore")).expect(".gitignore exists");

    assert!(
        ignore
            .lines()
            .any(|line| line.trim() == ".claude/worktrees/"),
        "`.claude/worktrees/` must stay in .gitignore. It is what keeps the \
         gitlinks from coming back on the next `git add -A`, and — because \
         ripgrep honours .gitignore — what keeps `rg --hidden` from returning a \
         hit per stale worktree instead of one per match."
    );
}
