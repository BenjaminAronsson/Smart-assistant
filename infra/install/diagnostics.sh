#!/usr/bin/env bash
# Collect a diagnostics bundle (F10.4, NFR-07, docs/09).
#
# One command that produces one file you can read — or send — when the house
# misbehaves.
#
# WHAT IT WILL NOT CONTAIN.
#
# No secrets, no message bodies, no transcripts, no tool arguments, no device
# names. Not because they are stripped on the way out, but because the bundle's
# shape has nowhere to put them: it is counts, host-defined identifiers,
# timestamps and states. A redaction *filter* is a list somebody has to maintain
# and it fails silently the day a field is added — in the one artifact whose
# entire purpose is being handed to someone else. A redaction *shape* cannot
# fail that way.
#
# That claim is tested, not asserted: `crates/jarvisd/tests/diagnostics_bundle.rs`
# seeds a credential, a spoken transcript and a message body into the database,
# generates a bundle, and fails if any of them appears anywhere in it.
#
# Usage:
#   JARVIS_TOKEN=<owner device token> infra/install/diagnostics.sh [output-file]

set -euo pipefail

OUT="${1:-jarvis-diagnostics-$(date -u +%Y%m%dT%H%M%SZ).json}"
BASE="${JARVIS_BASE_URL:-http://127.0.0.1:8741}"
: "${JARVIS_TOKEN:?JARVIS_TOKEN must be an owner device token (the bundle is ui-scoped)}"

echo "== collecting from $BASE"
if ! curl -sf --max-time 20 "$BASE/api/v1/diagnostics/bundle" \
	-H "Authorization: Bearer $JARVIS_TOKEN" -o "$OUT"; then
	echo >&2
	echo "Could not collect a bundle." >&2
	echo "  * is jarvisd running and reachable at $BASE?" >&2
	echo "  * is JARVIS_TOKEN an owner token? a node's token is refused here on" >&2
	echo "    purpose — a satellite has no business assembling this." >&2
	echo >&2
	echo "If the daemon is down entirely, the unauthenticated health page still" >&2
	echo "answers on loopback: $BASE/api/v1/diagnostics/health" >&2
	exit 1
fi

echo "   ok: $(wc -c < "$OUT") bytes -> $OUT"
echo
echo "Safe to send as-is. It carries counts, versions, adapter states and audit"
echo "event *types* — no message bodies, transcripts, tool arguments or secrets."
echo "Skim it yourself first if you like; that is rather the point of it being"
echo "small and readable:"
echo
echo "  python3 -m json.tool $OUT | head -40"
