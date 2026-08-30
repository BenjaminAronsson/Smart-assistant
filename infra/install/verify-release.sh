#!/usr/bin/env bash
# Verify a release before installing it (F10.7, docs/06 §9).
#
# Checks three separate things, and refuses on any of them:
#
#   1. the signature over the payload is valid;
#   2. the manifest is a CLOSED SET — every listed file matches its SHA-256,
#      and no file is present in the release that the manifest does not list;
#   3. the advisory scan behind this release is recent enough to mean anything.
#
# THE THIRD ONE IS THE POINT.
#
# A signature proves these bytes are the bytes that were built. It says nothing
# about whether they were known-vulnerable, and — crucially — nothing about how
# long ago anyone last checked. The supply-chain check is the one time-dependent
# gate in this pipeline: during M8, RUSTSEC-2026-0258 turned a green build red
# with no code change at all. The code had not moved; the world had learned
# something.
#
# So an artifact signed a year ago is exactly as cryptographically valid as one
# signed this morning, and its "cargo deny passed" is worth far less. This
# refuses a release whose advisory scan is older than MAX_ADVISORY_AGE_DAYS
# rather than letting a stale green tick launder itself into a fresh assurance.
#
# ON THE BUNDLED PUBLIC KEY.
#
# `signing-key.pub` ships with the release so the signature can be checked for
# internal consistency, and this script does that. It is NOT trust: a forger who
# replaced the binaries would replace the key too, and everything here would
# pass. Pass --signers <allowed_signers> to check against a key you already
# trust; without it this script says plainly that it has verified integrity, not
# authenticity. Saying so is the difference between a check and a ritual.
#
# Usage:
#   infra/install/verify-release.sh dist/jarvis-0.1.0
#   infra/install/verify-release.sh dist/jarvis-0.1.0 --signers ~/.jarvis/allowed_signers

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The same definition of "what a release contains" that release.sh built the
# manifest from. Sourced rather than repeated: two copies of that enumeration
# drift, and they drift silently — the builder starts covering a path the
# verifier does not look at, and the release still verifies.
# shellcheck source=release-manifest.sh
. "$HERE/release-manifest.sh"

SRC="${1:-}"
if [[ -z "$SRC" ]]; then
	echo "usage: $0 <release-directory> [--signers <allowed_signers>]" >&2
	exit 2
fi
SIGNERS=""
[[ "${2:-}" == "--signers" ]] && SIGNERS="${3:-}"

NAMESPACE="jarvis-release"
MAX_ADVISORY_AGE_DAYS="${JARVIS_MAX_ADVISORY_AGE_DAYS:-30}"

for f in SHA256SUMS RELEASE SIGNED-PAYLOAD SIGNED-PAYLOAD.sig signing-key.pub; do
	[[ -f "$SRC/$f" ]] || { echo "ABORT: $SRC/$f is missing — not a signed release." >&2; exit 2; }
done

echo "== 1/3 signature"
# The payload is regenerated from its parts rather than trusted as delivered:
# otherwise a forger could edit SHA256SUMS, leave the stale SIGNED-PAYLOAD
# intact, and the signature would still verify over bytes nobody checks again.
if ! diff -q <(cat "$SRC/SHA256SUMS" "$SRC/RELEASE") "$SRC/SIGNED-PAYLOAD" >/dev/null; then
	echo "   PROBLEM: SIGNED-PAYLOAD does not match SHA256SUMS + RELEASE." >&2
	echo "   The signed bytes and the bytes being checked have diverged." >&2
	exit 1
fi

IDENTITY="jarvis-release"
if [[ -n "$SIGNERS" ]]; then
	[[ -f "$SIGNERS" ]] || { echo "ABORT: no such allowed_signers file: $SIGNERS" >&2; exit 2; }
	IDENTITY="$(awk '{print $1; exit}' "$SIGNERS")"
	ALLOWED="$SIGNERS"
	TRUST="authenticity (against $SIGNERS)"
else
	# Self-consistency only. Stated as such below.
	ALLOWED="$(mktemp)"
	trap 'rm -f "$ALLOWED"' EXIT
	echo "$IDENTITY $(cat "$SRC/signing-key.pub")" > "$ALLOWED"
	TRUST="integrity only (the bundled key is not trust)"
fi

