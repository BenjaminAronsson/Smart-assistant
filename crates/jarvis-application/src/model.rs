//! The provider-neutral model boundary (docs/05 §4, FR-03, NFR-08). The
//! orchestrator talks only to this port; the Claude CLI adapter (F1.6) and the
//! `FakeModel` both implement it, and no provider-specific type ever crosses
//! upward (arch-test enforces the crate boundary).
//!
//! M1 carries the text-slice event set. `ToolProposal` and `ModelCapabilities`
//! (docs/05 §4) land additively with tools/routing in M2/F1.7 — the port shape
//! stays stable.

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use tokio_util::sync::CancellationToken;

/// A model profile identifier (e.g. `claude-cli`, `deterministic`). Opaque to
/// the orchestrator; the router (F1.7) reasons about profiles, not the loop.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProfileId(pub String);

impl ProfileId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One model turn's input: the assembled, bounded prompt (docs/02 §5 step 3).
/// Context assembly has already happened; this is what actually crosses to the
/// provider, and the user can inspect it (NFR-02).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRequest {
    pub prompt: String,
}

/// Why a model turn finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// The model produced a complete response.
    Stop,
    /// The response was truncated at the context/output limit.
    Length,
}

/// A token-usage sample from the provider (best-effort; providers that do not
/// report usage simply never emit this).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UsageSample {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// A provider-side failure, mapped from adapter internals to a neutral shape
/// (no raw provider text crosses this boundary, docs/06 §5). Health
/// classification (quota/auth/rate-limit with reset hints) is refined in
/// F1.6/F1.7; F1.3 needs only the coarse cases.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    /// The provider could not be started or is unhealthy (auth/quota/network).
    /// Degraded-mode queueing (F1.7) reacts to this; F1.3 fails the run.
    #[error("model provider unavailable: {0}")]
    Unavailable(String),
    /// The provider stream was malformed beyond tolerance.
    #[error("model stream malformed: {0}")]
    Malformed(String),
}

/// One event from a streaming model turn (docs/05 §4). Token deltas are
/// transient (never persisted/replayed, docs/05 §3); `Done`/`Error` are the
/// terminal signals the orchestrator acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEvent {
    /// One incremental chunk of response text.
    TextDelta(String),
    /// A usage sample (optional, best-effort).
    Usage(UsageSample),
    /// The turn finished producing output.
    Done(FinishReason),
    /// The turn failed mid-stream.
    Error(ModelError),
}

/// The model boundary (docs/05 §4). `run` returns a stream of [`ModelEvent`]s;
/// the provided [`CancellationToken`] must abort in-flight work promptly and
/// leave no orphaned process (adapters kill + reap — see the provider-adapter
/// skill; the `FakeModel` drops its stream).
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// The profile this provider serves.
    fn id(&self) -> ProfileId;

    /// Start a model turn. An `Err` here is an *open* failure (the turn never
    /// started); a mid-stream failure arrives as [`ModelEvent::Error`].
    async fn run(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ModelEvent>, ModelError>;
}
