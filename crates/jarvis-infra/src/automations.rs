//! Automation persistence (FR-17, F8.6, migration 0018).
//!
//! Nothing here reads or writes authority — there is no scopes column to read.
//! What is stored is who created an automation; what they may do is decided at
//! fire time by `jarvis_application::automations::decide_at_fire_time`.

use async_trait::async_trait;
use jarvis_application::ports::{AutomationStore, RepositoryError};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::automations::{
    Automation, AutomationAction, AutomationExecution, AutomationName, ExecutionOutcome, Trigger,
};
use jarvis_domain::ids::AutomationId;
use sqlx::PgPool;
use std::time::SystemTime;
use time::OffsetDateTime;

pub struct PgAutomationStore {
    pool: PgPool,
}

impl PgAutomationStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

struct AutomationRow {
    id: String,
    name: String,
    trigger_kind: String,
    trigger_minute: Option<i32>,
    trigger_entity: Option<String>,
    trigger_state: Option<String>,
    tool_id: String,
    arguments_json: String,
    enabled: bool,
    created_by_device_id: String,
    created_at: OffsetDateTime,
    last_fired_at: Option<OffsetDateTime>,
}

impl AutomationRow {
    /// Rebuild the domain type, re-validating every column. A row we cannot
    /// interpret is an error, never a default — in particular an unreadable
    /// trigger must not silently read as one that never fires, because a
    /// silently-never-firing automation is indistinguishable from a working one
    /// until the day it matters.
    fn into_automation(self) -> Result<Automation, RepositoryError> {
        let id = self
            .id
            .parse::<AutomationId>()
            .map_err(|e| RepositoryError::Storage(format!("bad automation id: {e}")))?;
        let name = AutomationName::new(&self.name)
            .map_err(|e| RepositoryError::Storage(format!("bad automation name: {e}")))?;
        let trigger = match self.trigger_kind.as_str() {
            "daily_at" => {
                let minute = self.trigger_minute.ok_or_else(|| {
                    RepositoryError::Storage("daily_at row has no minute".to_owned())
                })?;
                Trigger::DailyAt {
                    minutes_since_midnight: u16::try_from(minute).map_err(|_| {
                        RepositoryError::Storage(format!("minute {minute} out of range"))
                    })?,
                }
            }
            "ha_state" => Trigger::HomeAssistantState {
                entity_id: self.trigger_entity.ok_or_else(|| {
                    RepositoryError::Storage("ha_state row has no entity".to_owned())
                })?,
                state: self.trigger_state.ok_or_else(|| {
                    RepositoryError::Storage("ha_state row has no state".to_owned())
                })?,
            },
            other => {
                return Err(RepositoryError::Storage(format!(
                    "unknown trigger kind {other:?}"
                )));
            }
        };
        let arguments = crate::canonical::json_to_canonical(
            serde_json::from_str(&self.arguments_json)
                .map_err(|e| RepositoryError::Storage(format!("bad automation arguments: {e}")))?,
        );
        let action = AutomationAction {
            tool_id: self
                .tool_id
                .parse()
                .map_err(|e| RepositoryError::Storage(format!("bad tool id: {e}")))?,
            arguments,
        };
        let created_by = self
            .created_by_device_id
            .parse()
            .map_err(|e| RepositoryError::Storage(format!("bad creator device id: {e}")))?;

        Ok(Automation::from_parts(
            id,
            name,
            trigger,
            action,
            self.enabled,
            created_by,
            self.created_at.into(),
            self.last_fired_at.map(Into::into),
        ))
    }
}

/// The trigger's columns, in the shape the CHECK constraints require: exactly
/// the set its kind names, and nothing from the other kind.
fn trigger_columns(trigger: &Trigger) -> (Option<i32>, Option<&str>, Option<&str>) {
    match trigger {
        Trigger::DailyAt {
            minutes_since_midnight,
        } => (Some(i32::from(*minutes_since_midnight)), None, None),
        Trigger::HomeAssistantState { entity_id, state } => {
            (None, Some(entity_id.as_str()), Some(state.as_str()))
        }
    }
}

fn utc(t: SystemTime) -> OffsetDateTime {
    OffsetDateTime::from(t)
}

/// The outcome's stored spelling plus its detail, kept together so a variant
/// cannot be written with another's detail.
fn outcome_columns(outcome: &ExecutionOutcome) -> (&'static str, Option<String>) {
    match outcome {
        ExecutionOutcome::Executed => ("executed", None),
        ExecutionOutcome::NeedsApproval { exact_effect } => {
            ("needs_approval", Some(exact_effect.clone()))
        }
        ExecutionOutcome::Denied { reason } => ("denied", Some(reason.clone())),
        ExecutionOutcome::Failed { reason } => ("failed", Some(reason.clone())),
    }
}

