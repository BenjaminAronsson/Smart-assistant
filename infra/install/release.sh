#!/usr/bin/env bash
# Build, checksum and sign a release (F10.7, docs/06 §9, NFR-14).
#
# Produces a directory an owner can verify *without trusting the machine that
# built it*: the binaries, a SHA-256 manifest over them, an SSH signature over
# that manifest, and a record of when the advisory scan was run.
#
# WHY THE ADVISORY DATE IS IN THE MANIFEST.
#
# The supply-chain check is **time-dependent**, which nothing else in this
# pipeline is. During M8, RUSTSEC-2026-0258 turned a green pipeline red with no
# code change whatsoever — the code was identical, the world had learned
# something. A signature proves these bytes are the bytes that were built; it
# says nothing at all about whether they were known-vulnerable at the time, and
# nothing about how long ago anyone last checked.
#
# So "we ran cargo deny" is not a durable claim, and a release process that only
# records *that* it passed is recording the wrong thing. This records **when**,
# and `verify-release.sh` refuses an artifact whose scan is older than
# MAX_ADVISORY_AGE_DAYS. Stale evidence of safety is not evidence of safety.
#
# Usage:
#   JARVIS_RELEASE_KEY=~/.ssh/id_ed25519 infra/install/release.sh dist/
#
# Verify with:
#   infra/install/verify-release.sh dist/jarvis-<version>

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
OUT_ROOT="${1:-}"
if [[ -z "$OUT_ROOT" ]]; then
	echo "usage: JARVIS_RELEASE_KEY=<ssh-private-key> $0 <output-directory>" >&2
	exit 2
fi
: "${JARVIS_RELEASE_KEY:?JARVIS_RELEASE_KEY must point at an SSH private key}"
[[ -f "$JARVIS_RELEASE_KEY" ]] || { echo "ABORT: no such key: $JARVIS_RELEASE_KEY" >&2; exit 2; }

# The namespace scopes the signature: a signature made for `jarvis-release`
# cannot be replayed as a git commit signature, or vice versa.
NAMESPACE="jarvis-release"
# What ships is defined in ONE place — crates/xtask/src/dist.rs — and staged by
# `cargo xtask dist --stage`. A second list here is how a release ends up
# shipping an installer without the compose file it needs (F10.9).

# jarvisd does not carry its own version — crates/jarvisd/Cargo.toml says
# `version.workspace = true`. A grep of *that* file for `^version` matches the
# inheritance line, finds no quotes on it, and `cut` hands back the whole line
# unchanged: VERSION becomes the literal string "version.workspace = true",
# which then gets baked into the release directory name and signed inside
# RELEASE. It is internally consistent — verify-release.sh passes — and
# entirely wrong. The real version lives at the workspace root, under
# [workspace.package]; read it from there, matching workspace_version() in
# crates/xtask/src/dist.rs so the two extractions cannot disagree about what
# is being released.
VERSION="$(grep -m1 '^version = "' "$REPO/Cargo.toml" | cut -d'"' -f2 || true)"
if [[ -z "$VERSION" ]] || [[ "$VERSION" == *[[:space:]]* ]] || [[ "$VERSION" == *"="* ]]; then
	echo "ABORT: could not determine the workspace version from $REPO/Cargo.toml (got: '$VERSION')" >&2
	echo "  Expected a top-level 'version = \"X.Y.Z\"' under [workspace.package]." >&2
	echo "  Proceeding would sign a release named after a TOML fragment, not a version." >&2
	exit 2
fi
DEST="$OUT_ROOT/jarvis-$VERSION"
mkdir -p "$DEST"

echo "== 1/4 advisory scan"
# Run it here rather than trusting that CI ran it at some point: the whole
# argument below depends on knowing exactly when this happened.
SCAN_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
if command -v cargo-deny >/dev/null 2>&1 || cargo deny --version >/dev/null 2>&1; then
	if ! (cd "$REPO" && cargo deny check advisories 2>&1 | tail -5); then
		echo >&2
		echo "ABORT: the advisory scan found unaccepted findings." >&2
		echo "Releasing over a known advisory is a decision, not a default — accept it" >&2
		echo "explicitly in deny.toml with a reason, or fix it." >&2
		exit 1
	fi
	SCAN_STATUS=pass
else
	echo "ABORT: cargo-deny is not installed, so this release has no advisory evidence." >&2
	echo "  cargo install cargo-deny --locked" >&2
	exit 2
fi
echo "   ok: advisories clean at $SCAN_AT"

echo "== 2/4 build and stage"
# Builds the binaries AND the web assets, then stages the full installable
# payload: bin/, web/, migrations/, compose/, systemd/, install/, the config
# example and the README. See crates/xtask/src/dist.rs for the layout.
(cd "$REPO" && cargo xtask dist --stage "$DEST")
echo "   ok: payload staged"

echo "== 3/4 manifest"
# Every staged file, not a fixed list. install.sh runs as root and prod.yml
# decides what the daemon connects to; leaving either outside the signature
# while signing the binaries is worse than signing nothing, because the
# signature makes the whole directory look checked.
#
# Relative paths, LC_ALL=C sort: the manifest must be byte-identical for
# identical inputs, or the signature is over an accident of directory order.
(cd "$DEST" && find . -type f \
    ! -name SHA256SUMS ! -name RELEASE ! -name SIGNED-PAYLOAD \
    ! -name 'SIGNED-PAYLOAD.sig' ! -name signing-key.pub \
    -printf '%P\n' | LC_ALL=C sort | xargs sha256sum > SHA256SUMS)
cat > "$DEST/RELEASE" <<EOF
jarvis-release 1
version=$VERSION
built_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
advisory_scan_at=$SCAN_AT
advisory_scan_status=$SCAN_STATUS
EOF
# The signature covers BOTH files: SHA256SUMS pins the bytes, RELEASE pins the
# advisory date. Signing only the checksums would leave the freshness claim
# unsigned and therefore editable by anyone who could reach the directory.
cat "$DEST/SHA256SUMS" "$DEST/RELEASE" > "$DEST/SIGNED-PAYLOAD"
echo "   ok: $(wc -l < "$DEST/SHA256SUMS") artifacts"

echo "== 4/4 signature"
ssh-keygen -Y sign -f "$JARVIS_RELEASE_KEY" -n "$NAMESPACE" "$DEST/SIGNED-PAYLOAD" >/dev/null 2>&1
# The public half travels with the release so a verifier can check the signature
# is internally consistent. That is NOT the same as trusting it — verification
# against a known key is the operator's job, and verify-release.sh says so.
ssh-keygen -y -f "$JARVIS_RELEASE_KEY" > "$DEST/signing-key.pub"
echo "   ok: signed with $(cut -d' ' -f1 < "$DEST/signing-key.pub")"

echo
echo "release complete: $DEST"
echo "verify with: infra/install/verify-release.sh $DEST"