if ! ssh-keygen -Y verify -f "$ALLOWED" -I "$IDENTITY" -n "$NAMESPACE" \
	-s "$SRC/SIGNED-PAYLOAD.sig" < "$SRC/SIGNED-PAYLOAD" >/dev/null 2>&1; then
	echo "   PROBLEM: the signature does not verify." >&2
	exit 1
fi
echo "   ok: verified — $TRUST"

echo "== 2/3 artifact checksums"
if ! (cd "$SRC" && sha256sum --quiet -c SHA256SUMS); then
	echo "   PROBLEM: an artifact does not match its signed checksum." >&2
	exit 1
fi

# THE MANIFEST IS A CLOSED SET, NOT A WHITELIST.
#
# `sha256sum -c` verifies every file the manifest LISTS. It says nothing about a
# file present in the release directory and absent from the manifest. When the
# payload was two binaries an extra file was inert. It is not inert now: the
# release carries compose/postgres-init/, install.sh copies it to
# /etc/jarvis/compose/postgres-init, and prod.yml mounts it at
# /docker-entrypoint-initdb.d — where Postgres executes every *.sql and *.sh AS
# SUPERUSER on first initialisation. So dropping compose/postgres-init/00-evil.sql
# into a downloaded release would pass a listed-files-only check cleanly and then
# run as root on first boot: the same adversary and the same outcome as the
# tampered install.sh this signature exists to close.
#
# Enumerated with release_payload_paths, which is the enumeration release.sh
# built SHA256SUMS from — one definition, in release-manifest.sh, so the two
# sides cannot disagree about what should have been covered.
UNLISTED="$(LC_ALL=C comm -23 \
	<(release_payload_paths "$SRC") \
	<(manifest_paths "$SRC/SHA256SUMS" | LC_ALL=C sort))"
if [[ -n "$UNLISTED" ]]; then
	echo "   PROBLEM: these files are in the release but NOT covered by the signed manifest:" >&2
	sed 's/^/     /' <<<"$UNLISTED" >&2
	cat >&2 <<'EOF'
   Every listed file matched its checksum — that is what makes this dangerous.
   An unlisted file is outside the signature entirely, and some of them RUN:
   anything under compose/postgres-init/ is executed by Postgres as superuser
   the first time the database initialises.

   Re-download the release. Do not install this directory.
EOF
	exit 1
fi
echo "   ok: $(wc -l < "$SRC/SHA256SUMS") artifacts match, and nothing else is present"

echo "== 3/3 advisory freshness"
SCAN_AT="$(grep '^advisory_scan_at=' "$SRC/RELEASE" | cut -d= -f2-)"
STATUS="$(grep '^advisory_scan_status=' "$SRC/RELEASE" | cut -d= -f2-)"
if [[ "$STATUS" != "pass" ]]; then
	echo "   PROBLEM: this release records advisory_scan_status=$STATUS." >&2
	exit 1
fi
SCAN_EPOCH="$(date -u -d "$SCAN_AT" +%s 2>/dev/null || echo 0)"
if [[ "$SCAN_EPOCH" == "0" ]]; then
	echo "   PROBLEM: advisory_scan_at is unparseable ($SCAN_AT)." >&2
	exit 1
fi
AGE_DAYS=$(( ( $(date -u +%s) - SCAN_EPOCH ) / 86400 ))
if (( AGE_DAYS > MAX_ADVISORY_AGE_DAYS )); then
	cat >&2 <<EOF
   PROBLEM: the advisory scan behind this release is $AGE_DAYS days old
   (limit $MAX_ADVISORY_AGE_DAYS). The signature is still perfectly valid — that is
   the point. Advisories are published against code that never changed, so an old
   clean scan is not evidence that this build is clean now.

   Re-run infra/install/release.sh to rebuild with a current scan.
EOF
	exit 1
fi
echo "   ok: advisories scanned $AGE_DAYS day(s) ago (limit $MAX_ADVISORY_AGE_DAYS)"

echo
echo "release verified: $SRC"
if [[ -z "$SIGNERS" ]]; then
	echo
	echo "NOTE: verified for integrity, not authenticity. The public key came from"
	echo "the release itself, so this proves the parts agree — not who built them."
	echo "Pass --signers <allowed_signers> with a key you already trust."
fi
