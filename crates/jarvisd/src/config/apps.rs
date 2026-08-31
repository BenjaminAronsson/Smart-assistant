use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `[apps]` (FR-18, docs/06 §6, ADR-027/ADR-029/ADR-030). Generated apps.
///
/// `enabled` defaults to **false**, the same opt-in stance as every other
/// capability that spawns a process: an unconfigured host registers no
/// `app.generate` tool and can build nothing. Enabling it is the operator
/// saying they have installed the template's dependencies
/// (`npm --prefix tools/app-builder run install-templates`).
///
/// `worker_image` is the honest half of **D-M6-1**. Set it only for a launch
/// profile that really isolates the network (a container, per ADR-027); the host
/// then attests `network: disabled` in every bundle's provenance. Left unset —
/// ADR-027's dev/CI process fallback — the host attests `network: enabled`,
/// because that is what is true. The builder *refuses* to claim isolation it
/// did not have.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The command that runs the worker. Ops owns the launch profile.
    #[serde(default = "default_app_worker_command")]
    pub worker_command: String,
    /// Arguments to that command; the default runs the in-repo worker directly.
    #[serde(default = "default_app_worker_args")]
    pub worker_args: Vec<String>,
    /// The lockfile whose hash is recorded as build provenance. It is read and
    /// hashed by the **host** at startup — provenance a worker could report is
    /// provenance a worker could forge (docs/06 §5/§6).
    #[serde(default = "default_app_lockfile")]
    pub lockfile: PathBuf,
    /// Builder image reference. Presence is what lets the host attest network
    /// isolation; see the type docs.
    #[serde(default)]
    pub worker_image: Option<String>,
}

impl Default for AppsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            worker_command: default_app_worker_command(),
            worker_args: default_app_worker_args(),
            lockfile: default_app_lockfile(),
            worker_image: None,
        }
    }
}

fn default_app_worker_command() -> String {
    "node".to_owned()
}

fn default_app_worker_args() -> Vec<String> {
    vec!["tools/app-builder/src/index.mjs".to_owned()]
}

fn default_app_lockfile() -> PathBuf {
    PathBuf::from("tools/app-builder/templates/dashboard-v1/package-lock.json")
}
