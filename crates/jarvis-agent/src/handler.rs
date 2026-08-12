//! Directive handling — the agent's tested decision core (invariant 1).
//!
//! A [`DisplayDirective`] arriving from jarvisd is data, not authority: before
//! touching the compositor the agent re-validates it against its own rules
//! (defense in depth — even though jarvisd is trusted, the agent is the process
//! that actually holds OS window control):
//!
//! * the target `app_id` must be in the `jarvis.` namespace — the agent moves
//!   only its own surfaces, never an arbitrary window;
//! * the target monitor must actually exist (fail closed — never place on a
//!   guessed monitor);
//! * `app_id`/`monitor` must be single-line tokens (no control characters that
//!   could smuggle a second dispatch command).
//!
//! Only then does it issue the narrow compositor command.

use jarvis_contracts::display::DisplayDirective;

use crate::compositor::{Compositor, CompositorError};

/// The `jarvis.` app-id namespace the agent will place. Placement is broader
/// than launch (any jarvis surface may be moved), so it is a prefix check rather
/// than the exact launch allowlist.
const SURFACE_APP_PREFIX: &str = "jarvis.";

#[derive(Debug, PartialEq, Eq)]
pub enum HandleError {
    /// The app-id is outside the jarvis surface namespace — refused.
    ForeignAppId(String),
    /// A field carried a control character — refused before it reaches a dispatch.
    Malformed(&'static str),
    /// The named monitor is not connected.
    UnknownMonitor(String),
    /// The compositor command itself failed.
    Compositor(String),
}

impl std::fmt::Display for HandleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandleError::ForeignAppId(a) => write!(f, "refusing to place foreign app-id {a:?}"),
            HandleError::Malformed(field) => write!(f, "directive field {field} is malformed"),
            HandleError::UnknownMonitor(m) => write!(f, "monitor {m:?} is not connected"),
            HandleError::Compositor(e) => write!(f, "compositor: {e}"),
        }
    }
}

impl std::error::Error for HandleError {}

fn is_single_line_token(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(char::is_control)
}

/// Apply one directive against the compositor after full validation.
pub async fn apply(
    directive: &DisplayDirective,
    compositor: &impl Compositor,
) -> Result<(), HandleError> {
    match directive {
        DisplayDirective::PlaceSurface {
            surface: _,
            app_id,
            monitor,
            // Addressing is enforced server-side (F7.5): the agent only ever
            // receives directives meant for it, so this is context for logs
            // rather than a check the agent could be trusted to make.
            target_device_id: _,
        } => {
            if !is_single_line_token(app_id) {
                return Err(HandleError::Malformed("appId"));
            }
            if !is_single_line_token(monitor) {
                return Err(HandleError::Malformed("monitor"));
            }
            if !app_id.starts_with(SURFACE_APP_PREFIX) {
                return Err(HandleError::ForeignAppId(app_id.clone()));
            }

            // Fail closed on an unknown monitor: verify against what the
            // compositor actually reports before dispatching a move.
            let monitors = compositor
                .list_monitors()
                .await
                .map_err(|e| HandleError::Compositor(e.to_string()))?;
            if !monitors.iter().any(|m| m.name == *monitor) {
                return Err(HandleError::UnknownMonitor(monitor.clone()));
            }

            compositor
                .place_window(app_id, monitor)
                .await
                .map_err(|e: CompositorError| HandleError::Compositor(e.to_string()))
        }

        DisplayDirective::OpenMediaUrl { url, monitor } => {
            // This directive launches a process, so the agent re-validates
            // everything itself rather than trusting the sender (defense in
            // depth — jarvisd already checked, but the agent holds the OS
            // capability).
            if !is_single_line_token(url) {
                return Err(HandleError::Malformed("url"));
            }
            if !is_single_line_token(monitor) {
                return Err(HandleError::Malformed("monitor"));
            }
            // https only. Not merely "no file://": an `http://` cast would send
            // the request in the clear, and any other scheme (`javascript:`,
            // `data:`, `chrome://`) has no business reaching a browser argv.
            if !is_https_url(url) {
                return Err(HandleError::Malformed("url"));
            }
            if url.len() > MAX_MEDIA_URL_BYTES {
                return Err(HandleError::Malformed("url"));
            }

            let monitors = compositor
                .list_monitors()
                .await
                .map_err(|e| HandleError::Compositor(e.to_string()))?;
            if !monitors.iter().any(|m| m.name == *monitor) {
                return Err(HandleError::UnknownMonitor(monitor.clone()));
            }

            compositor
                .open_media_window(url)
                .await
                .map_err(|e: CompositorError| HandleError::Compositor(e.to_string()))?;
            // Place it on the requested monitor. A launch that cannot be placed
            // is not a failure of the cast itself — the video is playing — so a
            // placement error is reported but the window stays open.
            compositor
                .place_window(MEDIA_APP_ID, monitor)
                .await
                .map_err(|e: CompositorError| HandleError::Compositor(e.to_string()))
        }
    }
}

/// The media window's app-id — mirrors `jarvis_domain::display::Surface::
/// MediaWindow::app_id()`. Duplicated as a constant (rather than depending on
/// the domain crate) because the agent must not reach past `jarvis-contracts`
/// (arch-test rule). Both copies of the literal are pinned by tests — here in
/// `casts_an_https_url_into_the_media_window_and_places_it`, and domain-side in
/// `display::tests::app_ids_are_stable_distinct_and_defined_for_every_surface`.
const MEDIA_APP_ID: &str = "jarvis.media";

