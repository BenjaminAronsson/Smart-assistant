//! F10.7: a release verifies its own signature, and a stale one is refused.
//!
//! These run `infra/install/verify-release.sh` — the script an operator runs —
//! against release directories assembled here. The build half of
//! `release.sh` is deliberately not invoked: it compiles the workspace in
//! release mode, which is minutes, and what needs testing is the *verification*
//! logic, not `cargo build`.
//!
//! # The claim worth testing
//!
//! A signature proves the bytes are the bytes that were built. It says nothing
//! about whether they were known-vulnerable, and nothing about how long ago
//! anyone checked. The supply-chain gate is the one time-dependent check in this
//! pipeline: during M8, RUSTSEC-2026-0258 turned a green build red with no code
//! change at all — the code had not moved, the world had learned something.
//!
//! So the interesting tests are not "does a good signature verify" (it does)
//! but "is a *perfectly valid* signature over a stale scan refused", and "does
//! tampering that leaves the signature intact still get caught".

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

fn have(tool: &str) -> bool {
    Command::new("which")
        .arg(tool)
        .output()
        .is_ok_and(|o| o.status.success())
}

macro_rules! require_ssh_keygen {
    () => {
        if !have("ssh-keygen") || !have("sha256sum") {
            eprintln!("SKIP: ssh-keygen/sha256sum not available");
            return;
        }
    };
}

/// Assemble a signed release directory, exactly as `release.sh` lays one out.
///
/// `scan_at` is a parameter because the whole point of these tests is what
/// happens when it is old — and waiting a month is not a test strategy.
fn signed_release(dir: &Path, scan_at: &str, status: &str) {
    std::fs::create_dir_all(dir).expect("release dir");
    std::fs::write(dir.join("jarvisd"), b"pretend this is a daemon\n").expect("binary");
    std::fs::write(dir.join("jarvis-agent"), b"pretend this is an agent\n").expect("binary");

    let sums = Command::new("bash")
        .arg("-c")
        .arg("sha256sum jarvisd jarvis-agent | LC_ALL=C sort")
        .current_dir(dir)
        .output()
        .expect("sha256sum");
    std::fs::write(dir.join("SHA256SUMS"), &sums.stdout).expect("write sums");

    std::fs::write(
        dir.join("RELEASE"),
        format!(
            "jarvis-release 1\nversion=0.1.0\nbuilt_at={scan_at}\n\
             advisory_scan_at={scan_at}\nadvisory_scan_status={status}\n"
        ),
    )
    .expect("write RELEASE");

    let payload = [
        std::fs::read(dir.join("SHA256SUMS")).expect("sums"),
        std::fs::read(dir.join("RELEASE")).expect("release"),
    ]
    .concat();
    std::fs::write(dir.join("SIGNED-PAYLOAD"), &payload).expect("payload");

    // A throwaway signing key, generated per test — nothing here touches a real
    // release key, and a test that needed one would be a test nobody could run.
    let key = dir.join("key");
    assert!(
        Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-N", "", "-C", "test", "-f"])
            .arg(&key)
            .output()
            .expect("ssh-keygen")
            .status
            .success()
    );
    assert!(
        Command::new("ssh-keygen")
            .args(["-Y", "sign", "-n", "jarvis-release", "-f"])
            .arg(&key)
            .arg(dir.join("SIGNED-PAYLOAD"))
            .output()
            .expect("sign")
            .status
            .success()
    );
    let pubkey = Command::new("ssh-keygen")
        .arg("-y")
        .arg("-f")
        .arg(&key)
        .output()
        .expect("public key");
    std::fs::write(dir.join("signing-key.pub"), &pubkey.stdout).expect("write pubkey");
}

fn verify(dir: &Path) -> std::process::Output {
    Command::new("bash")
        .arg(repo_root().join("infra/install/verify-release.sh"))
        .arg(dir)
        .output()
        .expect("verify-release.sh")
}

