//! The compositor boundary (docs/02 §8): the *only* place `jarvis-agent` touches
//! the OS window manager. Everything above it works against the [`Compositor`]
//! trait, so the directive-handling logic is unit-tested with a fake and the real
//! Hyprland socket I/O stays a thin, replaceable adapter.
//!
//! The agent exposes a **narrow, closed** command set: list-monitors,
//! place-a-window-on-a-monitor (exit evidence #2), and — since F3a.7 — launch
//! the **media window** on an `https` URL (ADR-012 cast-a-link, exit evidence
//! #4's sibling capability). The canvas window is still launched by the shell.
//!
//! **It is not a shell**: there is no "run arbitrary command" method here, by
//! construction. The one method that starts a process
//! ([`Compositor::open_media_window`]) chooses its program from a fixed
//! allowlist, passes only compile-time-constant flags plus the URL as a single
//! argv element, and never goes through a shell. Adding a method that takes a
//! caller-supplied program would be a reviewed contract change.

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

    /// Launch the **media window** on `url` (FR-22, ADR-012 cast-a-link).
    ///
    /// This is the only method in the agent that starts a process, and it is
    /// deliberately shaped so that it *cannot* become "run a command":
    ///
    /// * the program is chosen from a fixed allowlist of browser binaries — the
    ///   caller never supplies it;
    /// * every flag is a compile-time constant (`--app=`, the fixed app-id, the
    ///   dedicated credential-free profile directory);
    /// * `url` is passed as a **single argv element**, never through a shell, so
    ///   no metacharacter in it can become a second command;
    /// * the caller has already enforced `https` and rejected control characters.
    ///
    /// Launching is idempotent from the caller's point of view: relaunching with
    /// the same profile reuses the existing window (the browser routes the URL to
    /// the running instance for that user-data-dir).
    async fn open_media_window(&self, url: &str) -> Result<(), CompositorError>;
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

    async fn open_media_window(&self, url: &str) -> Result<(), CompositorError> {
        let program = media_browser().ok_or_else(|| {
            CompositorError::ipc(
                "no allowlisted browser found for the media window (chromium, \
                 google-chrome-stable, google-chrome, brave)"
                    .to_owned(),
            )
        })?;

        // Spawned directly — NOT through the compositor's `dispatch exec`, which
        // would hand a string to a shell. `Command` execs the program with an
        // argv array, so the URL cannot become a second command however it is
        // spelled.
        let mut command = tokio::process::Command::new(program);
        command
            .arg(format!("--app={url}"))
            .arg(format!("--class={}", MEDIA_APP_ID))
            .arg(format!("--user-data-dir={}", media_profile_dir().display()))
            // The media window renders third-party video and must never carry
            // the owner's browsing identity: its own profile dir is empty and
            // separate from both the shell and the browser worker (docs/02 §11a
            // "own app-id, own profile, no credentials").
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        // Detached: the media window outlives this request by design. `spawn`
        // (not `status`) so a long-lived browser never blocks the client loop.
        let mut child = command
            .spawn()
            .map_err(|e| CompositorError::ipc(format!("launching the media window: {e}")))?;
        // Do not reap: dropping the handle without waiting leaves a zombie until
        // the agent exits. Detach the wait onto the runtime instead — bounded
        // work (one browser process), and it keeps the process table clean.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        Ok(())
    }
}

/// The app-id the media window is launched with — the same id the display
/// profile places (`Surface::MediaWindow::app_id`). A constant here rather than
/// a parameter: the agent launches exactly this window and nothing else.
const MEDIA_APP_ID: &str = "jarvis.media";

/// Browser binaries the media window may be launched with, in preference order.
/// A **fixed allowlist**: the caller cannot supply a program, so this directive
/// can never become an arbitrary-execution primitive.
const MEDIA_BROWSERS: [&str; 4] = ["chromium", "google-chrome-stable", "google-chrome", "brave"];

/// First allowlisted browser present on `PATH`.
fn media_browser() -> Option<&'static str> {
    MEDIA_BROWSERS.into_iter().find(|program| {
        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
            .unwrap_or(false)
    })
}

/// The media window's dedicated, credential-free profile directory. Under the
/// user's state dir so it survives restarts (the window keeps its size/position)
/// but is separate from every other browser profile on the machine.
fn media_profile_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("jarvis/media-profile")
}

// --- fake for tests ------------------------------------------------------

/// In-memory compositor for unit tests: a fixed monitor list and a record of
/// placements.
#[cfg(test)]
pub struct FakeCompositor {
    pub monitors: Vec<Monitor>,
    pub placements: std::sync::Mutex<Vec<(String, String)>>,
    pub opened: std::sync::Mutex<Vec<String>>,
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
            opened: std::sync::Mutex::new(Vec::new()),
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
    async fn open_media_window(&self, url: &str) -> Result<(), CompositorError> {
        self.opened.lock().unwrap().push(url.to_owned());
        Ok(())
    }
}

impl std::fmt::Display for CompositorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "compositor IPC failure: {}", self.0)
    }
}

impl std::error::Error for CompositorError {}
