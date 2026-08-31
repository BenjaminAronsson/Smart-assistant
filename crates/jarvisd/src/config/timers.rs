use serde::{Deserialize, Serialize};

use super::lists::default_true;

/// `[timers]` (FR-33, ADR-023, docs/09 §1). Timers, alarms and reminders are
/// **on by default** — unlike every `[integrations.*]` section, which gates an
/// outward-facing capability. A timer reaches nothing outside this machine: it
/// reads a clock, writes a local row, and makes a noise. Requiring opt-in for
/// the most-used assistant feature would be strictness spent where there is no
/// exposure to reduce.
///
/// `alert_command` is the only thing here with any reach, and it is **owner
/// config (Z1)**: no timer name, reminder note, or model output is ever
/// interpolated into it (the WAV goes to the child's stdin, so there is no
/// argument to inject into either).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimersConfig {
    /// Set false to run with no timer surface at all: no routes, no scheduler
    /// task, nothing resident.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Playback command for the audible alert. Fed a WAV on stdin, so `aplay`,
    /// `ffplay -nodisp -autoexit -` and friends all work. A command that is not
    /// installed means the timer fires silently (logged) — never a failed fire.
    #[serde(default = "default_alert_command")]
    pub alert_command: String,
    /// Extra arguments for `alert_command`.
    #[serde(default)]
    pub alert_args: Vec<String>,
}

impl Default for TimersConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            alert_command: default_alert_command(),
            alert_args: Vec::new(),
        }
    }
}

fn default_alert_command() -> String {
    "paplay".to_owned()
}
