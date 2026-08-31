use super::shared::RepositoryError;
use jarvis_domain::conversations::Message;
use jarvis_domain::ids::{RunId, SessionId};
use jarvis_domain::run::Run;
use std::time::SystemTime;

/// A run plus its persistence timestamps — the read model behind
/// `GET /runs/{id}` (docs/05 §1). The domain [`Run`] is deliberately clock-free
/// (F1.2), so the store surfaces `created_at`/`updated_at` alongside the
/// reconstructed run rather than folding clocks into the aggregate.
#[derive(Debug, Clone, PartialEq)]
pub struct RunView {
    pub run: Run,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}

/// Run persistence (FR-01/07, NFR-05). `create` records a new run and its
/// `run.started` event in one transaction (transactional outbox, docs/02 §2);
/// `load` reads a run back for restart recovery — the durable state the
/// orchestrator resumes from (its per-transition checkpoints go through the
/// [`crate::orchestrator::Checkpointer`] port, which infra implements on the
/// same store).
#[async_trait::async_trait]
pub trait RunStore: Send + Sync {
    async fn create(&self, run: &Run) -> Result<(), RepositoryError>;
    async fn load(&self, id: &RunId) -> Result<Option<Run>, RepositoryError>;
    /// Same as [`Self::load`] but including persistence timestamps for the wire
    /// `RunDto` (docs/05 §1).
    async fn view(&self, id: &RunId) -> Result<Option<RunView>, RepositoryError>;
    /// Every run not yet in a terminal state — the restart-recovery worklist
    /// (NFR-05, docs/02 §12). The host re-drives each from its durable
    /// checkpoint; returned oldest-first so recovery order is deterministic.
    async fn load_unfinished(&self) -> Result<Vec<Run>, RepositoryError>;
}

/// Message persistence (FR-01, FR-02). Messages are immutable (docs/04 §2);
/// `append` writes the row and its `message.created` event in one transaction,
/// `list_by_session` is the timeline read (oldest first).
#[async_trait::async_trait]
pub trait MessageStore: Send + Sync {
    async fn append(&self, message: &Message) -> Result<(), RepositoryError>;
    async fn list_by_session(
        &self,
        session_id: &SessionId,
        limit: u32,
    ) -> Result<Vec<Message>, RepositoryError>;
}
