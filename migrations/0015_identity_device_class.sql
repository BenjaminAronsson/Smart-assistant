-- M7 F7.1 (FR-19): a paired device gets a CLASS, and the class decides its
-- authority (docs/05 §6.3). Before this, every device inherited the owner's
-- full tool-scope set — correct for a single-owner loopback deployment, wrong
-- the moment a room satellite pairs.
--
-- `scopes` is deliberately left in place but demoted: it is the pairing-time
-- snapshot, kept for audit and diagnostics. Authorization derives from
-- `device_class` (jarvis_domain::identity::DeviceClass), so a stale or
-- tampered scopes row cannot widen what a device may do.

ALTER TABLE identity.devices
    -- Backfilled to 'owner-ui': every device that exists today IS the owner's
    -- bootstrap device (docs/05 §6.1). The default is dropped immediately
    -- afterwards so a future INSERT cannot acquire owner authority by omission.
    ADD COLUMN device_class   TEXT NOT NULL DEFAULT 'owner-ui',
    -- Last time the device was seen (docs/04 §2). Presence semantics land in
    -- M7 F7.4; the device list already shows it.
    ADD COLUMN last_seen_at   TIMESTAMPTZ,
    -- Why the owner revoked it. NULL whenever revoked_at is NULL.
    ADD COLUMN revoked_reason TEXT;

ALTER TABLE identity.devices ALTER COLUMN device_class DROP DEFAULT;

-- A reason without a revocation is a contradiction; fail loudly on write
-- rather than showing "revoked because: stolen" next to an active device.
ALTER TABLE identity.devices
    ADD CONSTRAINT devices_revoked_reason_requires_revocation
    CHECK (revoked_reason IS NULL OR revoked_at IS NOT NULL);

COMMENT ON COLUMN identity.devices.device_class IS
    'owner-ui | display-node | voice-node | room-node — the unit of authority (docs/05 §6.3)';
COMMENT ON COLUMN identity.devices.scopes IS
    'pairing-time snapshot for audit only; authorization derives from device_class';