fn outcome_from(stored: &str, detail: Option<String>) -> Result<ExecutionOutcome, RepositoryError> {
    Ok(match stored {
        "executed" => ExecutionOutcome::Executed,
        "needs_approval" => ExecutionOutcome::NeedsApproval {
            exact_effect: detail.unwrap_or_default(),
        },
        "denied" => ExecutionOutcome::Denied {
            reason: detail.unwrap_or_default(),
        },
        "failed" => ExecutionOutcome::Failed {
            reason: detail.unwrap_or_default(),
        },
        other => {
            return Err(RepositoryError::Storage(format!(
                "unknown execution outcome {other:?}"
            )));
        }
    })
}

#[async_trait]
impl AutomationStore for PgAutomationStore {
    async fn create(
        &self,
        automation: &Automation,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        let (minute, entity, state) = trigger_columns(automation.trigger());
        let arguments = serde_json::to_string(&crate::canonical::canonical_to_json(
            &automation.action().arguments,
        ))
        .map_err(|e| RepositoryError::Storage(format!("encoding arguments: {e}")))?;
        let now = utc(automation.created_at());

        // Row and audit in one transaction (invariant 6): an automation that
        // cannot be recorded is not created.
        let mut tx = self.pool.begin().await.map_err(storage)?;
        sqlx::query!(
            r#"
            INSERT INTO automations.automations
                (id, name, trigger_kind, trigger_minute, trigger_entity, trigger_state,
                 tool_id, arguments_json, enabled, created_by_device_id, created_at, updated_at,
                 last_fired_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $11, $12)
            "#,
            automation.id().as_str(),
            automation.name().as_str(),
            automation.trigger().kind(),
            minute,
            entity,
            state,
            automation.action().tool_id.as_str(),
            arguments,
            automation.is_enabled(),
            automation.created_by().as_str(),
            now,
            automation.last_fired_at().map(utc),
        )
        .execute(&mut *tx)
        .await
        .map_err(storage)?;

        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("automation create: audit: {e}")))?;
        tx.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn list_enabled(&self) -> Result<Vec<Automation>, RepositoryError> {
        let rows = sqlx::query_as!(
            AutomationRow,
            r#"
            SELECT id, name, trigger_kind, trigger_minute, trigger_entity, trigger_state,
                   tool_id, arguments_json, enabled, created_by_device_id, created_at,
                   last_fired_at
            FROM automations.automations
            WHERE enabled
            ORDER BY created_at
            "#
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(AutomationRow::into_automation)
            .collect()
    }

    async fn list_all(&self) -> Result<Vec<Automation>, RepositoryError> {
        let rows = sqlx::query_as!(
            AutomationRow,
            r#"
            SELECT id, name, trigger_kind, trigger_minute, trigger_entity, trigger_state,
                   tool_id, arguments_json, enabled, created_by_device_id, created_at,
                   last_fired_at
            FROM automations.automations
            ORDER BY created_at
            "#
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(AutomationRow::into_automation)
            .collect()
    }

    async fn set_enabled(&self, id: &AutomationId, enabled: bool) -> Result<(), RepositoryError> {
        sqlx::query!(
            r#"
            UPDATE automations.automations
            SET enabled = $2, updated_at = now()
            WHERE id = $1
            "#,
            id.as_str(),
            enabled,
        )
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn delete(&self, id: &AutomationId) -> Result<(), RepositoryError> {
        sqlx::query!(
            "DELETE FROM automations.automations WHERE id = $1",
            id.as_str()
        )
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn record_execution(
        &self,
        execution: &AutomationExecution,
    ) -> Result<(), RepositoryError> {
        let (outcome, detail) = outcome_columns(&execution.outcome);
        let occurred_at = utc(execution.occurred_at);

        // History row and the rate-limit stamp together: a firing that is
        // recorded but not rate-limited (or the reverse) shows up as a flapping
        // sensor turning the lights on forty times.
        let mut tx = self.pool.begin().await.map_err(storage)?;
        sqlx::query!(
            r#"
            INSERT INTO automations.executions (automation_id, occurred_at, outcome, detail)
            VALUES ($1, $2, $3, $4)
            "#,
            execution.automation_id.as_str(),
            occurred_at,
            outcome,
            detail,
        )
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        sqlx::query!(
            r#"
            UPDATE automations.automations
            SET last_fired_at = $2, updated_at = $2
            WHERE id = $1
            "#,
            execution.automation_id.as_str(),
            occurred_at,
        )
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn history(
        &self,
        id: &AutomationId,
        limit: i64,
    ) -> Result<Vec<AutomationExecution>, RepositoryError> {
        let rows = sqlx::query!(
            r#"
            SELECT automation_id, occurred_at, outcome, detail
            FROM automations.executions
            WHERE automation_id = $1
            ORDER BY occurred_at DESC
            LIMIT $2
            "#,
            id.as_str(),
            limit.clamp(1, 500),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;

        rows.into_iter()
            .map(|row| {
                Ok(AutomationExecution {
                    automation_id: row
                        .automation_id
                        .parse()
                        .map_err(|e| RepositoryError::Storage(format!("bad automation id: {e}")))?,
                    occurred_at: row.occurred_at.into(),
                    outcome: outcome_from(&row.outcome, row.detail)?,
                })
            })
            .collect()
    }
}

fn storage(e: sqlx::Error) -> RepositoryError {
    RepositoryError::Storage(e.to_string())
}
