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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::FakeCompositor;
    use jarvis_contracts::display::SurfaceDto;

    fn place(app_id: &str, monitor: &str) -> DisplayDirective {
        DisplayDirective::PlaceSurface {
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
