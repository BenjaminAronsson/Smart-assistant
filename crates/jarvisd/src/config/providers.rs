use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Model/embedding provider configuration (docs/09 §1 `[providers.*]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    #[serde(rename = "claude-cli")]
    pub claude_cli: ClaudeCliConfig,
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
}

/// `[providers.embeddings]` (M4, docs/09 §1/§5). The model is CPU-only and
/// loaded lazily by the adapter; these values are references/bounds, never
/// prompt content or secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingsConfig {
    #[serde(default = "default_embedding_model")]
    pub model: String,
    #[serde(default = "default_embedding_cache_dir")]
    pub cache_dir: PathBuf,
    #[serde(default = "default_embedding_idle_unload_secs")]
    pub idle_unload_secs: u64,
    #[serde(default = "default_embedding_threads")]
    pub intra_threads: usize,
}

fn default_embedding_model() -> String {
    "bge-small-en-v1.5".to_owned()
}

fn default_embedding_cache_dir() -> PathBuf {
    PathBuf::from("/var/lib/jarvis/models")
}

fn default_embedding_idle_unload_secs() -> u64 {
    600
}

fn default_embedding_threads() -> usize {
    2
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            model: default_embedding_model(),
            cache_dir: default_embedding_cache_dir(),
            idle_unload_secs: default_embedding_idle_unload_secs(),
            intra_threads: default_embedding_threads(),
        }
    }
}

impl EmbeddingsConfig {
    pub fn to_adapter(&self) -> jarvis_adapters::embeddings::FastEmbedConfig {
        jarvis_adapters::embeddings::FastEmbedConfig {
            model: self.model.clone(),
            cache_dir: self.cache_dir.clone(),
            intra_threads: self.intra_threads,
            idle_unload_secs: self.idle_unload_secs,
        }
    }
}

/// `[providers.claude-cli]` (docs/09 §1, ADR-004). The reasoning-profile CLI
/// adapter's spawn contract: binary, controlled workdir, built-in tools disabled.
///
/// Unknown keys are tolerated (no `deny_unknown_fields`) because docs/09 §1
/// documents the full block — `enabled`, `timeout_secs`, `single_flight`,
/// `backoff_initial_secs`, `backoff_max_secs` — but those are host-level health
/// /single-flight concerns wired in F1.7, not the adapter's spawn contract. They
/// are modelled here when that wiring lands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeCliConfig {
    /// The CLI binary, resolved on the PATH of the service user.
    pub binary: String,
    /// Controlled working directory the process is spawned in (ADR-004).
    pub workdir: PathBuf,
    /// Reasoning profile disables the CLI's built-in tools — Jarvis tools are the
    /// only action path (invariant 1, ADR-004/014).
    pub reasoning_disable_builtin_tools: bool,
    /// Idle read timeout in seconds: no event within this window ⇒ unhealthy.
    pub idle_timeout_secs: u64,
}

impl ClaudeCliConfig {
    /// Map to the adapter's spawn config (`jarvis-adapters`). Kept here so the
    /// adapter never depends on the host's config types.
    pub fn to_adapter(&self) -> jarvis_adapters::claude_cli::ClaudeCliConfig {
        jarvis_adapters::claude_cli::ClaudeCliConfig {
            binary: self.binary.clone(),
            workdir: self.workdir.clone(),
            disable_builtin_tools: self.reasoning_disable_builtin_tools,
            idle_timeout: std::time::Duration::from_secs(self.idle_timeout_secs),
        }
    }
}
