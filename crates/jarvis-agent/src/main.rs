#![deny(unsafe_code)]
//! Desktop agent binary (docs/02 §8/§12, FR-09/10): a paired `display`-channel
//! client that places Jarvis surfaces on monitors via Hyprland. It exposes a
//! narrow, closed command set — **it is not a shell**.
//!
//! Configuration is environment-only (no secrets on the command line, invariant
//! 5):
//!
//! * `JARVIS_AGENT_WS_URL` — jarvisd WebSocket, e.g. `ws://127.0.0.1:8741/ws/v1`
//! * `JARVIS_AGENT_TOKEN` — the paired device bearer token (secret; never logged)
//!
//! Hyprland is discovered from `XDG_RUNTIME_DIR` + `HYPRLAND_INSTANCE_SIGNATURE`.

mod client;
mod compositor;
mod handler;

use anyhow::Context;

use crate::compositor::HyprctlClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let ws_url = std::env::var("JARVIS_AGENT_WS_URL")
        .context("JARVIS_AGENT_WS_URL is required (e.g. ws://127.0.0.1:8741/ws/v1)")?;
    // The token is a secret: read it, hold it locally, never trace it.
    let token = std::env::var("JARVIS_AGENT_TOKEN")
        .context("JARVIS_AGENT_TOKEN is required (paired device bearer token)")?;

    let compositor = HyprctlClient::from_env().context(
        "not running under Hyprland (XDG_RUNTIME_DIR / HYPRLAND_INSTANCE_SIGNATURE unset)",
    )?;

    // Ctrl-C / SIGTERM flips the shutdown watch so the client loop drains.
    // Intentionally detached, untracked work (invariant 4): a process-lifetime
    // signal listener with nothing to drain — it self-terminates on the first
    // signal and the awaited client loop below is the real shutdown join point.
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = tx.send(true);
        }
    });

    tracing::info!(%ws_url, "jarvis-agent starting");
    client::run(&ws_url, &token, &compositor, rx).await
}
