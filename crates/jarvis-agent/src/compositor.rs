//! The compositor boundary (docs/02 §8): the *only* place `jarvis-agent` touches
//! the OS window manager. Everything above it works against the [`Compositor`]
//! trait, so the directive-handling logic is unit-tested with a fake and the real
//! Hyprland socket I/O stays a thin, replaceable adapter.
//!
//! The agent exposes a **narrow, closed** command set. In this slice that is
//! list-monitors + place-a-window-on-a-monitor (exit evidence #2). App-launch in
//! app-mode with an allowlist is part of the eventual set (docs/02 §8) but lands
//! with its own directive — the canvas window is launched by the shell until
//! then. **It is not a shell**: there is no "run arbitrary command" method here,
//! by construction, and adding one would be a reviewed contract change.

use serde::Deserialize;

/// A monitor as reported by the compositor. `name` is the connector (`DP-1`,
/// `eDP-1`) that a placement directive targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    pub name: String,
}

/// A failure talking to the compositor. Monitor-existence and app-id validation
/// are the caller's job (see `handler`), so the compositor itself only surfaces
/// IPC/protocol faults.
#[derive(Debug)]
pub struct CompositorError(String);

impl CompositorError {
    fn ipc(msg: String) -> Self {
        Self(msg)
    }
}

/// The narrow command set the agent exposes over Hyprland (docs/02 §8). Async
/// because the real implementation does socket I/O; the fake is trivial.
#[allow(async_fn_in_trait)]
pub trait Compositor {
    /// Monitors the compositor currently reports.
    async fn list_monitors(&self) -> Result<Vec<Monitor>, CompositorError>;

    /// Move the window whose app-id is `app_id` onto `monitor`. The caller has
    /// already validated that `monitor` exists and that `app_id` is a jarvis
    /// surface; this issues the compositor dispatch.
    async fn place_window(&self, app_id: &str, monitor: &str) -> Result<(), CompositorError>;
}

// --- real Hyprland client ------------------------------------------------

/// Talks to Hyprland's request socket (`$XDG_RUNTIME_DIR/hypr/$HIS/.socket.sock`)
/// using the plain-text hyprctl command protocol — no external crate, so the
/// dependency surface stays tiny (low-power). Requests are short-lived: connect,
/// write one command, read the reply, close.
///
/// The socket I/O here is exercised manually against a live Hyprland session
/// (CI has no compositor); the *decision* logic that uses it — monitor
/// verification, app-id namespacing, fail-closed — lives in `handler` and is
/// unit-tested with [`FakeCompositor`].
pub struct HyprctlClient {
    socket_path: std::path::PathBuf,
}

impl HyprctlClient {
    /// Locate the request socket from the Hyprland environment. Returns `None`
    /// when not running under Hyprland (no `HYPRLAND_INSTANCE_SIGNATURE`), so the
    /// binary can start and report the compositor as unavailable rather than
    /// panicking.
    pub fn from_env() -> Option<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
        let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
        let socket_path = std::path::Path::new(&runtime)
            .join("hypr")
            .join(&signature)
            .join(".socket.sock");
        Some(Self { socket_path })
    }

    /// How long a single hyprctl round-trip may take before it is abandoned. A
    /// hung compositor must not wedge the client loop and block graceful shutdown
    /// (invariant 4) — a timed-out request fails closed like any other IPC error.
    const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

    /// Send one hyprctl command and return the raw reply text.
    async fn request(&self, command: &str) -> Result<String, CompositorError> {
        tokio::time::timeout(Self::REQUEST_TIMEOUT, self.request_inner(command))
            .await
            .map_err(|_| CompositorError::ipc("compositor request timed out".to_owned()))?
    }

    async fn request_inner(&self, command: &str) -> Result<String, CompositorError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| CompositorError::ipc(format!("connect: {e}")))?;
        stream
            .write_all(command.as_bytes())
            .await
            .map_err(|e| CompositorError::ipc(format!("write: {e}")))?;
        stream
            .shutdown()
            .await
            .map_err(|e| CompositorError::ipc(format!("shutdown: {e}")))?;
        let mut reply = String::new();
        stream
            .read_to_string(&mut reply)
            .await
            .map_err(|e| CompositorError::ipc(format!("read: {e}")))?;
        Ok(reply)
    }
}

#[derive(Deserialize)]
struct HyprMonitor {
    name: String,
}

impl Compositor for HyprctlClient {
    async fn list_monitors(&self) -> Result<Vec<Monitor>, CompositorError> {
        // `j/monitors` returns a JSON array of monitor objects.
        let reply = self.request("j/monitors").await?;
        let monitors: Vec<HyprMonitor> = serde_json::from_str(&reply)
            .map_err(|e| CompositorError::ipc(format!("parse monitors: {e}")))?;
        Ok(monitors
            .into_iter()
            .map(|m| Monitor { name: m.name })
            .collect())
    }

    async fn place_window(&self, app_id: &str, monitor: &str) -> Result<(), CompositorError> {
        // Focus the surface's window by its app-id (Hyprland exposes the
        // app-mode app-id as the window `class`), then move the active window to
        // the target monitor. `app_id`/`monitor` are validated by the caller and
        // additionally cannot contain control characters (checked domain-side and
        // agent-side), so they are safe to place in a dispatch line.
        // TODO(F3a follow-up): these are two independent dispatches with no
        // atomicity — a focus race could move the wrong active window. Prefer a
        // single windowrule/target dispatch when the placement UX is hardened.
        self.request(&format!("dispatch focuswindow class:{app_id}"))
            .await?;
        self.request(&format!("dispatch movewindow mon:{monitor}"))
            .await?;
        Ok(())
    }
}

// --- fake for tests ------------------------------------------------------

/// In-memory compositor for unit tests: a fixed monitor list and a record of
/// placements.
#[cfg(test)]
pub struct FakeCompositor {
    pub monitors: Vec<Monitor>,
    pub placements: std::sync::Mutex<Vec<(String, String)>>,
}

#[cfg(test)]
impl FakeCompositor {
    pub fn with_monitors(names: &[&str]) -> Self {
        Self {
            monitors: names
                .iter()
                .map(|n| Monitor {
                    name: (*n).to_owned(),
                })
                .collect(),
            placements: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
impl Compositor for FakeCompositor {
    async fn list_monitors(&self) -> Result<Vec<Monitor>, CompositorError> {
        Ok(self.monitors.clone())
    }
    async fn place_window(&self, app_id: &str, monitor: &str) -> Result<(), CompositorError> {
        self.placements
            .lock()
            .unwrap()
            .push((app_id.to_owned(), monitor.to_owned()));
        Ok(())
    }
}

impl std::fmt::Display for CompositorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "compositor IPC failure: {}", self.0)
    }
}

impl std::error::Error for CompositorError {}
