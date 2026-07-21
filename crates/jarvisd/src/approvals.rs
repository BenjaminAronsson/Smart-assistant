//! The live human-approval seam (F2.5, docs/06 §3-§4, invariant #1).
//!
//! [`JarvisApprovalGate`] is the host implementation of the application
//! [`ApprovalGate`] port. When the orchestrator parks a run at `WaitingApproval`
//! (F2.6 wires that path — a tool must first be *proposed*, and no tool is
//! registered yet, ADR-004), the gate:
//!   1. mints an [`ApprovalId`] (the host owns randomness; the pure orchestrator
//!      cannot),
//!   2. builds the [`ApprovalCardDto`] carrying the **exact effect** and the real
//!      proposed arguments,
//!   3. persists `approval.requested` to the outbox (→ WS + timeline) and the
//!      audit chain in one transaction, and
//!   4. parks the drive future on a one-shot until [`resolve`] delivers the
//!      human's decision from `POST /runs/{id}/approvals/{approval_id}`.
//!
//! Text never grants authority (invariant #1): the REST body cannot *cause*
//! execution — it only unblocks the orchestrator, which then mints and validates
//! a grant bound to exactly what was approved. Editing the arguments rebinds that
//! grant, so an edited approval executes the edited set or nothing (docs/06 §4).
//!
//! The pending map is process-local: a restart while a run is parked at
//! `WaitingApproval` drops the pending decision (the run re-drives from its
//! input on recovery). Durable cross-restart approvals stay out of M2 scope.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use async_trait::async_trait;
use jarvis_application::policy::{ApprovalGate, ApprovalOutcome, ApprovalRequest};
use jarvis_contracts::approvals::{
    ApprovalCardDto, ApprovalDecision, ApprovalDecisionDto, ApprovalResolutionDto,
};
use jarvis_contracts::events::DomainEvent;
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::{ApprovalId, RunId};
use jarvis_domain::tools::{CanonicalValue, ToolId};
use jarvis_infra::approvals::{ApprovalPersistError, record_approval_event};
use sqlx::PgPool;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::auth::fresh_id;

/// A parked approval awaiting a human decision. Holds the *original* proposed
/// arguments so an approval WITHOUT edits binds exactly what was proposed, and
/// the `run_id` so a decision can be checked to belong to the addressed run.
struct Pending {
    run_id: RunId,
    tool_id: ToolId,
    proposed_arguments: CanonicalValue,
    tx: oneshot::Sender<ApprovalOutcome>,
}

/// Postgres-backed, process-local approval gate. Shared (`Arc`) between the run
/// engine (which hands it to the orchestrator's `ToolStack`) and the REST
/// surface (which resolves pending approvals).
pub struct JarvisApprovalGate {
    pool: PgPool,
    pending: Mutex<HashMap<ApprovalId, Pending>>,
}

/// Why a `POST /runs/{id}/approvals/{approval_id}` could not be applied.
#[derive(Debug)]
pub enum ResolveError {
    /// No pending approval with that id for that run (already resolved, expired
    /// with the run, wrong run, or never existed). A 404 to the client.
    NotFound,
    /// The durable record of the decision could not be written; the run stays
    /// parked and the client may retry.
    Persist(ApprovalPersistError),
}

impl From<ApprovalPersistError> for ResolveError {
    fn from(error: ApprovalPersistError) -> Self {
        Self::Persist(error)
    }
}

