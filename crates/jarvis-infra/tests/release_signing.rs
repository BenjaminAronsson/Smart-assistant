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

/// Run a snippet with `infra/install/release-manifest.sh` sourced.
///
/// That file holds the ONE definition of what a release contains — the anchored
/// `find` that `release.sh` builds `SHA256SUMS` from and `verify-release.sh`
/// checks the delivered directory against. Tests source the same file rather
/// than reimplementing it, so a fixture cannot quietly enumerate a release
/// differently from the scripts under test.
fn with_manifest_lib(snippet: &str) -> std::process::Output {
    let lib = repo_root().join("infra/install/release-manifest.sh");
    Command::new("bash")
        .arg("-c")
        .arg(format!(
            "set -euo pipefail\n. '{}'\n{snippet}",
            lib.display()
        ))
        .output()
        .expect("bash runs with release-manifest.sh sourced")
}

/// Rebuild `SHA256SUMS` over `dir` exactly as `release.sh` does.
fn reseal(dir: &Path) {
    let out = with_manifest_lib(&format!(
        "release_payload_paths '{dir}' | (cd '{dir}' && xargs -r -d '\\n' sha256sum > SHA256SUMS)",
        dir = dir.display()
    ));
    assert!(
        out.status.success(),
        "building the fixture manifest failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    // A nested payload directory, because the interesting F10.9 case is not a
    // top-level file: compose/postgres-init/ is mounted at
    // /docker-entrypoint-initdb.d and executed by Postgres as superuser.
    std::fs::create_dir_all(dir.join("compose/postgres-init")).expect("postgres-init");
    std::fs::write(
        dir.join("compose/postgres-init/01-extensions.sql"),
        b"create extension if not exists vector;\n",
    )
    .expect("init sql");

    // Built with release.sh's OWN enumeration, so the fixture is a release as
    // release.sh would have produced one rather than as this test imagines it.
    reseal(dir);

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
    //
    // OUTSIDE the release directory. Inside, `key` and `key.pub` would be two
    // files present in the release and absent from SHA256SUMS — which is now,
    // correctly, a verification failure.
    let key = dir.parent().expect("release dir has a parent").join("key");
    let _ = std::fs::remove_file(&key);
    let _ = std::fs::remove_file(key.with_extension("pub"));
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

/// F10.7's version extraction read `crates/jarvisd/Cargo.toml`, which does not
/// carry its own version — it says `version.workspace = true`. A grep of that
/// file for `^version` matches the inheritance line, finds no quotes on it,
/// and `cut -d'"' -f2` hands back the whole line unchanged. The release
/// directory ends up named `jarvis-version.workspace = true`, and that same
/// TOML fragment is written into `RELEASE` and signed inside `SIGNED-PAYLOAD`
/// — a release that verifies cleanly because it is consistently wrong about
/// its own version. `verify-release.sh` cannot catch this: nothing about the
/// signature is broken, only its content is nonsense.
///
/// This test extracts `release.sh`'s `VERSION=` line and runs it for real
/// against the actual workspace tree, asserting the result is a plausible
/// version string rather than merely checking that the script "mentions"
/// Cargo.toml. Against the old one-line grep of `crates/jarvisd/Cargo.toml`
/// this fails immediately (`version.workspace = true` contains whitespace and
/// `=`); against the fix, reading `[workspace.package]` from the repo-root
/// `Cargo.toml`, it produces `0.1.0`.
#[test]
fn release_sh_derives_a_plausible_version_not_a_toml_fragment() {
    let root = repo_root();
    let script = std::fs::read_to_string(root.join("infra/install/release.sh"))
        .expect("release.sh is readable");

    // Pull out exactly the `VERSION="$(...)"` assignment release.sh uses, so
    // this test runs the script's real extraction rather than a hand-rolled
    // stand-in that could drift from it.
    let assign_line = script
        .lines()
        .find(|line| line.trim_start().starts_with("VERSION="))
        .expect("release.sh must have a VERSION= assignment");

    let out = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "set -euo pipefail; REPO='{repo}'; {assign}; echo \"$VERSION\"",
            repo = root.display(),
            assign = assign_line.trim()
        ))
        .output()
        .expect("running release.sh's VERSION= assignment");

    assert!(
        out.status.success(),
        "release.sh's version extraction must succeed against the real workspace tree:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let version = String::from_utf8_lossy(&out.stdout).trim().to_owned();

    assert!(!version.is_empty(), "the derived version must not be empty");
    assert!(
        !version.contains(char::is_whitespace),
        "a version containing whitespace is a TOML fragment, not a version: {version:?}"
    );
    assert!(
        !version.contains('='),
        "a version containing '=' is a TOML fragment (e.g. an unstripped \
         'version.workspace = true'), not a version: {version:?}"
    );
    assert!(
        version.chars().next().is_some_and(|c| c.is_ascii_digit()),
        "a plausible version starts with a digit, got: {version:?}"
    );
}

/// F10.9 widened what a release contains, and therefore what the signature
/// covers.
///
/// Before: the payload was two binaries. Everything else an owner runs —
/// `install.sh`, executed as root; `prod.yml`, which decides what the daemon
/// connects to; the systemd units — travelled unsigned, or not at all. A
/// tampered installer beside a valid signature is a strictly better attack
/// than a tampered binary, because the signature makes the whole directory
/// look checked.
#[test]
fn the_manifest_covers_the_installer_not_only_the_binaries() {
    let script_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("repo root")
        .join("infra/install/release.sh");
    let script = std::fs::read_to_string(&script_path).expect("release.sh is readable");

    assert!(
        !script.contains(r#"sha256sum "${BINARIES[@]}""#),
        "release.sh still checksums only the binaries; install.sh and the \
         systemd units would ship unsigned"
    );
    assert!(
        script.contains("release_payload_paths") && script.contains("SHA256SUMS"),
        "release.sh must checksum every staged file, not a fixed list"
    );
    assert!(
        script.contains("xtask dist --stage"),
        "release.sh must stage through xtask so there is one definition of \
         what ships"
    );
}

/// **A file in the release that the manifest does not list must be refused.**
///
/// `sha256sum -c` verifies every LISTED file. It says nothing about a file
/// present on disk and absent from the manifest — the manifest was a whitelist,
/// not a closed set. While the payload was two binaries an extra file was
/// inert. F10.9 changed that: the release now carries `compose/postgres-init/`,
/// install.sh copies it to `/etc/jarvis/compose/postgres-init`, and prod.yml
/// mounts it at `/docker-entrypoint-initdb.d`, where Postgres executes every
/// `*.sql` and `*.sh` AS SUPERUSER on first initialisation.
///
/// So dropping `compose/postgres-init/00-evil.sql` into a downloaded release
/// passed verification cleanly — every listed file matched, the signature was
/// valid — and then ran as root on first boot. Same adversary, same
/// root-equivalent outcome, as the tampered install.sh this feature was built
/// to close.
#[test]
fn a_file_smuggled_into_the_release_is_refused_even_though_everything_listed_matches() {
    require_ssh_keygen!();
    let scratch = tempfile::tempdir().expect("tempdir");
    let dir = scratch.path().join("jarvis-0.1.0");
    signed_release(&dir, &now(), "pass");

    // Sanity: the release verifies before the smuggled file is added, so a
    // failure below is caused by that file and not by a broken fixture.
    assert!(
        verify(&dir).status.success(),
        "the fixture release must verify before anything is added to it"
    );

    std::fs::write(
        dir.join("compose/postgres-init/00-evil.sql"),
        b"ALTER ROLE jarvis SUPERUSER;\n",
    )
    .expect("smuggle a file");

    let out = verify(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a file present in the release and absent from the signed manifest must \
         be refused — postgres-init runs as superuser on first boot.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("00-evil.sql"),
        "the refusal must NAME the uncovered file; 'verification failed' sends \
         an owner to look at the signature, which is fine.\nstderr:\n{stderr}"
    );
}

/// The manifest is built and checked by the SAME enumeration, in one place.
///
/// Two copies of that `find` drift, and they drift silently: the builder starts
/// covering a path the verifier does not look at (or the reverse) and the
/// release still verifies. So neither script may grow its own — both source
/// `release-manifest.sh`, and this fails if either stops.
#[test]
fn the_manifest_is_built_and_checked_by_one_shared_enumeration() {
    let root = repo_root();
    let lib = std::fs::read_to_string(root.join("infra/install/release-manifest.sh"))
        .expect("release-manifest.sh exists");
    assert_eq!(
        lib.matches("find .").count(),
        1,
        "release-manifest.sh must hold exactly one payload enumeration"
    );

    for script in [
        "infra/install/release.sh",
        "infra/install/verify-release.sh",
    ] {
        let source = std::fs::read_to_string(root.join(script)).expect("script is readable");
        assert!(
            source.contains(". \"$HERE/release-manifest.sh\""),
            "{script} must source the shared enumeration rather than repeating it"
        );
        assert!(
            source.contains("release_payload_paths"),
            "{script} must enumerate the release through release_payload_paths"
        );
        for line in source.lines() {
            let code = line.trim_start();
            assert!(
                code.starts_with('#') || !code.contains("find ."),
                "{script} has its own `find` over the release directory. Two \
                 enumerations drift, and the drift is invisible — the release \
                 still verifies. Got: {code}"
            );
        }
    }
}

/// A relative output directory produced a signed, cleanly-verifying, EMPTY
/// release.
///
/// `mkdir -p "$DEST"` ran in the caller's cwd while
/// `(cd "$REPO" && cargo xtask dist --stage "$DEST")` resolved the same relative
/// path against the repo root — and the README documents the relative form,
/// `infra/install/release.sh dist/`. Run from anywhere but the repo root, the
/// payload landed in `$REPO/dist/…` and the manifest step's `cd "$DEST"` found
/// the empty directory it had just created.
///
/// It did not fail, and that is the whole point: `find … | xargs sha256sum` with
/// EMPTY input still runs `sha256sum` once, which reads stdin and emits a line
/// for `-`. The empty release was signed, and `verify-release.sh` reported
/// "1 artifacts match".
///
/// Three properties, all extracted from or shared with the real script: the
/// output directory is resolved absolutely, an empty staging directory yields an
/// EMPTY manifest, and an implausible manifest is refused before signing.
#[test]
fn release_sh_cannot_sign_an_empty_or_misplaced_staging_directory() {
    let root = repo_root();
    let script = std::fs::read_to_string(root.join("infra/install/release.sh"))
        .expect("release.sh is readable");
    let scratch = tempfile::tempdir().expect("tempdir");

    // 1. $OUT_ROOT is made absolute, so `mkdir -p` and `xtask dist --stage`
    //    cannot resolve it against two different directories. Runs release.sh's
    //    own assignment line from a cwd that is NOT the repo root.
    let resolve = script
        .lines()
        .find(|line| line.trim_start().starts_with("OUT_ROOT=\"$(realpath"))
        .expect(
            "release.sh must resolve OUT_ROOT to an absolute path — a relative \
             one stages into $REPO and signs the empty directory it created",
        );
    let out = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "set -euo pipefail; OUT_ROOT='dist/'; {}; echo \"$OUT_ROOT\"",
            resolve.trim()
        ))
        .current_dir(scratch.path())
        .output()
        .expect("running release.sh's OUT_ROOT resolution");
    let resolved = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    assert!(
        resolved.starts_with('/'),
        "OUT_ROOT must be absolute after resolution, got {resolved:?}"
    );

    // 2. An empty staging directory must yield an EMPTY manifest, not a
    //    one-line manifest over stdin. This is release.sh's real pipeline.
    let empty = scratch.path().join("empty-release");
    std::fs::create_dir_all(&empty).expect("empty release dir");
    let out = with_manifest_lib(&format!(
        "release_payload_paths '{dir}' | (cd '{dir}' && xargs -r -d '\\n' sha256sum > SHA256SUMS)",
        dir = empty.display()
    ));
    assert!(
        out.status.success(),
        "the manifest pipeline failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let sums = std::fs::read_to_string(empty.join("SHA256SUMS")).expect("SHA256SUMS written");
    assert!(
        sums.is_empty(),
        "an empty staging directory must produce an EMPTY manifest. Without \
         `xargs -r`, xargs runs sha256sum once with no operands, it reads STDIN, \
         and the manifest gets a line for `-` — which then signs and verifies. \
         Got:\n{sums}"
    );

    // 3. And that manifest must be refused BEFORE the signature, by the same
    //    function release.sh calls.
    let out = with_manifest_lib(&format!("assert_manifest_plausible '{}'", empty.display()));
    assert!(
        !out.status.success(),
        "signing must be refused over a manifest that cannot describe a release"
    );
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("ABORT"),
        "the refusal must be loud and say why, got:\n{said}"
    );
    assert!(
        script.contains("assert_manifest_plausible"),
        "release.sh must actually call the check before it signs"
    );
    let check_at = script
        .find("assert_manifest_plausible")
        .expect("release.sh calls the check");
    let sign_at = script
        .find("ssh-keygen -Y sign")
        .expect("release.sh signs the payload");
    assert!(
        check_at < sign_at,
        "the plausibility check must run BEFORE the signature — afterwards it \
         would be objecting to something already signed"
    );
}

