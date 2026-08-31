use super::hub::*;
use std::time::SystemTime;

use axum::extract::ws::{Message, WebSocket};
use jarvis_contracts::envelope::EventEnvelope;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Rows per page when replaying persisted events on a `?since=` reconnect.
const REPLAY_PAGE: i64 = 256;

/// Replay persisted domain events with `id > since`, paging through the log.
///
/// Filtered by the same [`delivers_to`] rule as live delivery (F7.4, CF-8).
/// Replay is where an unfiltered channel is *worst*: a node reconnecting with
/// `?since=0` would be handed the entire history of the household's runs and
/// approval payloads in one burst.
pub(crate) async fn replay_since(
    socket: &mut WebSocket,
    state: &WsState,
    since: i64,
    device: &crate::auth::DeviceContext,
) -> Result<(), ()> {
    let mut cursor = since;
    loop {
        let rows = match state.events.since(cursor, REPLAY_PAGE).await {
            Ok(rows) => rows,
            // Replay is best-effort; the client can always REST-resync.
            Err(_) => return Ok(()),
        };
        if rows.is_empty() {
            return Ok(());
        }
        let n = rows.len();
        for row in &rows {
            let envelope = state.hub.domain_envelope(row);
            // No stream is owned yet at replay time: a reconnecting socket has
            // not opened a capture stream, so stream-addressed voice events
            // are correctly not its business.
            if delivers_to(
                &envelope,
                device.class,
                Some(device.device_id.as_str()),
                None,
            ) {
                send_envelope(socket, &envelope).await?;
            }
            cursor = row.id;
        }
        if (n as i64) < REPLAY_PAGE {
            return Ok(());
        }
    }
}

pub(crate) async fn send_envelope(
    socket: &mut WebSocket,
    envelope: &EventEnvelope,
) -> Result<(), ()> {
    let text = serde_json::to_string(envelope).expect("envelope serializes");
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

/// `id` (BIGINT, always ≥ 1 for a real row) → `seq`. Negatives cannot occur for
/// an identity column; clamped defensively rather than wrapping.
pub(crate) fn seq_of(id: i64) -> u64 {
    u64::try_from(id).unwrap_or(0)
}

/// A transient event has no stored timestamp — its occurrence *is* now.
pub(crate) fn now_rfc3339() -> String {
    rfc3339(OffsetDateTime::now_utc())
}

pub(crate) fn rfc3339_system_time(at: SystemTime) -> String {
    time::OffsetDateTime::from(at)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

pub(crate) fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).expect("UTC timestamp formats")
}

/// Split a `#[serde(tag = "type")]` value into its discriminator and the
/// remaining fields, so the envelope carries the type and the payload carries
/// only the event's own fields (matching the outbox payload convention).
pub(crate) fn split_tagged(value: serde_json::Value) -> (String, serde_json::Value) {
    match value {
        serde_json::Value::Object(mut map) => {
            let event_type = map
                .remove("type")
                .and_then(|t| t.as_str().map(str::to_owned))
                .unwrap_or_default();
            (event_type, serde_json::Value::Object(map))
        }
        // Typed events always serialize to an object; keep the value as payload.
        other => (String::new(), other),
    }
}
