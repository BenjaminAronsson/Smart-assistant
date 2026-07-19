//! Run DTOs (docs/05 §1/§4, FR-01/07). The wire projection of the orchestrator
//! `RunState` (docs/02 §4) and its budgets/outcome. The domain enum lives in
//! `jarvis-domain` (F1.2); this is the transport shape and the two are mapped
//! at the gateway, never conflated.

use jarvis_domain::ids::{RunId, SessionId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Wire projection of `jarvis_domain::RunState` (docs/02 §4). The policy/tool/
/// approval states exist on the wire from M1 but are only produced once their
/// step executors land in M2 — the shape is stable so clients need no change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStateDto {
    Received,
    ContextReady,
    ModelRunning,
    PolicyReview,
    WaitingApproval,
    ToolRunning,
    Replanning,
    Responding,
    Completed,
    Failed,
    Cancelled,
}
// NOTE: the FR-12 "queued / visible waiting" signal is carried by the
// `DomainEvent::RunQueued` event, not a RunState — whether queuing becomes a
// distinct `RunState` is the state-machine feature's (F1.2) call, made with an
// ADR + a docs/02 §4 update. Adding a wire variant then stays additive.

impl RunStateDto {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Per-run resource caps (docs/05 §4, NFR-12). Durations are seconds on the
/// wire (JSON has no duration type).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunBudgetDto {
    pub max_model_turns: u8,
    pub max_tool_calls: u16,
    pub max_duration_secs: u64,
    pub max_artifact_bytes: u64,
}

/// Terminal outcome of a run (docs/02 §4). `detail` is a short human sentence,
/// never raw provider/driver text (docs/06 §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunOutcome {
    pub kind: RunOutcomeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcomeKind {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunDto {
    #[schemars(with = "crate::schema::UlidString")]
    pub id: RunId,
    #[schemars(with = "crate::schema::UlidString")]
    pub session_id: SessionId,
    pub state: RunStateDto,
    pub budget: RunBudgetDto,
    /// Present only once the run is terminal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunOutcome>,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339.
    pub updated_at: String,
}

/// Acknowledgement returned by `POST /sessions/{id}/messages` (docs/05 §1):
/// the run has been accepted and persisted; streaming follows on the WS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunAck {
    #[schemars(with = "crate::schema::UlidString")]
    pub run_id: RunId,
    #[schemars(with = "crate::schema::UlidString")]
    pub session_id: SessionId,
    pub state: RunStateDto,
}