impl JarvisApprovalGate {
    pub fn new(pool: PgPool) -> Arc<Self> {
        Arc::new(Self {
            pool,
            pending: Mutex::new(HashMap::new()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<ApprovalId, Pending>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Apply a human decision to a parked approval (the REST path). Records
    /// `approval.resolved` durably BEFORE unparking the run, so the decision
    /// survives even if the parked task has since been cancelled. Approving with
    /// `editedArguments` binds the edited set; approving without edits binds the
    /// original proposal (docs/06 §4).
    pub async fn resolve(
        &self,
        run_id: &RunId,
        approval_id: &ApprovalId,
        decision: ApprovalDecisionDto,
        actor: &str,
    ) -> Result<(), ResolveError> {
        // Take ownership of the pending entry under the lock. A decision for the
        // wrong run is reinserted and reported as not-found (no cross-run oracle).
        let pending = {
            let mut map = self.lock();
            match map.remove(approval_id) {
                Some(p) if p.run_id == *run_id => p,
                Some(p) => {
                    map.insert(approval_id.clone(), p);
                    return Err(ResolveError::NotFound);
                }
                None => return Err(ResolveError::NotFound),
            }
        };

        let (outcome, resolution) = match decision.decision {
            ApprovalDecision::Approve => {
                let arguments = match decision.edited_arguments {
                    Some(edited) => json_to_canonical(edited),
                    None => pending.proposed_arguments.clone(),
                };
                (
                    ApprovalOutcome::Approved { arguments },
                    ApprovalResolutionDto::Approved,
                )
            }
            ApprovalDecision::Deny => (ApprovalOutcome::Denied, ApprovalResolutionDto::Denied),
        };

        let event = DomainEvent::ApprovalResolved {
            approval_id: approval_id.clone(),
            run_id: run_id.clone(),
            outcome: resolution,
        };
        let (event_type, payload) = domain_event_outbox(&event);
        let audit = resolved_audit(run_id, approval_id, &pending.tool_id, resolution, actor);
        record_approval_event(&self.pool, event_type, payload, &audit).await?;

        // Unpark the run. A send error means the parked task is already gone
        // (cancelled/restarted); the decision is durably recorded regardless.
        let _ = pending.tx.send(outcome);
        Ok(())
    }
}

#[async_trait]
impl ApprovalGate for JarvisApprovalGate {
    async fn request(
        &self,
        request: ApprovalRequest,
        cancel: CancellationToken,
    ) -> ApprovalOutcome {
        let approval_id = fresh_id::<ApprovalId>();
        let (tx, rx) = oneshot::channel();

        let card = ApprovalCardDto {
            approval_id: approval_id.clone(),
            run_id: request.run_id.clone(),
            tool_id: request.tool_id.to_string(),
            exact_effect: request.exact_effect.clone(),
            proposed_arguments: canonical_to_json(&request.proposed_arguments),
            risk: request.risk.into(),
            reversible: request.reversible,
            egress: request.egress.into(),
        };
        let audit = requested_audit(&request, &approval_id);

        // Register the one-shot BEFORE persisting so a client acting on the
        // `approval.requested` WS event (published only after commit) always
        // finds the pending entry.
        self.lock().insert(
            approval_id.clone(),
            Pending {
                run_id: request.run_id.clone(),
                tool_id: request.tool_id.clone(),
                proposed_arguments: request.proposed_arguments.clone(),
                tx,
            },
        );

        let event = DomainEvent::ApprovalRequested { card };
        let (event_type, payload) = domain_event_outbox(&event);
        if let Err(error) = record_approval_event(&self.pool, event_type, payload, &audit).await {
            // Fail-safe: a request that cannot be recorded authorizes nothing.
            // The infallible port has no error channel (CF-6), so a panic is the
            // conservative outcome — the parked run never proceeds to execute.
            self.lock().remove(&approval_id);
            panic!("approval.requested persist failed: {error}");
        }

        // Park until resolved or cancelled (invariant #4). The returned outcome
        // on cancellation is never observed — the orchestrator drops this future.
        tokio::select! {
            outcome = rx => outcome.unwrap_or(ApprovalOutcome::Denied),
            _ = cancel.cancelled() => {
                self.lock().remove(&approval_id);
                ApprovalOutcome::Denied
            }
        }
    }
}

/// Serialize a `DomainEvent` to its outbox `(event_type, payload)`: the payload
/// is the event object MINUS the `type` discriminator (the envelope carries it),
/// matching the convention `jarvisd::runs::domain_event` folds back on resync.
fn domain_event_outbox(event: &DomainEvent) -> (&'static str, serde_json::Value) {
    let mut value = serde_json::to_value(event).expect("domain event serializes");
    value
        .as_object_mut()
        .expect("domain event serializes to an object")
        .remove("type");
    (event.event_type(), value)
}

/// `approval.requested` audit row. Deliberately minimal: only the tool id — NOT
/// the exact effect or raw arguments — so a sensitive payload never enters the
/// audit chain (invariant #5, CF-7). The full card, which the human must see, is
/// carried on the authenticated outbox/WS path, not here.
fn requested_audit(request: &ApprovalRequest, approval_id: &ApprovalId) -> AuditEvent {
    let payload = serde_json::json!({ "toolId": request.tool_id.to_string() });
    AuditEvent {
        occurred_at: SystemTime::now(),
        actor: "system".to_owned(),
        event_type: "approval.requested".to_owned(),
        target: format!("approval:{approval_id}"),
        correlation_id: Some(request.run_id.as_str().to_owned()),
        payload_json: payload.to_string(),
    }
}

/// `approval.resolved` audit row: the deciding actor, the tool, and the outcome
/// (no raw arguments — invariant #5).
fn resolved_audit(
    run_id: &RunId,
    approval_id: &ApprovalId,
    tool_id: &ToolId,
    resolution: ApprovalResolutionDto,
    actor: &str,
) -> AuditEvent {
    let outcome = match resolution {
        ApprovalResolutionDto::Approved => "approved",
        ApprovalResolutionDto::Denied => "denied",
    };
    let payload = serde_json::json!({ "toolId": tool_id.to_string(), "outcome": outcome });
    AuditEvent {
        occurred_at: SystemTime::now(),
        actor: actor.to_owned(),
        event_type: "approval.resolved".to_owned(),
        target: format!("approval:{approval_id}"),
        correlation_id: Some(run_id.as_str().to_owned()),
        payload_json: payload.to_string(),
    }
}

/// Project a domain [`CanonicalValue`] to display JSON for the approval card.
/// The card's `proposedArguments` is what the human reads; the arguments that
/// actually *bind* come from the gate's stored `CanonicalValue` (or from
/// [`json_to_canonical`] on an edit), so this direction is display-only.
fn canonical_to_json(value: &CanonicalValue) -> serde_json::Value {
    use serde_json::Value;
    match value {
        CanonicalValue::Null => Value::Null,
        CanonicalValue::Bool(b) => Value::Bool(*b),
        CanonicalValue::Int(n) => Value::Number((*n).into()),
        CanonicalValue::Float(text) => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        CanonicalValue::Str(s) => Value::String(s.clone()),
        CanonicalValue::Array(items) => Value::Array(items.iter().map(canonical_to_json).collect()),
        CanonicalValue::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), canonical_to_json(v)))
                .collect(),
        ),
    }
}

