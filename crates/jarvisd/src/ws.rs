//! The WebSocket hub and `/ws/v1` upgrade (docs/05 §1-§3). One
//! token-authenticated fan-out carries the owner's run events. Two producers
//! converge here:
//!
//! * committed **domain events** arrive via [`OutboxPublisher`] — the dispatcher
//!   calls us after commit. They are persisted and replayable; `seq` is the
//!   outbox row `id`, the same global cursor the timeline `since` uses.
//! * transient **text deltas** arrive via [`RunEventSink`] straight from the
//!   orchestrator. They are never persisted and never replayed.
//!
//! The hub owns every envelope field (docs/05 §3); payload authors never set
//! `seq`/`occurredAt`/etc. Run **state** changes are deliberately NOT emitted
//! through the sink — they are persisted by the checkpointer and delivered on
//! the outbox path, so the sink drops `StateChanged`/`Finished` to avoid the
//! double-emit the F1.4 review flagged.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use jarvis_application::orchestrator::{RunEventSink, RunUpdate};
use jarvis_application::ports::{DisplayDirectiveSink, RepositoryError};
use jarvis_contracts::CONTRACT_VERSION;
use jarvis_contracts::display::{DisplayDirective, SurfaceDto};
use jarvis_contracts::envelope::{Channel, EventEnvelope};
use jarvis_contracts::events::TransientEvent;
use jarvis_domain::display::Surface;
use jarvis_domain::ids::RunId;
use jarvis_infra::dispatcher::{OutboxPublisher, OutboxRecord, PublishError};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Bounded fan-out buffer. A client that falls this far behind is disconnected
/// (`broadcast::Lagged`) and resyncs via REST — never unbounded buffering
/// (low-power / DoS guard). Generous for a single owner's devices.
const CHANNEL_CAPACITY: usize = 1024;

/// Rows per page when replaying persisted events on a `?since=` reconnect.
const REPLAY_PAGE: i64 = 256;

/// Inbound WS frame/message ceiling. M1 sends no commands over the socket, so
/// this is deliberately tiny — enough for control frames, far below the 64 MiB
/// tungstenite default (DoS hardening, security-auditor F1.5).
const MAX_INBOUND_FRAME_BYTES: usize = 64 * 1024;

/// Read side of the persisted event log (docs/05 §3), abstracted so the hub and
/// timeline endpoint can be driven by a fake in tests. Implemented by
/// `jarvis_infra::events::PgEventLog`; returns raw outbox rows which jarvisd
/// maps to the wire types (infra cannot depend on `jarvis-contracts`).
#[async_trait]
pub trait EventReader: Send + Sync {
    /// Every committed event with `id > since`, oldest first (the WS replay).
    async fn since(&self, since: i64, limit: i64) -> Result<Vec<OutboxRecord>, RepositoryError>;
    /// The persisted timeline for one session with `id > since`, oldest first.
    async fn timeline(
        &self,
        session_id: &str,
        since: i64,
        limit: i64,
    ) -> Result<Vec<OutboxRecord>, RepositoryError>;
}

#[async_trait]
impl EventReader for jarvis_infra::events::PgEventLog {
    async fn since(&self, since: i64, limit: i64) -> Result<Vec<OutboxRecord>, RepositoryError> {
        jarvis_infra::events::PgEventLog::since(self, since, limit).await
    }
    async fn timeline(
        &self,
        session_id: &str,
        since: i64,
        limit: i64,
    ) -> Result<Vec<OutboxRecord>, RepositoryError> {
        jarvis_infra::events::PgEventLog::timeline(self, session_id, since, limit).await
    }
}

pub struct WsHub {
    tx: broadcast::Sender<Arc<EventEnvelope>>,
    /// The largest outbox `id` broadcast so far — the domain resync high-water.
    /// Transient deltas ride at this value; they never advance the cursor, so a
    /// client tracks its `since` from domain events only (docs/05 §3).
    high_water: AtomicU64,
}

