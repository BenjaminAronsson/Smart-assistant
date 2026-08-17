#!/usr/bin/env bash
# Provisions the openWakeWord model assets for a node (F8.3, ADR-032).
#
# ADR-032 consequence 3: "Model assets are provisioned, not vendored. They are
# downloaded at install time with a pinned checksum. A repository that commits a
# 20 MB binary blob it did not build cannot meaningfully review the licence of
# what it ships."
#
# So this is the *only* supported way those files arrive. Every download is
# checked against a pinned SHA-256 before it is installed; a mismatch aborts and
# leaves the previous assets untouched, because a half-replaced model chain is
# worse than an old one.
#
# Licences (reviewed in ADR-032 §1): openWakeWord is Apache-2.0; the
# melspectrogram and embedding feature extractors derive from Google's TFHub
# speech-embedding model, Apache-2.0; the pre-trained wake-word models are
# released by the openWakeWord project under Apache-2.0.
#
# Usage:
#   infra/install/fetch-wake-assets.sh [destination]
#
# Destination defaults to the directory the agent reads
# (JARVIS_AGENT_WAKE_MODEL_DIR, else ~/.local/share/jarvis-agent/wake).

set -euo pipefail

RELEASE="https://github.com/dscripka/openWakeWord/releases/download/v0.5.1"
DEST="${1:-${JARVIS_AGENT_WAKE_MODEL_DIR:-$HOME/.local/share/jarvis-agent/wake}}"

# Word-independent stages, shared by every wake word. These are the two files
# whose checksums are also pinned in `wake_onnx.rs`: the installer and the
# daemon must agree about what "the reviewed asset" is, or the check is theatre.
#
# The per-word models are installed under the name the agent derives from the
# configured word (`hey mycroft` -> `hey_mycroft.onnx`), not their upstream
# release name, so that swapping the wake word is a config change (ADR-032 §4).
read -r -d '' ASSETS <<'EOF' || true
melspectrogram.onnx melspectrogram.onnx ba2b0e0f8b7b875369a2c89cb13360ff53bac436f2895cced9f479fa65eb176f
embedding_model.onnx embedding_model.onnx 70d164290c1d095d1d4ee149bc5e00543250a7316b59f31d056cff7bd3075c1f
alexa_v0.1.onnx alexa.onnx 6ff566a01d12670e8d9e3c59da32651db1575d17272a601b7f8a39283dfbae3e
hey_mycroft_v0.1.onnx hey_mycroft.onnx c2a311e8fa1338de89c31b3b46dc4dffd4af2f9a8d6ddead48893c2d301b1f18
hey_jarvis_v0.1.onnx hey_jarvis.onnx 94a13cfe60075b132f6a472e7e462e8123ee70861bc3fb58434a73712ee0d2cb
EOF

mkdir -p "$DEST"
STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT

echo "Provisioning openWakeWord assets into $DEST"

while read -r remote local sha; do
	[ -n "${remote:-}" ] || continue
	echo "  fetching $remote"
	curl --fail --silent --show-error --location \
		--output "$STAGING/$local" "$RELEASE/$remote"

	actual="$(sha256sum "$STAGING/$local" | cut -d' ' -f1)"
	if [ "$actual" != "$sha" ]; then
		echo "ABORT: $remote failed its pinned checksum." >&2
		echo "  expected $sha" >&2
		echo "  actual   $actual" >&2
		echo "Nothing has been installed; the existing assets are untouched." >&2
		exit 1
	fi
done <<<"$ASSETS"

# Only once every file is present and verified.
mv "$STAGING"/*.onnx "$DEST"/

cat <<EOF

Installed $(ls -1 "$DEST"/*.onnx | wc -l) assets.

Pre-trained words available: alexa, hey mycroft, hey jarvis.
Set JARVIS_AGENT_WAKE_WORD to one of them, and build the agent with
  cargo build -p jarvis-agent --features wake-word-onnx

NOTE: the configured default wake word is "Andy" (ADR-032 §1), and openWakeWord
publishes no pre-trained model for it. Until one is trained, a node configured
for "Andy" logs that it cannot answer to its name and falls back to
push-to-talk. See docs/milestones/M8a-gate-report.md.
EOF
