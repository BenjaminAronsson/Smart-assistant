#!/usr/bin/env bash
# Generate jarvisd's TLS certificate (F8.9, F7.3, ADR-031 §4).
#
# Self-signed on purpose. There is no CA in a house, and there does not need to
# be one: a node PINS this certificate's fingerprint during pairing, so the
# fingerprint — not a chain — is what turns "encrypted to somebody" into
# "encrypted to the daemon I paired with".
#
# Which means: REGENERATING THIS CERTIFICATE INVALIDATES EVERY PAIRED NODE.
# They will refuse to connect, correctly, because the daemon they pinned is no
# longer the one answering. Re-pair each node afterwards.
set -euo pipefail

OUT_DIR="${1:-/var/lib/jarvis/tls}"
HOSTNAME="${2:-$(hostname -f 2>/dev/null || hostname)}"

if [[ -e "$OUT_DIR/cert.pem" ]]; then
    echo "refusing to overwrite $OUT_DIR/cert.pem" >&2
    echo "every paired node has pinned it; remove it deliberately if you mean to re-pair them all" >&2
    exit 1
fi

mkdir -p "$OUT_DIR"
# 0700: the private key lives here.
chmod 700 "$OUT_DIR"

# SANs cover how a node actually reaches the daemon — a LAN hostname, .local,
# and the loopback the owner's own browser uses. The pin makes the name
# cosmetic, but a wrong name still trips tooling that checks it.
openssl req -x509 -newkey rsa:4096 -sha256 -days 3650 -nodes \
    -keyout "$OUT_DIR/key.pem" \
    -out "$OUT_DIR/cert.pem" \
    -subj "/CN=$HOSTNAME" \
    -addext "subjectAltName=DNS:$HOSTNAME,DNS:${HOSTNAME%%.*}.local,DNS:localhost,IP:127.0.0.1"

chmod 600 "$OUT_DIR/key.pem"
chmod 644 "$OUT_DIR/cert.pem"

echo "wrote $OUT_DIR/cert.pem and $OUT_DIR/key.pem"
echo
echo "fingerprint nodes will pin (sha256 of the DER):"
openssl x509 -in "$OUT_DIR/cert.pem" -outform DER | sha256sum | cut -d' ' -f1
