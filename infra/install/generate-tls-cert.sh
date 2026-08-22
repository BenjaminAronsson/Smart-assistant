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

# The daemon that has to READ this key runs as User=jarvis (a system unit with
# ProtectHome=true). Root-owned 0600 under a 0700 directory is exactly right
# for a private key and exactly unreadable for jarvisd: it would fail to load
# [server.tls], and a certificate that cannot be loaded stops startup before
# anything binds (crates/jarvisd/src/main.rs). Hand it to the service account
# here — install.sh's `chown -R` has already run by the time anyone gets here,
# so nothing else will.
if id jarvis >/dev/null 2>&1 && [[ "$(id -u)" -eq 0 ]]; then
    chown -R jarvis:jarvis "$OUT_DIR"
    echo "owner: jarvis:jarvis (the service account that reads the key)"
fi

echo "wrote $OUT_DIR/cert.pem and $OUT_DIR/key.pem"
echo
echo "Now edit /etc/jarvis/jarvisd.toml: set bind = \"0.0.0.0:8741\" and"
echo "uncomment [server.tls], then: systemctl restart jarvisd"
echo
echo "fingerprint nodes will pin (sha256 of the DER):"
openssl x509 -in "$OUT_DIR/cert.pem" -outform DER | sha256sum | cut -d' ' -f1