/// Lift edited JSON arguments back into a domain [`CanonicalValue`] so the grant
/// binds them. Object keys sort (via `CanonicalValue::Object`'s `BTreeMap`), so
/// the same edit in any key order yields the same canonical form and hash
/// (docs/06 §4/§5). An integer that does not fit `i64` degrades to a `Float`
/// string — still deterministic, so re-validation of the same edit binds.
fn json_to_canonical(value: serde_json::Value) -> CanonicalValue {
    use serde_json::Value;
    match value {
        Value::Null => CanonicalValue::Null,
        Value::Bool(b) => CanonicalValue::Bool(b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => CanonicalValue::Int(i),
            None => CanonicalValue::Float(n.to_string()),
        },
        Value::String(s) => CanonicalValue::Str(s),
        Value::Array(items) => {
            CanonicalValue::Array(items.into_iter().map(json_to_canonical).collect())
        }
        Value::Object(map) => CanonicalValue::Object(
            map.into_iter()
                .map(|(k, v)| (k, json_to_canonical(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_canonical_round_trips_a_nested_argument_tree() {
        // The argument-fidelity seam: a value that survives JSON → canonical →
        // JSON unchanged is one the human reads and the grant binds identically.
        let original = json!({
            "to": "carol@example.com",
            "count": 3,
            "flag": true,
            "tags": ["a", "b"],
            "nested": { "k": "v" }
        });
        let canonical = json_to_canonical(original.clone());
        assert_eq!(canonical_to_json(&canonical), original);
    }

    #[test]
    fn json_object_keys_sort_so_edit_order_cannot_change_the_binding() {
        let a = json_to_canonical(json!({ "b": 1, "a": 2 }));
        let b = json_to_canonical(json!({ "a": 2, "b": 1 }));
        // Same logical edit in different key order ⇒ identical canonical value,
        // hence identical hash ⇒ the grant binds the same thing (docs/06 §4).
        assert_eq!(a, b);
    }
}
