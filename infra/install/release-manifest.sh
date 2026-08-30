#!/usr/bin/env bash
# What a release contains — ONE definition, sourced by both sides (F10.7/F10.9).
#
# `release.sh` enumerates the staged directory to BUILD `SHA256SUMS`;
# `verify-release.sh` enumerates the delivered directory to CHECK that nothing
# is present on disk that the manifest does not cover. Those two enumerations
# must be the same enumeration. Written out twice they drift, and the way they
# drift is silent: the builder starts covering a path the verifier does not
# look at (or the reverse), and the release still verifies. So there is exactly
# one `find` in this file and neither script has its own — a test
# (crates/jarvis-infra/tests/release_signing.rs) fails if either grows one.
#
# This file is sourced, not executed. It is staged into the release beside
# verify-release.sh so a delivered release can still be verified.

# All payload paths inside a release directory, relative and stably ordered.
#
# `! -type d` and not `-type f`: a symlink anywhere in the payload (web/ is an
# npm build output, which is one `ln -s` away from having one) is a file that
# SHIPS. Under `-type f` it would ship outside SHA256SUMS — inside the release,
# outside the signature. The predicate is stated as "not a directory" rather
# than "a regular file or a symlink" so that the odd entries are caught too: a
# fifo at compose/postgres-init/00-init.sql is covered by neither `-type f` nor
# `-type l`, ships unlisted, survives install.sh's `cp -r`, and hangs the
# Postgres init entrypoint on first boot.
#
# The five exclusions are ANCHORED to the top level (`-path './NAME'`, not
# `-name NAME`): `-name` matches a basename at ANY depth, so a future payload
# file that happens to share a name with one of these — e.g.
# `migrations/RELEASE` — would be silently dropped from SHA256SUMS while still
# shipping inside the release. These five are release metadata that only ever
# exist at the top of the release directory, so anchoring changes nothing about
# today's layout.
#
# LC_ALL=C sort: the manifest must be byte-identical for identical inputs, or
# the signature is over an accident of directory order. `comm` in
# verify-release.sh needs the same collation for the same reason.
release_payload_paths() { # release_payload_paths <release-directory>
	(
		cd "$1" && find . ! -type d \
			! -path './SHA256SUMS' ! -path './RELEASE' ! -path './SIGNED-PAYLOAD' \
			! -path './SIGNED-PAYLOAD.sig' ! -path './signing-key.pub' \
			-printf '%P\n' | LC_ALL=C sort
	)
}

# The paths a manifest covers, one per line, hashes stripped.
#
# `sha256sum` writes "<64 hex><space><space-or-*><path>"; the path may itself
# contain spaces, so this strips a fixed-width prefix rather than splitting on
# whitespace.
manifest_paths() { # manifest_paths <SHA256SUMS-file>
	sed -E 's/^[0-9a-f]{64} [ *]//' "$1"
}

# Refuse to sign a manifest that cannot plausibly describe a release.
#
# `mkdir -p "$DEST"` used to run in the caller's cwd while
# `cargo xtask dist --stage "$DEST"` resolved the same RELATIVE path against
# the repo root — so the README's documented `infra/install/release.sh dist/`,
# run from anywhere but the repo root, staged into `$REPO/dist/...` and then
# built the manifest in the empty directory it had created. That did not fail:
# `find | xargs sha256sum` with empty input still runs `sha256sum` once, which
# reads stdin and emits one line for `-`. The result was a signed, cleanly
# verifying, completely empty release — internally consistent and entirely
# wrong. `release.sh` now resolves $DEST absolutely and passes `xargs -r`; this
# is the belt to that brace, and it names files rather than only counting so a
# payload that lost its binaries cannot pass on bulk alone.
release_manifest_min_entries=20
assert_manifest_plausible() { # assert_manifest_plausible <release-directory>
	local dir="$1" entries missing=0 required
	entries="$(manifest_paths "$dir/SHA256SUMS" | grep -c . || true)"
	if ((entries < release_manifest_min_entries)); then
		echo "ABORT: SHA256SUMS lists $entries path(s); a real release has at least $release_manifest_min_entries." >&2
		echo "  Staging almost certainly did not land in $dir. Signing this would produce a" >&2
		echo "  release that verifies cleanly and contains nothing." >&2
		return 1
	fi
	for required in bin/jarvisd bin/jarvis-agent install/install.sh \
		install/verify-release.sh install/release-manifest.sh \
		compose/prod.yml systemd/jarvisd.service jarvisd.toml.example README.md; do
		if ! manifest_paths "$dir/SHA256SUMS" | grep -Fxq "$required"; then
			echo "ABORT: SHA256SUMS does not cover $required." >&2
			missing=1
		fi
	done
	if ((missing)); then
		echo "  Every one of those is something an owner runs or the daemon reads; a" >&2
		echo "  signature that omits them makes the whole directory look checked." >&2
		return 1
	fi
	return 0
}
