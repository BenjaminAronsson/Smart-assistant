//! Desktop display surfaces and profiles (FR-09/10, docs/02 §8/§12).
//!
//! Pure value types: the server maintains a set of logical UI **surfaces** and a
//! **display profile** maps each to a physical monitor. `jarvis-agent` executes
//! the placement over Hyprland; every OS/IPC detail lives in that binary, never
//! here (invariant 3 — this module does no I/O and pulls in no OS crate).
//!
//! The placement decision (which surface goes on which monitor) is domain logic
//! and is unit-tested here; the wire directive that carries it to the agent is a
//! `jarvis-contracts` DTO, and the audit of the placement is written by jarvisd.

use std::fmt;

/// A logical UI surface the server maintains (docs/02 §8). The HUD (M3b) and the
/// M1/M2 ops surfaces render these; a [`DisplayProfile`] pins each to a monitor.
///
/// Exhaustive and closed: the agent's narrow command set targets exactly these
/// windows by their stable app-id, so a `match` on `Surface` must stay
/// exhaustive (no `_` arm) — a new surface is a deliberate, reviewed addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Surface {
    /// Chat / conversation view (M1 front face, ops layer in M3b).
    Conversation,
    /// Run timeline / run spine.
    RunTimeline,
    /// Approval tray (F2.5).
    ApprovalTray,
    /// Artifact canvas — where F3a artifacts render (exit evidence #2 targets this).
    ArtifactCanvas,
    /// Ambient status / presence.
    AmbientStatus,
    /// Diagnostics / health.
    Diagnostics,
}

impl Surface {
    /// The stable Chromium app-mode **app-id** for this surface's window. The
    /// agent launches and moves windows by this id, so it is a load-bearing
    /// contract with the shell launcher and must never change for a given
    /// surface. Reverse-DNS-ish, kebab-case, `jarvis.<surface>`.
    pub fn app_id(self) -> &'static str {
        match self {
            Surface::Conversation => "jarvis.conversation",
            Surface::RunTimeline => "jarvis.run-timeline",
            Surface::ApprovalTray => "jarvis.approval-tray",
            Surface::ArtifactCanvas => "jarvis.artifact-canvas",
            Surface::AmbientStatus => "jarvis.ambient-status",
            Surface::Diagnostics => "jarvis.diagnostics",
        }
    }

    /// Every surface, for iteration in profiles/tests. Kept in sync with the enum
    /// by the exhaustive `app_id` match above (a new variant fails that match).
    pub const ALL: [Surface; 6] = [
        Surface::Conversation,
        Surface::RunTimeline,
        Surface::ApprovalTray,
        Surface::ArtifactCanvas,
        Surface::AmbientStatus,
        Surface::Diagnostics,
    ];
}

/// A physical monitor, named by its compositor **connector** (e.g. `DP-1`,
/// `eDP-1`, `HDMI-A-1`). This is an OS-assigned name, not a ULID — the domain
/// only validates that it is a non-empty single-line token; the agent verifies
/// the monitor actually exists before placing (fail-closed) and never guesses.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MonitorId(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MonitorIdError {
    #[error("monitor id must not be empty")]
    Empty,
    #[error("monitor id must be a single line without control characters")]
    Malformed,
}

