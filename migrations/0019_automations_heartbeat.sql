-- The daemon's last-seen wall clock (M8b, closes the gate report's D2).
--
-- F8.7 built a restart sweep that announces automations whose moment passed
-- while the daemon was down, and PR #56 tested it — but jarvisd had nowhere to
-- read "when was I last running" from, so it passed `None` and the sweep
-- reported nothing in production. That was the one place in M8 where a test
-- passed and the deployed behaviour did not follow: the feature existed, was
-- correct, and was inert.
--
-- One row, by construction. `only_row` is a boolean primary key with a CHECK
-- that it is true, which is the standard way to say "this table holds exactly
-- one fact" in SQL — a second INSERT collides on the primary key rather than
-- silently giving the daemon two disagreeing opinions about its own history.
--
-- Why a timestamptz and not the minutes-since-midnight the sweep works in: the
-- sweep's window is a time of day, but "how long was it down" is a duration,
-- and the difference matters past 24 hours. A daemon off for three days that
-- stored only a time of day would report a plausible-looking partial window and
-- quietly omit everything else. Storing the instant lets the caller notice that
-- the downtime exceeded a day and say so.
--
-- Not in the `identity` schema even though it is a kind of liveness: this
-- exists to serve the automation restart sweep, and docs/04 §3's rule is that a
-- table belongs to the module that reads it.
--
-- Deliberately outside 0018's append-only trigger: an execution row is history
-- and must never change, but this row is a *cursor* and does nothing but change.
CREATE TABLE automations.daemon_heartbeat (
    only_row BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (only_row),
    last_seen_at TIMESTAMPTZ NOT NULL
);
