//! The agent's connection to jarvisd (docs/05 §1): a paired client on
//! `/ws/v1`. It receives display directives and applies them to the compositor;
//! it sends nothing back in this slice (monitor-inventory reporting is
//! deferred). Session/voice-channel frames are ignored — this device acts on
//! the `display` channel only.
//!
//! Two things F8.1 adds to the M3a loop: the connection is made over **pinned**
//! TLS when the node paired against an HTTPS daemon (ADR-031 §4), and the loop
//! **reconnects with backoff** instead of returning after one socket — a
//! kitchen satellite outlives any single TCP connection, and a daemon restart
//! must not require a human.
//!
//! The one thing it must *not* do is retry forever against a daemon that has
//! told it to go away. Revocation is terminal: it is not an error to recover
//! from, it is the owner's decision (docs/05 §6.4).
//!
//! The pure decode step and the outcome classification are unit-tested below;
//! the socket loop is covered end to end by `tests/node_session.rs` against a
//! real TLS listener.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt;
use jarvis_contracts::display::DisplayDirective;
use jarvis_contracts::envelope::{Channel, EventEnvelope};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};

/// Inbound frame ceiling for the agent, mirroring jarvisd's own 64 KiB cap
/// (`jarvisd::ws`). Directives are tiny; a trusted peer has no legitimate large
/// inbound payload — DoS hardening for symmetry.
const MAX_INBOUND_FRAME_BYTES: usize = 64 * 1024;

/// jarvisd closes a revoked device's socket with 1008 "policy violation"
/// (`jarvisd::ws::REVOKED_CLOSE_CODE`). The node treats it as terminal.
const REVOKED_CLOSE_CODE: u16 = 1008;

/// Reconnect backoff: quick enough that a daemon restart is invisible, capped
/// low enough that a satellite is not a poll loop against a dead host
/// (low-power rule 2).
const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

use crate::compositor::Compositor;
use crate::handler;
use crate::pinning;
use crate::store::Credentials;

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

/// How a single socket ended, which is what decides whether there is a next one.
#[derive(Debug, PartialEq, Eq)]
pub enum SessionOutcome {
    /// The owner asked the process to stop.
    Shutdown,
    /// The daemon revoked this device. Terminal: re-pairing is the only way
    /// back (ADR-031 consequences), so retrying is pure noise.
    Revoked,
    /// Anything else — daemon restart, network blip, cable. Reconnect.
    Disconnected,
}

/// The `wss://…/ws/v1` (or `ws://`) URL for a daemon base URL.
pub fn ws_url_for(server_url: &str) -> anyhow::Result<String> {
    let base = server_url.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        anyhow::bail!("server URL must start with https:// or http://");
    };
    Ok(format!("{base}/ws/v1"))
}

/// Classifies a tungstenite failure into "try again" or "stop".
///
/// A handshake rejected with 401/403 means the token is no longer authority —
/// revoked, or a daemon that has forgotten this device. Either way the node
/// cannot fix it by asking again, and a satellite hammering a daemon that keeps
/// saying no is exactly the failure mode backoff is supposed to prevent.
fn classify(error: &tokio_tungstenite::tungstenite::Error) -> SessionOutcome {
    use tokio_tungstenite::tungstenite::Error;
    match error {
        Error::Http(response) => {
            let status = response.status().as_u16();
            if status == 401 || status == 403 {
                SessionOutcome::Revoked
            } else {
                SessionOutcome::Disconnected
            }
        }
        _ => SessionOutcome::Disconnected,
    }
}

/// Connect once and pump directives until the socket ends.
pub async fn connect_once<C: Compositor>(
    credentials: &Credentials,
    compositor: &C,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<SessionOutcome> {
    let url = ws_url_for(&credentials.server_url)?;
    let mut request = url
        .as_str()
        .into_client_request()
        .context("invalid jarvisd WebSocket URL")?;
    // The token is a secret: it goes in the header and is never traced.
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", credentials.device_token)
            .parse()
            .context("token is not a valid header value")?,
    );
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_INBOUND_FRAME_BYTES))
        .max_frame_size(Some(MAX_INBOUND_FRAME_BYTES));

    // The pin, applied. A node that paired over TLS refuses to connect over
    // anything else, and refuses any certificate but the one it pinned.
    let connector = match (
        credentials.is_tls(),
        credentials.server_fingerprint.as_deref(),
    ) {
        (true, Some(fingerprint)) => Some(Connector::Rustls(Arc::new(pinning::pinned_config(
            fingerprint,
        )))),
        (true, None) => anyhow::bail!(
            "this node paired over TLS but stored no fingerprint; re-pair rather than \
             connecting unpinned"
        ),
        (false, _) => None,
    };

    // Make the connect itself cancellable (invariant 4): a stuck TCP connect must
    // still yield to shutdown rather than hang on the OS timeout.
    let connected = tokio::select! {
        _ = shutdown.changed() => return Ok(SessionOutcome::Shutdown),
        connected = connect_async_tls_with_config(request, Some(config), false, connector) => connected,
    };
    let (mut socket, _resp) = match connected {
        Ok(connected) => connected,
        Err(e) => {
            let outcome = classify(&e);
            if outcome == SessionOutcome::Revoked {
                tracing::error!("daemon refused this device's token; it has been revoked");
            } else {
                tracing::warn!(error = %e, "connecting to jarvisd /ws/v1 failed");
            }
            return Ok(outcome);
        }
    };
    tracing::info!("connected to jarvisd; listening for display directives");

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                // A send-side drop or a `true` value ends the loop.
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(SessionOutcome::Shutdown);
                }
            }
            frame = socket.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => dispatch(&text, compositor).await,
                Some(Ok(WsMessage::Close(Some(frame)))) => {
                    return Ok(if u16::from(frame.code) == REVOKED_CLOSE_CODE {
                        tracing::error!(reason = %frame.reason, "daemon closed the socket: this device was revoked");
                        SessionOutcome::Revoked
                    } else {
                        tracing::info!(code = %u16::from(frame.code), "daemon closed the socket");
                        SessionOutcome::Disconnected
                    });
                }
                Some(Ok(WsMessage::Close(None))) | None => return Ok(SessionOutcome::Disconnected),
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "websocket error; closing");
                    return Ok(SessionOutcome::Disconnected);
                }
            },
        }
    }
}

/// Connect, and keep reconnecting, until shutdown or revocation.
///
/// Returns `true` if the node was revoked — the caller turns that into a clean
/// exit with a message the owner can act on.
pub async fn run<C: Compositor>(
    credentials: &Credentials,
    compositor: &C,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<bool> {
    let mut shutdown = shutdown;
    let mut backoff = BACKOFF_INITIAL;
    loop {
        match connect_once(credentials, compositor, &mut shutdown).await? {
            SessionOutcome::Shutdown => return Ok(false),
            SessionOutcome::Revoked => return Ok(true),
            SessionOutcome::Disconnected => {}
        }
        tracing::info!(seconds = backoff.as_secs(), "reconnecting after backoff");
        tokio::select! {
            _ = shutdown.changed() => return Ok(false),
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
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
        } = directive
        else {
            panic!("expected a place_surface directive, got {directive:?}");
        };
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