impl MonitorId {
    /// Validate and construct. Trims surrounding whitespace; rejects an empty
    /// result or any control/newline character (a connector name is a short
    /// single-line token — this keeps an attacker-supplied `display` field from
    /// smuggling a newline into a Hyprland dispatch line at the agent).
    pub fn new(raw: impl Into<String>) -> Result<Self, MonitorIdError> {
        let trimmed = raw.into().trim().to_owned();
        if trimmed.is_empty() {
            return Err(MonitorIdError::Empty);
        }
        if trimmed.chars().any(|c| c.is_control()) {
            return Err(MonitorIdError::Malformed);
        }
        Ok(Self(trimmed))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MonitorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Maps logical surfaces to monitors (docs/02 §8 "a display profile maps
/// surfaces to monitor/workspace"). Single-machine, multi-monitor only in M3
/// (distributed nodes are M7). Editing/reviewing the profile is a later UI
/// concern; here it is an immutable resolved mapping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisplayProfile {
    assignments: Vec<(Surface, MonitorId)>,
}

impl DisplayProfile {
    /// Build from assignments. A later duplicate for the same surface wins (last
    /// write), so config layering is predictable.
    pub fn new(assignments: impl IntoIterator<Item = (Surface, MonitorId)>) -> Self {
        let mut profile = Self::default();
        for (surface, monitor) in assignments {
            profile.set(surface, monitor);
        }
        profile
    }

    fn set(&mut self, surface: Surface, monitor: MonitorId) {
        if let Some(slot) = self.assignments.iter_mut().find(|(s, _)| *s == surface) {
            slot.1 = monitor;
        } else {
            self.assignments.push((surface, monitor));
        }
    }

    /// The monitor a surface is pinned to by this profile, if any.
    pub fn monitor_for(&self, surface: Surface) -> Option<&MonitorId> {
        self.assignments
            .iter()
            .find(|(s, _)| *s == surface)
            .map(|(_, m)| m)
    }

    /// Resolve the placement target for a surface. An explicit per-request
    /// `requested` monitor (e.g. from `POST …/open {display}`) wins over the
    /// profile default. Returns `None` when neither names a monitor — the caller
    /// must then fail closed (never place on an arbitrary monitor).
    pub fn resolve(
        &self,
        surface: Surface,
        requested: Option<MonitorId>,
    ) -> Option<SurfacePlacement> {
        let monitor = requested.or_else(|| self.monitor_for(surface).cloned())?;
        Some(SurfacePlacement { surface, monitor })
    }
}

/// A resolved decision: place `surface` on `monitor`. Produced by
/// [`DisplayProfile::resolve`] and carried to the agent as a directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfacePlacement {
    pub surface: Surface,
    pub monitor: MonitorId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ids_are_stable_distinct_and_defined_for_every_surface() {
        let ids: Vec<&str> = Surface::ALL.iter().map(|s| s.app_id()).collect();
        // Distinct: no two surfaces share a window app-id.
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            ids.len(),
            "app-ids must be unique per surface"
        );
        // Stable naming contract (a rename here breaks the shell launcher).
        assert_eq!(Surface::ArtifactCanvas.app_id(), "jarvis.artifact-canvas");
    }

    #[test]
    fn monitor_id_trims_and_rejects_empty_or_control() {
        assert_eq!(MonitorId::new("  DP-1 ").unwrap().as_str(), "DP-1");
        assert_eq!(MonitorId::new("   ").unwrap_err(), MonitorIdError::Empty);
        assert_eq!(MonitorId::new("").unwrap_err(), MonitorIdError::Empty);
        // A newline must not survive into a Hyprland dispatch line.
        assert_eq!(
            MonitorId::new("DP-1\ndispatch exec danger").unwrap_err(),
            MonitorIdError::Malformed
        );
    }

    #[test]
    fn requested_monitor_overrides_the_profile_default() {
        let profile =
            DisplayProfile::new([(Surface::ArtifactCanvas, MonitorId::new("eDP-1").unwrap())]);
        let requested = MonitorId::new("DP-1").unwrap();
        let placement = profile
            .resolve(Surface::ArtifactCanvas, Some(requested.clone()))
            .expect("explicit request always resolves");
        assert_eq!(placement.surface, Surface::ArtifactCanvas);
        assert_eq!(placement.monitor, requested);
    }

    #[test]
    fn profile_default_used_when_no_request() {
        let profile =
            DisplayProfile::new([(Surface::ArtifactCanvas, MonitorId::new("eDP-1").unwrap())]);
        let placement = profile
            .resolve(Surface::ArtifactCanvas, None)
            .expect("profile default resolves");
        assert_eq!(placement.monitor.as_str(), "eDP-1");
    }

    #[test]
    fn unresolvable_surface_returns_none_so_caller_fails_closed() {
        let profile = DisplayProfile::default();
        assert!(profile.resolve(Surface::ArtifactCanvas, None).is_none());
    }

    #[test]
    fn last_assignment_for_a_surface_wins() {
        let profile = DisplayProfile::new([
            (Surface::ArtifactCanvas, MonitorId::new("eDP-1").unwrap()),
            (Surface::ArtifactCanvas, MonitorId::new("DP-2").unwrap()),
        ]);
        assert_eq!(
            profile
                .monitor_for(Surface::ArtifactCanvas)
                .unwrap()
                .as_str(),
            "DP-2"
        );
    }
}
