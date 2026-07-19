-- Event-driven outbox dispatch (docs/02 §2, sqlx-data skill §5, perf-warden):
-- the dispatcher LISTENs and reacts, it does NOT poll. A statement-level
-- AFTER INSERT trigger fires one NOTIFY per insert statement; the payload is
-- empty because the dispatcher drains all undispatched rows on any wake-up, so
-- it never needs the notification to carry data. NOTIFY is delivered on commit,
-- so the dispatcher only ever sees committed events.

CREATE FUNCTION outbox.notify_new_event() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('outbox_events', '');
    RETURN NULL;
END;
$$;

CREATE TRIGGER outbox_events_notify
    AFTER INSERT ON outbox.outbox_events
    FOR EACH STATEMENT
    EXECUTE FUNCTION outbox.notify_new_event();