impl WsHub {
    pub fn new() -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Arc::new(Self {
            tx,
            high_water: AtomicU64::new(0),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<EventEnvelope>> {
        self.tx.subscribe()
    }

    pub fn high_water(&self) -> u64 {
        self.high_water.load(Ordering::SeqCst)
    }

    /// Envelope for a committed outbox row. `seq` is the row `id` (global,
    /// monotonic, == timeline `since`); `occurredAt` is the row's stored commit
    /// time, so a replayed event keeps its ORIGINAL time, not "now". The payload
    /// is forwarded verbatim, with the discriminator on the envelope `type`
    /// (never re-typed — F1.4 note). Shared by live delivery and `?since=` replay
    /// so the two never disagree.
    fn domain_envelope(&self, record: &OutboxRecord) -> EventEnvelope {
        EventEnvelope {
            v: CONTRACT_VERSION,
            seq: seq_of(record.id),
            channel: Channel::Session,
            event_type: record.event_type.clone(),
            occurred_at: rfc3339(record.created_at),
            trace_id: None,
            resource_version: None,
            payload: record.payload.clone(),
        }
    }

    /// Broadcast a committed domain event. No subscribers is success: the event
    /// is durable and any client resyncs via REST — we never re-deliver just
    /// because nobody is currently listening.
    fn broadcast_domain(&self, record: &OutboxRecord) {
        self.high_water
            .fetch_max(seq_of(record.id), Ordering::SeqCst);
        let _ = self.tx.send(Arc::new(self.domain_envelope(record)));
    }

    /// Broadcast a display directive on the `display` channel (FR-09/10). Like a
    /// text delta it is transient — a command to the agent, not a replayable
    /// timeline event — so it rides at the current high-water `seq` and never
    /// advances the resync cursor. Returns true if at least one WS client was
    /// subscribed (best-effort delivery; no agent connected ⇒ audited-but-
    /// undelivered). `app_id` is derived server-side from the closed surface set,
    /// never from model/user text.
    fn broadcast_display(&self, placement: &jarvis_domain::display::SurfacePlacement) -> bool {
        let directive = DisplayDirective::PlaceSurface {
            surface: surface_dto(placement.surface),
            app_id: placement.surface.app_id().to_owned(),
            monitor: placement.monitor.as_str().to_owned(),
        };
        let (event_type, payload) =
            split_tagged(serde_json::to_value(&directive).expect("directive serializes"));
        let envelope = EventEnvelope {
            v: CONTRACT_VERSION,
            seq: self.high_water.load(Ordering::SeqCst),
            channel: Channel::Display,
            event_type,
            occurred_at: now_rfc3339(),
            trace_id: None,
            resource_version: None,
            payload,
        };
        self.tx.send(Arc::new(envelope)).is_ok()
    }

    /// Broadcast a transient text delta at the current high-water `seq` (it does
    /// not advance the resync cursor; a lost delta is re-derived, docs/05 §3).
    fn broadcast_delta(&self, run_id: &RunId, text: &str) {
        let event = TransientEvent::TextDelta {
            run_id: run_id.clone(),
            text: text.to_owned(),
        };
        // Split the `type` tag out of the payload so the wire matches the outbox
        // convention: discriminator on the envelope, fields in the payload.
        let (event_type, payload) =
            split_tagged(serde_json::to_value(&event).expect("transient event serializes"));
        let envelope = EventEnvelope {
            v: CONTRACT_VERSION,
            seq: self.high_water.load(Ordering::SeqCst),
            channel: Channel::Session,
            event_type,
            occurred_at: now_rfc3339(),
            trace_id: None,
            resource_version: None,
            payload,
        };
        let _ = self.tx.send(Arc::new(envelope));
    }
}

/// The dispatcher publishes committed domain events through this impl.
#[async_trait]
impl OutboxPublisher for WsHub {
    async fn publish(&self, record: &OutboxRecord) -> Result<(), PublishError> {
        // Broadcast never fails per-subscriber, and "no subscribers" is success
        // (durable + REST resync). The `Result` exists for a future delivery
        // path with a fallible durable step; there is none in M1.
        self.broadcast_domain(record);
        Ok(())
    }
}

/// jarvisd dispatches resolved display placements to connected agents here.
#[async_trait]
impl DisplayDirectiveSink for WsHub {
    async fn dispatch(&self, placement: &jarvis_domain::display::SurfacePlacement) -> bool {
        self.broadcast_display(placement)
    }
}

/// Map the domain surface to its wire mirror. Exhaustive on purpose: a new
/// `Surface` variant forces a wire mapping decision here (no `_` arm).
fn surface_dto(surface: Surface) -> SurfaceDto {
    match surface {
        Surface::Conversation => SurfaceDto::Conversation,
        Surface::RunTimeline => SurfaceDto::RunTimeline,
        Surface::ApprovalTray => SurfaceDto::ApprovalTray,
        Surface::ArtifactCanvas => SurfaceDto::ArtifactCanvas,
        Surface::AmbientStatus => SurfaceDto::AmbientStatus,
        Surface::Diagnostics => SurfaceDto::Diagnostics,
    }
}

/// The orchestrator emits run updates through this impl.
#[async_trait]
impl RunEventSink for WsHub {
    async fn emit(&self, update: RunUpdate) {
        match update {
            RunUpdate::TextDelta { run_id, text } => self.broadcast_delta(&run_id, &text),
            // Persisted by the checkpointer and delivered on the outbox path —
            // dropping them here is the double-emit reconciliation (F1.4).
            // CompensationRegistered (F2.3) is likewise a persisted domain event;
            // its outbox delivery + approval-tray rendering lands in F2.5. No
            // tools are wired into jarvisd yet (tools: None), so it never fires.
            RunUpdate::StateChanged { .. }
            | RunUpdate::Finished { .. }
            | RunUpdate::CompensationRegistered { .. } => {}
        }
    }
}

/// `?since=` cursor for the WS reconnect replay. Absent = live-only from now.
#[derive(Debug, Deserialize)]
pub struct WsParams {
    pub since: Option<i64>,
}

/// State the `/ws/v1` route carries: the hub to subscribe to, the event log for
/// replay, and the process shutdown token for a clean close on drain.
#[derive(Clone)]
pub struct WsState {
    pub hub: Arc<WsHub>,
    pub events: Arc<dyn EventReader>,
    pub shutdown: CancellationToken,
}

/// `GET /ws/v1` — authenticated WebSocket upgrade (the bearer middleware has
/// already validated the device when this runs).
pub async fn ws_upgrade(
    State(state): State<WsState>,
    Query(params): Query<WsParams>,
    ws: WebSocketUpgrade,
) -> Response {
    // Absent `since` = live-only from now; `since=0` = replay everything (outbox
    // ids start at 1 and the filter is `id > since`); a negative value clamps to
    // a full replay rather than being rejected.
    let since = params.since.map(|s| s.max(0));
    // M1 accepts NO inbound commands over the socket (run control is REST), so
    // tighten the default 64 MiB ceiling to a small cap — a client has no
    // legitimate large inbound payload (DoS hardening, security-auditor F1.5).
    ws.max_message_size(MAX_INBOUND_FRAME_BYTES)
        .max_frame_size(MAX_INBOUND_FRAME_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state, since))
}

async fn handle_socket(mut socket: WebSocket, state: WsState, since: Option<i64>) {
    // Subscribe BEFORE replaying so no live event slips through the gap. Any
    // overlap between replay and live is deduped by the client on `seq` (the
    // outbox id is unique and monotonic).
    let mut rx = state.hub.subscribe();

    if let Some(since) = since
        && replay_since(&mut socket, &state, since).await.is_err()
    {
        return; // client gone (or replay failed → it can REST-resync)
    }

    loop {
        tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => {
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            received = rx.recv() => match received {
                Ok(envelope) => {
                    if send_envelope(&mut socket, &envelope).await.is_err() {
                        return;
                    }
                }
                // Too far behind: close so the client reconnects and resyncs
                // (persisted events are recovered via `?since=` / REST).
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            // Inbound frames: M1 accepts no commands over the socket (run control
            // is REST) — we only honour a Close and keep the connection healthy.
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Close(_))) | None => return,
                Some(Ok(_)) => {}
                Some(Err(_)) => return,
            },
        }
    }
}

