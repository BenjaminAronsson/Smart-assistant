-- Owner-tunable runtime settings (F8.8's voice section, F8.11's spend).
--
-- Two tables, and the split is the point.
--
-- `settings.voice` is the *override* layer. `jarvisd.toml` still supplies the
-- defaults and everything security-relevant; this table holds only the handful
-- of values the owner is allowed to change from the shell. Deliberately NOT a
-- generic key/value store: a table that can hold any key is a table that will
-- eventually hold `bind_address` or a secret reference, and the allowlist would
-- then live only in whatever route last touched it. Named columns mean the
-- schema itself refuses anything that is not on the list.
--
-- Nothing in here is a credential. The ElevenLabs API key stays a keyring
-- reference in the config file (invariant 5, ADR-033) — this table records
-- whether the owner has *consented* to using it, never the key itself.
--
-- `settings.elevenlabs_spend` is the durable half of the character budget.
-- Through F8.11 the budget was an `AtomicU64` that reset on every restart,
-- which made "monthly budget" untrue in the one direction that matters: a
-- daemon restarted daily had no ceiling at all. Keyed by period so the budget
-- rolls over on its own rather than needing a cron job to zero it, and so last
-- month's spend is still readable next month.
CREATE SCHEMA settings;

CREATE TABLE settings.voice (
    only_row BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (only_row),
    -- The word a node answers to (ADR-032 §4: configuration, not code).
    -- NULL means "whatever the config file says", so clearing an override is
    -- expressible and is not the same as choosing the empty string.
    wake_word TEXT CHECK (wake_word IS NULL OR length(wake_word) BETWEEN 1 AND 64),
    -- ADR-033's consent gate. NULL means the config file decides.
    elevenlabs_enabled BOOLEAN,
    updated_at TIMESTAMPTZ NOT NULL,
    -- Which device made the change; the audit row carries the rest.
    updated_by_device_id TEXT NOT NULL
);

CREATE TABLE settings.elevenlabs_spend (
    -- 'YYYY-MM', UTC. A text period rather than a date range because the only
    -- question ever asked of it is "how much this month".
    period TEXT PRIMARY KEY CHECK (period ~ '^[0-9]{4}-[0-9]{2}$'),
    spent_characters BIGINT NOT NULL DEFAULT 0 CHECK (spent_characters >= 0)
);