/// A symlink in the payload is a file that ships, and `-type f` does not see it.
///
/// `web/` is an npm build output: one `ln -s` in a future build and a file would
/// travel inside the release and outside `SHA256SUMS`. Nothing in today's
/// Angular output creates one, which is exactly why this needs a test rather
/// than an assumption.
#[test]
fn a_symlink_in_the_payload_is_covered_by_the_manifest() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let dir = scratch.path().join("jarvis-0.1.0");
    std::fs::create_dir_all(dir.join("web")).expect("web dir");
    std::fs::write(dir.join("web/main.js"), b"console.log(1)\n").expect("asset");
    std::os::unix::fs::symlink("main.js", dir.join("web/current.js")).expect("symlink");

    let out = with_manifest_lib(&format!("release_payload_paths '{}'", dir.display()));
    let listed = String::from_utf8_lossy(&out.stdout);
    assert!(
        listed.lines().any(|path| path == "web/current.js"),
        "a symlink in the payload must be enumerated into the manifest — under \
         `-type f` it ships inside the release and outside the signature. \
         Got:\n{listed}"
    );
}

/// `-name` matches a basename at ANY depth. A future payload file that
/// happens to share a name with one of the five metadata exclusions — e.g.
/// `migrations/RELEASE` — would be silently dropped from `SHA256SUMS` while
/// still shipping inside the tarball: present in the release, absent from
/// what the signature covers. That defeats the exact property the manifest
/// exists to provide.
///
/// This runs release.sh's OWN `find … > SHA256SUMS` pipeline (extracted from
/// the script, not a hand-rolled stand-in that could drift from it) against a
/// staged directory containing a nested file named `RELEASE`, and asserts it
/// survives into the manifest while the five real top-level metadata files
/// stay excluded. Against the old `-name`-based exclusions this fails
/// immediately: `migrations/RELEASE` is dropped just like the real
/// `./RELEASE`. Against the anchored `-path './RELEASE'` fix, only the
/// top-level file is excluded.
#[test]
fn manifest_exclusions_are_anchored_to_the_top_level_not_any_depth() {
    let root = repo_root();
    // The enumeration moved into the shared library when verify-release.sh
    // needed the same one; the property is unchanged.
    let script = std::fs::read_to_string(root.join("infra/install/release-manifest.sh"))
        .expect("release-manifest.sh is readable");

    // Static check: `-name` un-anchored would still "contain find and
    // SHA256SUMS" (the existing coverage test above), so it does not catch a
    // regression to `-name`. Assert the exclusions are path-anchored.
    for excluded in [
        "SHA256SUMS",
        "RELEASE",
        "SIGNED-PAYLOAD",
        "SIGNED-PAYLOAD.sig",
        "signing-key.pub",
    ] {
        let unanchored = format!("-name {excluded}");
        let unanchored_quoted = format!("-name '{excluded}'");
        assert!(
            !script.contains(&unanchored) && !script.contains(&unanchored_quoted),
            "release-manifest.sh excludes {excluded} with -name, which matches at ANY \
             depth — a future payload file such as migrations/{excluded} \
             would be silently dropped from SHA256SUMS while still shipping \
             inside the release"
        );
        let anchored = format!("-path './{excluded}'");
        assert!(
            script.contains(&anchored),
            "release-manifest.sh must exclude {excluded} with an anchored -path \
             './{excluded}', not a bare basename match. Got script:\n{script}"
        );
    }

    // Behavioral check: run the real enumeration, sourced from the file the
    // scripts source, against a staged directory.
    let scratch = tempfile::tempdir().expect("tempdir");
    let dest = scratch.path();
    std::fs::create_dir_all(dest.join("migrations")).expect("migrations dir");
    // A payload file that is NOT metadata, but shares a basename with one.
    std::fs::write(
        dest.join("migrations/RELEASE"),
        b"0001_init.sql is not release metadata\n",
    )
    .expect("nested RELEASE");
    std::fs::write(dest.join("jarvisd"), b"pretend binary\n").expect("binary");
    for metadata in [
        "SHA256SUMS",
        "RELEASE",
        "SIGNED-PAYLOAD",
        "SIGNED-PAYLOAD.sig",
        "signing-key.pub",
    ] {
        std::fs::write(dest.join(metadata), b"pretend metadata\n").expect("write metadata");
    }

    let out = with_manifest_lib(&format!(
        "release_payload_paths '{dir}' | (cd '{dir}' && xargs -r -d '\\n' sha256sum > SHA256SUMS)",
        dir = dest.display()
    ));
    assert!(
        out.status.success(),
        "the shared manifest pipeline failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let sums = std::fs::read_to_string(dest.join("SHA256SUMS")).expect("SHA256SUMS written");
    assert!(
        sums.contains("migrations/RELEASE"),
        "a nested payload file named RELEASE must be checksummed into \
         SHA256SUMS — an unanchored exclusion drops it silently. Got:\n{sums}"
    );
    for metadata in [
        "SHA256SUMS",
        "RELEASE",
        "SIGNED-PAYLOAD",
        "SIGNED-PAYLOAD.sig",
        "signing-key.pub",
    ] {
        assert!(
            !sums.lines().any(|line| line
                .split_whitespace()
                .nth(1)
                .is_some_and(|path| path == metadata)),
            "top-level metadata file {metadata} must stay excluded from \
             SHA256SUMS. Got:\n{sums}"
        );
    }
}