/// Replay persisted domain events with `id > since`, paging through the log.
async fn replay_since(socket: &mut WebSocket, state: &WsState, since: i64) -> Result<(), ()> {
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
            send_envelope(socket, &envelope).await?;
            cursor = row.id;
        }
        if (n as i64) < REPLAY_PAGE {
            return Ok(());
        }
    }
}

async fn send_envelope(socket: &mut WebSocket, envelope: &EventEnvelope) -> Result<(), ()> {
    let text = serde_json::to_string(envelope).expect("envelope serializes");
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}

/// `id` (BIGINT, always ≥ 1 for a real row) → `seq`. Negatives cannot occur for
/// an identity column; clamped defensively rather than wrapping.
fn seq_of(id: i64) -> u64 {
    u64::try_from(id).unwrap_or(0)
}

/// A transient event has no stored timestamp — its occurrence *is* now.
fn now_rfc3339() -> String {
    rfc3339(OffsetDateTime::now_utc())
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).expect("UTC timestamp formats")
}

/// Split a `#[serde(tag = "type")]` value into its discriminator and the
/// remaining fields, so the envelope carries the type and the payload carries
/// only the event's own fields (matching the outbox payload convention).
fn split_tagged(value: serde_json::Value) -> (String, serde_json::Value) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::run::{RunOutcome, RunOutcomeKind, RunState};
    use serde_json::json;

    const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    #[tokio::test]
    async fn publish_builds_a_domain_envelope_carrying_the_outbox_seq() {
        let hub = WsHub::new();
        let mut rx = hub.subscribe();

        hub.publish(&OutboxRecord {
            id: 42,
            event_type: "run.state_changed".to_owned(),
            payload: json!({ "runId": RUN, "state": "model_running" }),
            created_at: OffsetDateTime::UNIX_EPOCH,
        })
        .await
        .unwrap();

        let env = rx.recv().await.unwrap();
        assert_eq!(env.seq, 42);
        // occurredAt reflects the stored commit time, not "now".
        assert_eq!(env.occurred_at, "1970-01-01T00:00:00Z");
        assert_eq!(env.v, CONTRACT_VERSION);
        assert_eq!(env.channel, Channel::Session);
        assert_eq!(env.event_type, "run.state_changed");
        // Payload forwarded verbatim; the type stays on the envelope only.
        assert_eq!(
            env.payload,
            json!({ "runId": RUN, "state": "model_running" })
        );
        assert_eq!(hub.high_water(), 42);
    }

    #[tokio::test]
    async fn sink_broadcasts_deltas_and_drops_state_and_finished() {
        let hub = WsHub::new();
        let mut rx = hub.subscribe();
        let run_id: RunId = RUN.parse().unwrap();

        // State + finished are owned by the outbox path — dropped here.
        hub.emit(RunUpdate::StateChanged {
            run_id: run_id.clone(),
            state: RunState::ModelRunning,
        })
        .await;
        hub.emit(RunUpdate::Finished {
            run_id: run_id.clone(),
            outcome: RunOutcome {
                kind: RunOutcomeKind::Completed,
                detail: None,
            },
        })
        .await;
        // Only the transient delta is broadcast.
        hub.emit(RunUpdate::TextDelta {
            run_id: run_id.clone(),
            text: "hi".to_owned(),
        })
        .await;

        let env = rx.recv().await.unwrap();
        assert_eq!(env.event_type, "text.delta");
        assert_eq!(env.payload, json!({ "runId": RUN, "text": "hi" }));
        assert!(
            rx.try_recv().is_err(),
            "state/finished must not be broadcast"
        );
    }

    #[test]
    fn split_tagged_moves_the_type_out_of_the_payload() {
        let (event_type, payload) =
            split_tagged(json!({ "type": "text.delta", "runId": RUN, "text": "x" }));
        assert_eq!(event_type, "text.delta");
        assert_eq!(payload, json!({ "runId": RUN, "text": "x" }));
    }

    #[test]
    fn seq_clamps_a_nonpositive_id() {
        assert_eq!(seq_of(7), 7);
        assert_eq!(seq_of(0), 0);
        assert_eq!(seq_of(-1), 0);
    }
}
