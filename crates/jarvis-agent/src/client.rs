//! The agent's connection to jarvisd (docs/05 §1): a paired `display`-channel
//! client on `/ws/v1`. It receives display directives and applies them to the
//! compositor; it sends nothing back in this slice (monitor-inventory reporting
//! is deferred). Session/voice-channel frames are ignored — this device acts on
//! the `display` channel only.
//!
//! The socket connect/read loop is exercised manually against a running jarvisd
//! (CI has no live socket); the pure decode step is unit-tested below.

use anyhow::Context;
use futures_util::StreamExt;
use jarvis_contracts::display::DisplayDirective;
use jarvis_contracts::envelope::{Channel, EventEnvelope};
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// Inbound frame ceiling for the agent, mirroring jarvisd's own 64 KiB cap
/// (`jarvisd::ws`). Directives are tiny; a trusted peer has no legitimate large
/// inbound payload — DoS hardening for symmetry.
const MAX_INBOUND_FRAME_BYTES: usize = 64 * 1024;

use crate::compositor::Compositor;
use crate::handler;

/// Decode a raw `/ws/v1` frame into a display directive, or `None` if the frame
/// is not a display-channel directive we act on (other channels, or a directive
/// tag this agent version does not know — logged by the caller, never a panic).
///
/// The hub splits the `type` discriminator onto the envelope and leaves the
/// directive's own fields in `payload`; we merge them back before decoding.
pub fn decode_directive(text: &str) -> anyhow::Result<Option<DisplayDirective>> {
    let envelope: EventEnvelope =
        serde_json::from_str(text).context("frame is not a valid event envelope")?;
    if envelope.channel != Channel::Display {
        return Ok(None);
    }
    let mut value = envelope.payload;
    let obj = value
        .as_object_mut()
        .context("display payload is not an object")?;
    obj.insert(
        "type".to_owned(),
        serde_json::Value::String(envelope.event_type),
    );
    // An unknown display directive tag is not fatal to the connection — decode
    // failures return None so the loop keeps running (forward compatibility).
    Ok(serde_json::from_value::<DisplayDirective>(value).ok())
}

/// Connect to jarvisd and apply display directives until the socket closes or
/// `shutdown` fires. `token` is the paired device bearer token (a secret — never
/// logged).
pub async fn run<C: Compositor>(
    ws_url: &str,
    token: &str,
    compositor: &C,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut request = ws_url
        .into_client_request()
        .context("invalid jarvisd WebSocket URL")?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}")
            .parse()
            .context("token is not a valid header value")?,
    );
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_INBOUND_FRAME_BYTES))
        .max_frame_size(Some(MAX_INBOUND_FRAME_BYTES));

    let mut shutdown = shutdown;
    // Make the connect itself cancellable (invariant 4): a stuck TCP connect must
    // still yield to shutdown rather than hang on the OS timeout.
    let (mut socket, _resp) = tokio::select! {
        _ = shutdown.changed() => return Ok(()),
        connected = connect_async_with_config(request, Some(config), false) => {
            connected.context("connecting to jarvisd /ws/v1")?
        }
    };
    tracing::info!("connected to jarvisd; listening for display directives");
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                // A send-side drop or a `true` value ends the loop.
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            frame = socket.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => dispatch(&text, compositor).await,
                Some(Ok(WsMessage::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "websocket error; closing");
                    break;
                }
            },
        }
    }
    Ok(())
}

async fn dispatch<C: Compositor>(text: &str, compositor: &C) {
    match decode_directive(text) {
        Ok(Some(directive)) => {
            if let Err(e) = handler::apply(&directive, compositor).await {
                // A refused or failed directive is logged, never fatal — the
                // agent stays connected for the next one.
                tracing::warn!(error = %e, "display directive not applied");
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "undecodable frame ignored"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_place_surface_directive_on_the_display_channel() {
        // Shape a hub-style envelope: type on the envelope, fields in payload.
        let frame = serde_json::json!({
            "v": 1,
            "seq": 3,
            "channel": "display",
            "type": "display.place_surface",
            "occurredAt": "2026-07-22T00:00:00Z",
            "payload": {
                "surface": "artifact_canvas",
                "appId": "jarvis.artifact-canvas",
                "monitor": "DP-1"
            }
        })
        .to_string();

        let directive = decode_directive(&frame).unwrap().unwrap();
        let DisplayDirective::PlaceSurface {
            app_id, monitor, ..
        } = directive;
        assert_eq!(app_id, "jarvis.artifact-canvas");
        assert_eq!(monitor, "DP-1");
    }

    #[test]
    fn ignores_a_non_display_channel_frame() {
        let frame = serde_json::json!({
            "v": 1, "seq": 1, "channel": "session",
            "type": "text.delta", "occurredAt": "2026-07-22T00:00:00Z",
            "payload": { "runId": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "text": "hi" }
        })
        .to_string();
        assert!(decode_directive(&frame).unwrap().is_none());
    }

    #[test]
    fn an_unknown_display_directive_tag_is_none_not_an_error() {
        let frame = serde_json::json!({
            "v": 1, "seq": 1, "channel": "display",
            "type": "display.some_future_command", "occurredAt": "2026-07-22T00:00:00Z",
            "payload": { "foo": "bar" }
        })
        .to_string();
        assert!(decode_directive(&frame).unwrap().is_none());
    }
}
