use super::replay::*;
use std::sync::Arc;

use async_trait::async_trait;
use jarvis_application::orchestrator::{RunEventSink, RunUpdate};
use jarvis_application::ports::DisplayDirectiveSink;
use jarvis_contracts::CONTRACT_VERSION;
use jarvis_contracts::display::DisplayDirective;
use jarvis_contracts::envelope::{Channel, EventEnvelope};
use jarvis_infra::dispatcher::{OutboxPublisher, OutboxRecord, PublishError};

use super::hub::WsHub;

/// Deep-dive turns and list commands publish canvas instructions through this
/// impl (F3b.6).
impl crate::cards::CanvasSink for WsHub {
    fn publish(&self, canvas: jarvis_contracts::deepdive::HudCanvasDto) {
        self.broadcast_hud_canvas(canvas);
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
    async fn dispatch(
        &self,
        placement: &jarvis_domain::display::SurfacePlacement,
        target: Option<&str>,
    ) -> bool {
        self.broadcast_display(placement, target)
    }
}

/// Cast-a-link dispatch (F3a.7, ADR-012): the media window's URL rides the same
/// display channel as a placement. The URL was validated (`https`, bounded, no
/// control characters) by the tool before it got here, and the agent validates
/// it again before launching anything.
#[async_trait]
impl jarvis_application::ports::MediaWindowSink for WsHub {
    async fn open_url(
        &self,
        url: &str,
        monitor: &jarvis_domain::display::MonitorId,
        target: Option<&str>,
    ) -> bool {
        let directive = DisplayDirective::OpenMediaUrl {
            url: url.to_owned(),
            monitor: monitor.as_str().to_owned(),
            target_device_id: target.map(ToOwned::to_owned),
        };
        let (event_type, payload) =
            split_tagged(serde_json::to_value(&directive).expect("directive serializes"));
        let envelope = EventEnvelope {
            v: CONTRACT_VERSION,
            seq: self.high_water(),
            channel: Channel::Display,
            event_type,
            occurred_at: now_rfc3339(),
            trace_id: None,
            resource_version: None,
            payload,
        };
        self.tx.send(Arc::new(envelope)).is_ok()
    }
}

/// The orchestrator emits run updates through this impl.
#[async_trait]
impl RunEventSink for WsHub {
    async fn emit(&self, update: RunUpdate) {
        match update {
            RunUpdate::TextDelta { run_id, text } => self.broadcast_delta(&run_id, &text),
            RunUpdate::Queued {
                run_id,
                reason,
                position,
            } => self.broadcast_queued(&run_id, &reason, position),
            RunUpdate::Agenda { run_id, events } => self.broadcast_agenda(&run_id, events),
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
