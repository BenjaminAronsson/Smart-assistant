//! Postgres-backed run persistence (docs/04 §3, docs/02 §4, NFR-05). One store
//! implements both `RunStore` (create/load) and the orchestrator's
//! `Checkpointer` (per-transition save). Every write that changes run state also
//! writes its outbox event in the SAME transaction (docs/02 §2), so the durable
//! state and the published event can never disagree. Repos return domain types,
//! never rows; the mapping lives here.

use jarvis_application::orchestrator::{CheckpointError, Checkpointer};
use jarvis_application::ports::{RepositoryError, RunStore, RunView};
use jarvis_domain::ids::{RunId, SessionId};
use jarvis_domain::run::{Run, RunBudget, RunOutcome, RunOutcomeKind, RunState, RunUsage};
use serde_json::json;
use sqlx::PgPool;
use std::time::Duration;
use time::OffsetDateTime;

pub struct PgRunStore {
    pool: PgPool,
}

impl PgRunStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl RunStore for PgRunStore {
    async fn create(&self, run: &Run) -> Result<(), RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let now = OffsetDateTime::now_utc();
        sqlx::query!(
            r#"
            INSERT INTO orchestration.runs
                (id, session_id, state, max_model_turns, max_tool_calls,
                 max_duration_secs, max_artifact_bytes, used_model_turns,
                 used_tool_calls, outcome_kind, outcome_detail, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $12)
            "#,
            run.id.as_str(),
            run.session_id.as_str(),
            state_str(run.state),
            i32::from(run.budget.max_model_turns),
            i32::from(run.budget.max_tool_calls),
            clamp_i64(run.budget.max_duration.as_secs()),
            clamp_i64(run.budget.max_artifact_bytes),
            i32::from(run.usage.model_turns),
            i32::from(run.usage.tool_calls),
            outcome_kind_str(run.outcome.as_ref()),
            outcome_detail(run.outcome.as_ref()),
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(storage)?;

        insert_outbox(
            &mut tx,
            "run.started",
            json!({ "runId": run.id.as_str(), "sessionId": run.session_id.as_str() }),
        )
        .await
        .map_err(storage)?;

        tx.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn load(&self, id: &RunId) -> Result<Option<Run>, RepositoryError> {
        Ok(self.view(id).await?.map(|v| v.run))
    }

    async fn view(&self, id: &RunId) -> Result<Option<RunView>, RepositoryError> {
        let row = sqlx::query!(
            r#"
            SELECT session_id, state, max_model_turns, max_tool_calls,
                   max_duration_secs, max_artifact_bytes, used_model_turns,
                   used_tool_calls, outcome_kind, outcome_detail,
                   created_at, updated_at
            FROM orchestration.runs WHERE id = $1
            "#,
            id.as_str(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;

        let Some(r) = row else {
            return Ok(None);
        };
        let run = build_run(RunRow {
            id: id.clone(),
            session_id: r.session_id,
            state: r.state,
            max_model_turns: r.max_model_turns,
            max_tool_calls: r.max_tool_calls,
            max_duration_secs: r.max_duration_secs,
            max_artifact_bytes: r.max_artifact_bytes,
            used_model_turns: r.used_model_turns,
            used_tool_calls: r.used_tool_calls,
            outcome_kind: r.outcome_kind,
            outcome_detail: r.outcome_detail,
        })?;
        Ok(Some(RunView {
            run,
            created_at: r.created_at.into(),
            updated_at: r.updated_at.into(),
        }))
    }

    async fn load_unfinished(&self) -> Result<Vec<Run>, RepositoryError> {
        // Non-terminal == no recorded outcome (the outcome is written exactly on
        // the terminal transition). Oldest-first so recovery is deterministic.
        // Runs once at startup (docs/02 §12); the result is bounded by how many
        // runs were in flight at the last crash — a handful in M1 (single-flight
        // lands in F1.6). If concurrent-run scale grows this should page (LIMIT +
        // cursor) rather than load every row (perf-warden F1.5 advisory).
        let rows = sqlx::query!(
            r#"
            SELECT id, session_id, state, max_model_turns, max_tool_calls,
                   max_duration_secs, max_artifact_bytes, used_model_turns,
                   used_tool_calls, outcome_kind, outcome_detail
            FROM orchestration.runs
            WHERE outcome_kind IS NULL
            ORDER BY created_at ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;

        rows.into_iter()
            .map(|r| {
                let id: RunId = r
                    .id
                    .parse()
                    .map_err(|e| RepositoryError::Storage(format!("stored run id invalid: {e}")))?;
                build_run(RunRow {
                    id,
                    session_id: r.session_id,
                    state: r.state,
                    max_model_turns: r.max_model_turns,
                    max_tool_calls: r.max_tool_calls,
                    max_duration_secs: r.max_duration_secs,
                    max_artifact_bytes: r.max_artifact_bytes,
                    used_model_turns: r.used_model_turns,
                    used_tool_calls: r.used_tool_calls,
                    outcome_kind: r.outcome_kind,
                    outcome_detail: r.outcome_detail,
                })
            })
            .collect()
    }
}

/// The persisted columns of a run row, shared by the read paths so the
/// row → domain mapping lives in exactly one place.
struct RunRow {
    id: RunId,
    session_id: String,
    state: String,
    max_model_turns: i32,
    max_tool_calls: i32,
    max_duration_secs: i64,
    max_artifact_bytes: i64,
    used_model_turns: i32,
    used_tool_calls: i32,
    outcome_kind: Option<String>,
    outcome_detail: Option<String>,
}

fn build_run(r: RunRow) -> Result<Run, RepositoryError> {
    let session_id: SessionId = r
        .session_id
        .parse()
        .map_err(|e| RepositoryError::Storage(format!("stored session id invalid: {e}")))?;
    Ok(Run {
        id: r.id,
        session_id,
        state: state_from(&r.state)?,
        budget: RunBudget {
            max_model_turns: u8::try_from(r.max_model_turns).unwrap_or(u8::MAX),
            max_tool_calls: u16::try_from(r.max_tool_calls).unwrap_or(u16::MAX),
            max_duration: Duration::from_secs(u64::try_from(r.max_duration_secs).unwrap_or(0)),
            max_artifact_bytes: u64::try_from(r.max_artifact_bytes).unwrap_or(0),
        },
        usage: RunUsage {
            model_turns: u8::try_from(r.used_model_turns).unwrap_or(u8::MAX),
            tool_calls: u16::try_from(r.used_tool_calls).unwrap_or(u16::MAX),
            // Recomputed by the orchestrator from the run start on resume.
            elapsed: Duration::ZERO,
            artifact_bytes: 0,
        },
        outcome: outcome_from(r.outcome_kind.as_deref(), r.outcome_detail)?,
    })
}

#[async_trait::async_trait]
impl Checkpointer for PgRunStore {
    async fn save(&self, run: &Run) -> Result<(), CheckpointError> {
        let mut tx = self.pool.begin().await.map_err(ckpt)?;
        let now = OffsetDateTime::now_utc();
        sqlx::query!(
            r#"
            UPDATE orchestration.runs
            SET state = $2, used_model_turns = $3, used_tool_calls = $4,
                outcome_kind = $5, outcome_detail = $6, updated_at = $7
            WHERE id = $1
            "#,
            run.id.as_str(),
            state_str(run.state),
            i32::from(run.usage.model_turns),
            i32::from(run.usage.tool_calls),
            outcome_kind_str(run.outcome.as_ref()),
            outcome_detail(run.outcome.as_ref()),
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(ckpt)?;

        sqlx::query!(
            "INSERT INTO orchestration.checkpoints (run_id, state) VALUES ($1, $2)",
            run.id.as_str(),
            state_str(run.state),
        )
        .execute(&mut *tx)
        .await
        .map_err(ckpt)?;

        let (event_type, payload) = run_event(run);
        insert_outbox(&mut tx, event_type, payload)
            .await
            .map_err(ckpt)?;

        tx.commit().await.map_err(ckpt)?;
        Ok(())
    }
}

async fn insert_outbox(
    conn: &mut sqlx::PgConnection,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO outbox.outbox_events (event_type, payload) VALUES ($1, $2)",
        event_type,
        payload,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// The persisted domain event for the run's current state: `run.completed` once
/// terminal (carrying the outcome), else `run.state_changed`. Payload shapes
/// match the wire `DomainEvent` (docs/05 §3) so the host forwards them verbatim.
///
/// Note: `DomainEvent::CheckpointSaved` (`run.checkpoint_saved`) is intentionally
/// NOT emitted in M1 — every checkpoint boundary is already carried by the
/// `state_changed`/`completed` event written in the same transaction, so a
/// distinct event would be redundant for resync. Whether checkpoints should
/// surface separately is revisited if a client ever needs the last safe boundary
/// independently of the state (contract-keeper F1.4).
fn run_event(run: &Run) -> (&'static str, serde_json::Value) {
    match run.outcome.as_ref() {
        Some(outcome) => (
            "run.completed",
            json!({ "runId": run.id.as_str(), "outcome": outcome_json(outcome) }),
        ),
        None => (
            "run.state_changed",
            json!({ "runId": run.id.as_str(), "state": state_str(run.state) }),
        ),
    }
}

fn outcome_json(outcome: &RunOutcome) -> serde_json::Value {
    match &outcome.detail {
        Some(detail) => json!({ "kind": outcome_kind_text(outcome.kind), "detail": detail }),
        None => json!({ "kind": outcome_kind_text(outcome.kind) }),
    }
}

fn clamp_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn storage(e: sqlx::Error) -> RepositoryError {
    RepositoryError::Storage(e.to_string())
}

fn ckpt(e: sqlx::Error) -> CheckpointError {
    CheckpointError(e.to_string())
}

fn state_str(state: RunState) -> &'static str {
    match state {
        RunState::Received => "received",
        RunState::ContextReady => "context_ready",
        RunState::ModelRunning => "model_running",
        RunState::PolicyReview => "policy_review",
        RunState::WaitingApproval => "waiting_approval",
        RunState::ToolRunning => "tool_running",
        RunState::Replanning => "replanning",
        RunState::Responding => "responding",
        RunState::Completed => "completed",
        RunState::Failed => "failed",
        RunState::Cancelled => "cancelled",
    }
}

fn state_from(state: &str) -> Result<RunState, RepositoryError> {
    Ok(match state {
        "received" => RunState::Received,
        "context_ready" => RunState::ContextReady,
        "model_running" => RunState::ModelRunning,
        "policy_review" => RunState::PolicyReview,
        "waiting_approval" => RunState::WaitingApproval,
        "tool_running" => RunState::ToolRunning,
        "replanning" => RunState::Replanning,
        "responding" => RunState::Responding,
        "completed" => RunState::Completed,
        "failed" => RunState::Failed,
        "cancelled" => RunState::Cancelled,
        other => {
            return Err(RepositoryError::Storage(format!(
                "stored run state invalid: {other:?}"
            )));
        }
    })
}

fn outcome_kind_text(kind: RunOutcomeKind) -> &'static str {
    match kind {
        RunOutcomeKind::Completed => "completed",
        RunOutcomeKind::Failed => "failed",
        RunOutcomeKind::Cancelled => "cancelled",
    }
}

fn outcome_kind_str(outcome: Option<&RunOutcome>) -> Option<&'static str> {
    outcome.map(|o| outcome_kind_text(o.kind))
}

fn outcome_detail(outcome: Option<&RunOutcome>) -> Option<String> {
    outcome.and_then(|o| o.detail.clone())
}

fn outcome_from(
    kind: Option<&str>,
    detail: Option<String>,
) -> Result<Option<RunOutcome>, RepositoryError> {
    let kind = match kind {
        None => return Ok(None),
        Some("completed") => RunOutcomeKind::Completed,
        Some("failed") => RunOutcomeKind::Failed,
        Some("cancelled") => RunOutcomeKind::Cancelled,
        Some(other) => {
            return Err(RepositoryError::Storage(format!(
                "stored outcome kind invalid: {other:?}"
            )));
        }
    };
    Ok(Some(RunOutcome { kind, detail }))
}
