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

# --- speech-to-text tokenizer ------------------------------------------------
#
# Same rule as above (ADR-032 consequence 3), for the other model asset a voice
# host needs. faster-whisper loads its tokenizer from `tokenizer.json` inside the
# model directory and, when that file is absent, silently falls back to
# DOWNLOADING it from `openai/whisper-tiny` at startup. The rhasspy CTranslate2
# model repos do not ship one — so the STT service reaches the network on every
# cold start, and on a host whose container runtime has no working DNS it does
# not start at all: it crash-loops with LocalEntryNotFoundError while the port
# stays bound by the port-forwarder, which looks healthy from outside.
#
# Seeding the file makes the fallback unreachable: faster-whisper checks
# `os.path.isfile(tokenizer_file)` before it considers the Hub. The tokenizer is
# identical across whisper sizes for a given multilingual-ness, which is why
# upstream uses tiny's for every model.
#
# Skipped without complaint when no whisper volume exists — a node-only install
# has no STT service to provision.
WHISPER_TOKENIZER_SHA=27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566
WHISPER_TOKENIZER_URL=https://huggingface.co/openai/whisper-tiny/resolve/main/tokenizer.json

runtime=""
for candidate in podman docker; do
	command -v "$candidate" >/dev/null 2>&1 && { runtime="$candidate"; break; }
done

volume_dir=""
if [ -n "$runtime" ]; then
	for name in compose_whisper-models whisper-models; do
		volume_dir="$("$runtime" volume inspect "$name" --format '{{.Mountpoint}}' 2>/dev/null || true)"
		[ -n "$volume_dir" ] && break
	done
fi

if [ -z "$volume_dir" ] || [ ! -d "$volume_dir" ]; then
	echo
	echo "No whisper model volume found — skipping the STT tokenizer."
	echo "  (Expected a '$runtime' volume named compose_whisper-models. Run"
	echo "   'docker compose -f infra/compose/voice.yml up -d' first if this host"
	echo "   is meant to run speech-to-text.)"
else
	# Every model snapshot present, so this stays correct if the owner changes
	# the model size in voice.yml and a second snapshot appears.
	mapfile -t snapshots < <(find "$volume_dir" -mindepth 3 -maxdepth 3 \
		-path '*/models--*faster-whisper*/snapshots/*' -type d 2>/dev/null)

	if [ "${#snapshots[@]}" -eq 0 ]; then
		echo
		echo "Whisper volume has no model snapshot yet — skipping the tokenizer."
		echo "  Start the STT service once with working network so it downloads the"
		echo "  model, then re-run this script to seed the tokenizer for later"
		echo "  offline starts."
	else
		echo "  fetching whisper tokenizer.json"
		curl --fail --silent --show-error --location \
			--output "$STAGING/tokenizer.json" "$WHISPER_TOKENIZER_URL"

		actual="$(sha256sum "$STAGING/tokenizer.json" | cut -d' ' -f1)"
		if [ "$actual" != "$WHISPER_TOKENIZER_SHA" ]; then
			echo "ABORT: whisper tokenizer failed its pinned checksum." >&2
			echo "  expected $WHISPER_TOKENIZER_SHA" >&2
			echo "  actual   $actual" >&2
			echo "The wake assets above are installed; STT is untouched." >&2
			exit 1
		fi

		for snapshot in "${snapshots[@]}"; do
			install -m 0644 "$STAGING/tokenizer.json" "$snapshot/tokenizer.json"
			echo "  seeded $(basename "$(dirname "$(dirname "$snapshot")")")"
		done
		compose_hint="docker compose"
		if [ "$runtime" = "podman" ]; then
			compose_hint="podman compose"
		fi
		echo "  restart the STT service to pick it up:"
		echo "    $compose_hint -f infra/compose/voice.yml restart wyoming-whisper"
	fi
fi

cat <<EOF

Installed $(ls -1 "$DEST"/*.onnx | wc -l) assets.

Pre-trained words available: alexa, hey mycroft, hey jarvis.
Set JARVIS_AGENT_WAKE_WORD to one of them, and build the agent with
  cargo build -p jarvis-agent --features wake-word-onnx

The default wake word is "hey jarvis" (ADR-032 §1), which is one of the models
installed above — so a node answers to its name as soon as it restarts.

Any word outside the published set needs a model trained for it. A node
configured for a word it has no model for says so at startup and falls back to
push-to-talk; it never fails to boot.
EOF