fn now() -> String {
    let out = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .expect("date");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn days_ago(days: u32) -> String {
    let out = Command::new("date")
        .args([
            "-u",
            "-d",
            &format!("{days} days ago"),
            "+%Y-%m-%dT%H:%M:%SZ",
        ])
        .output()
        .expect("date");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// **The feature's acceptance:** a release build verifies its own signature.
#[test]
fn a_fresh_release_verifies_its_own_signature() {
    require_ssh_keygen!();
    let scratch = tempfile::tempdir().expect("tempdir");
    let dir = scratch.path().join("jarvis-0.1.0");
    signed_release(&dir, &now(), "pass");

    let out = verify(&dir);
    assert!(
        out.status.success(),
        "a freshly signed release must verify:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A tampered binary is caught even though the signature is untouched.
#[test]
fn a_swapped_binary_is_caught_by_the_signed_checksums() {
    require_ssh_keygen!();
    let scratch = tempfile::tempdir().expect("tempdir");
    let dir = scratch.path().join("jarvis-0.1.0");
    signed_release(&dir, &now(), "pass");

    std::fs::write(dir.join("jarvisd"), b"something else entirely\n").expect("tamper");

    let out = verify(&dir);
    assert!(
        !out.status.success(),
        "a replaced binary must fail verification"
    );
}

/// Editing the manifest without re-signing is caught.
///
/// The specific attack: rewrite `SHA256SUMS` to match a swapped binary and leave
/// the original `SIGNED-PAYLOAD` alone. The signature still verifies — over
/// bytes nobody would look at again — so the script regenerates the payload from
/// its parts and compares. Without that step this passes.
#[test]
fn rewriting_the_manifest_without_resigning_is_caught() {
    require_ssh_keygen!();
    let scratch = tempfile::tempdir().expect("tempdir");
    let dir = scratch.path().join("jarvis-0.1.0");
    signed_release(&dir, &now(), "pass");

    std::fs::write(dir.join("jarvisd"), b"something else entirely\n").expect("tamper");
    let sums = Command::new("bash")
        .arg("-c")
        .arg("sha256sum jarvisd jarvis-agent | LC_ALL=C sort")
        .current_dir(&dir)
        .output()
        .expect("sha256sum");
    std::fs::write(dir.join("SHA256SUMS"), &sums.stdout).expect("rewrite sums");

    let out = verify(&dir);
    assert!(
        !out.status.success(),
        "a manifest rewritten under an unchanged signature must be refused"
    );
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("diverged") || said.contains("does not match"),
        "the failure must name the divergence, got: {said}"
    );
}

/// **The M8 lesson, enforced.** A perfectly valid signature over a months-old
/// advisory scan is refused.
///
/// RUSTSEC-2026-0258 turned a green pipeline red with no code change. An old
/// clean scan is therefore not evidence that this build is clean now, however
/// impeccable its cryptography.
#[test]
fn a_valid_signature_over_a_stale_advisory_scan_is_refused() {
    require_ssh_keygen!();
    let scratch = tempfile::tempdir().expect("tempdir");
    let dir = scratch.path().join("jarvis-0.1.0");
    signed_release(&dir, &days_ago(90), "pass");

    let out = verify(&dir);
    assert!(
        !out.status.success(),
        "a 90-day-old advisory scan must be refused even with a valid signature"
    );
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("days old"),
        "the failure must say the scan is stale, not merely 'invalid': {said}"
    );
}

/// A release that records a failed scan is refused, not merely noted.
#[test]
fn a_release_recording_a_failed_scan_is_refused() {
    require_ssh_keygen!();
    let scratch = tempfile::tempdir().expect("tempdir");
    let dir = scratch.path().join("jarvis-0.1.0");
    signed_release(&dir, &now(), "fail");

    assert!(
        !verify(&dir).status.success(),
        "advisory_scan_status=fail must refuse, however valid the signature"
    );
}

/// Verification without a trusted key says so, rather than implying authenticity.
///
/// The bundled `signing-key.pub` proves the parts agree; it cannot prove who
/// built them, because a forger replacing the binaries would replace the key
/// too. A verifier that stayed quiet about that would be worse than one that
/// did nothing, because it would be believed.
#[test]
fn verifying_without_a_trusted_key_says_what_it_did_not_prove() {
    require_ssh_keygen!();
    let scratch = tempfile::tempdir().expect("tempdir");
    let dir = scratch.path().join("jarvis-0.1.0");
    signed_release(&dir, &now(), "pass");

    let out = verify(&dir);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("integrity") && said.contains("not authenticity"),
        "verification without a trusted key must not read as proof of origin: {said}"
    );
}
