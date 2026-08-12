-- M7 F7.2 (FR-19, ADR-031): a node proves possession of a private key at
-- pairing, so the device row records the PUBLIC half.
--
-- Nullable on purpose: the owner's bootstrap device (docs/05 §6.1) pairs with
-- a one-time code over loopback and holds no keypair. A NULL here therefore
-- means "code-paired", not "key missing" — and because revocation kills the
-- token and the key together (F7.1), there is no state where a key outlives
-- the device it authenticated.
ALTER TABLE identity.devices
    ADD COLUMN public_key TEXT;

-- Two devices must never share a key: the key IS the identity a reconnecting
-- node proves, so a duplicate would make "which device is this?" ambiguous.
CREATE UNIQUE INDEX devices_public_key_unique
    ON identity.devices (public_key)
    WHERE public_key IS NOT NULL;

COMMENT ON COLUMN identity.devices.public_key IS
    'base64 Ed25519 public key presented at pairing (ADR-031); NULL for the code-paired bootstrap device';
