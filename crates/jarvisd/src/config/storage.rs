use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `[storage]` (docs/04 §1, ADR-008). Root of the content-addressed artifact
/// blob store: manifests live in Postgres, blob bytes live here keyed by their
/// SHA-256. The directory is created lazily on first write, so it need not exist
/// at startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    #[serde(default = "default_artifacts_root")]
    pub artifacts_root: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            artifacts_root: default_artifacts_root(),
        }
    }
}

fn default_artifacts_root() -> PathBuf {
    PathBuf::from("/var/lib/jarvis/artifacts")
}