/// Longest media URL accepted. Real video URLs are well under this; the bound
/// keeps an oversized argv out of a process launch.
const MAX_MEDIA_URL_BYTES: usize = 2048;

/// `https`-only scheme check, ASCII-case-insensitive. Mirrors
/// `jarvis_domain::media::is_https_url` (the agent may not depend on that crate
/// — arch rule). Compares **bytes**: a string-slice index panics when byte 8
/// falls inside a multi-byte character, and this input is attacker-influenced.
fn is_https_url(url: &str) -> bool {
    const PREFIX: &[u8] = b"https://";
    url.len() > PREFIX.len() && url.as_bytes()[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::FakeCompositor;
    use jarvis_contracts::display::SurfaceDto;

    fn place(app_id: &str, monitor: &str) -> DisplayDirective {
        DisplayDirective::PlaceSurface {
            target_device_id: None,
            surface: SurfaceDto::ArtifactCanvas,
            app_id: app_id.to_owned(),
            monitor: monitor.to_owned(),
        }
    }

    #[tokio::test]
    async fn places_a_jarvis_surface_on_a_connected_monitor() {
        let comp = FakeCompositor::with_monitors(&["eDP-1", "DP-1"]);
        apply(&place("jarvis.artifact-canvas", "DP-1"), &comp)
            .await
            .unwrap();
        assert_eq!(
            *comp.placements.lock().unwrap(),
            vec![("jarvis.artifact-canvas".to_owned(), "DP-1".to_owned())]
        );
    }

    #[tokio::test]
    async fn refuses_a_foreign_app_id() {
        let comp = FakeCompositor::with_monitors(&["DP-1"]);
        let err = apply(&place("firefox", "DP-1"), &comp).await.unwrap_err();
        assert!(matches!(err, HandleError::ForeignAppId(_)));
        assert!(comp.placements.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn fails_closed_on_an_unknown_monitor() {
        let comp = FakeCompositor::with_monitors(&["eDP-1"]);
        let err = apply(&place("jarvis.artifact-canvas", "DP-9"), &comp)
            .await
            .unwrap_err();
        assert_eq!(err, HandleError::UnknownMonitor("DP-9".to_owned()));
        assert!(comp.placements.lock().unwrap().is_empty());
    }

    fn open(url: &str, monitor: &str) -> DisplayDirective {
        DisplayDirective::OpenMediaUrl {
            url: url.to_owned(),
            monitor: monitor.to_owned(),
        }
    }

    #[tokio::test]
    async fn casts_an_https_url_into_the_media_window_and_places_it() {
        let comp = FakeCompositor::with_monitors(&["DP-1"]);
        apply(&open("https://www.youtube.com/watch?v=abc", "DP-1"), &comp)
            .await
            .unwrap();

        assert_eq!(
            *comp.opened.lock().unwrap(),
            vec!["https://www.youtube.com/watch?v=abc".to_owned()]
        );
        // Placed by the media window's own app-id, never a caller-supplied one.
        assert_eq!(
            *comp.placements.lock().unwrap(),
            vec![("jarvis.media".to_owned(), "DP-1".to_owned())]
        );
    }

    #[tokio::test]
    async fn refuses_every_non_https_scheme() {
        // The launch path must never accept a scheme that reads a local file,
        // executes script, or sends the request in the clear.
        let comp = FakeCompositor::with_monitors(&["DP-1"]);
        for hostile in [
            "file:///etc/passwd",
            "http://example.com/video",
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "chrome://settings",
            "https://",
            "",
            "  https://example.com",
            // Multi-byte at the scheme boundary: reject, never panic.
            "https:/\u{20ac}evil.example/x",
            "https:/\u{20ac}",
        ] {
            let err = apply(&open(hostile, "DP-1"), &comp).await.unwrap_err();
            assert_eq!(
                err,
                HandleError::Malformed("url"),
                "must refuse {hostile:?}"
            );
        }
        assert!(comp.opened.lock().unwrap().is_empty());
        assert!(comp.placements.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refuses_a_url_carrying_a_control_character_or_an_absurd_length() {
        let comp = FakeCompositor::with_monitors(&["DP-1"]);
        let err = apply(&open("https://ok.example\n--foo=bar", "DP-1"), &comp)
            .await
            .unwrap_err();
        assert_eq!(err, HandleError::Malformed("url"));

        let long = format!("https://example.com/{}", "a".repeat(4096));
        let err = apply(&open(&long, "DP-1"), &comp).await.unwrap_err();
        assert_eq!(err, HandleError::Malformed("url"));
        assert!(comp.opened.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_cast_to_an_unknown_monitor_launches_nothing() {
        // Fail closed BEFORE launching: a rejected placement must not leave a
        // stray browser window open on an arbitrary screen.
        let comp = FakeCompositor::with_monitors(&["eDP-1"]);
        let err = apply(&open("https://example.com/v", "DP-9"), &comp)
            .await
            .unwrap_err();
        assert_eq!(err, HandleError::UnknownMonitor("DP-9".to_owned()));
        assert!(comp.opened.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refuses_a_control_character_in_a_field() {
        let comp = FakeCompositor::with_monitors(&["DP-1"]);
        let err = apply(
            &place("jarvis.artifact-canvas", "DP-1\ndispatch exec x"),
            &comp,
        )
        .await
        .unwrap_err();
        assert_eq!(err, HandleError::Malformed("monitor"));
        assert!(comp.placements.lock().unwrap().is_empty());
    }
}
