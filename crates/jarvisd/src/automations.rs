//! Automation REST surface and the daemon driver (FR-17, F8.7).
//!
//! Two things live here: the routes `docs/05 §1` has advertised since M0, and
//! the loop that actually sweeps triggers — the half that makes an automation
//! something that happens rather than something that is stored.
//!
//! The creator of an automation is taken from the **authenticated device**, never
//! from the request body. A client that could name its own `createdByDeviceId`
//! could borrow another device's authority at every future fire time, which is
//! the escalation the whole design exists to prevent.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use jarvis_application::automations::AutomationService;
use jarvis_application::ports::AutomationStore;
use jarvis_contracts::automations::{
    AutomationDto, AutomationExecutionDto, AutomationHistoryResponse, AutomationListResponse,
    CreateAutomationRequest, TriggerDto, UpdateAutomationRequest,
};
use jarvis_contracts::errors::ErrorCode;
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::automations::{
    Automation, AutomationAction, AutomationExecution, AutomationName, ExecutionOutcome, Trigger,
};
use jarvis_domain::ids::AutomationId;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio_util::sync::CancellationToken;

use crate::problem::problem;

/// How often the clock sweep runs.
///
/// One minute, matching the resolution of a `daily_at` trigger: a finer tick
/// would burn wakeups on an 8 GB target for a trigger that cannot express
/// anything smaller (docs/09 §5).
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct AutomationApi {
    store: Arc<dyn AutomationStore>,
}

impl AutomationApi {
    pub fn new(store: Arc<dyn AutomationStore>) -> Self {
        Self { store }
    }
}

fn rfc3339(t: SystemTime) -> String {
    OffsetDateTime::from(t)
        .format(&Rfc3339)
        .expect("UTC timestamp formats")
}

fn trigger_dto(trigger: &Trigger) -> TriggerDto {
    match trigger {
        Trigger::DailyAt {
            minutes_since_midnight,
        } => TriggerDto::DailyAt {
            minutes_since_midnight: *minutes_since_midnight,
        },
        Trigger::HomeAssistantState { entity_id, state } => TriggerDto::HomeAssistantState {
            entity_id: entity_id.clone(),
            state: state.clone(),
        },
    }
}

/// Why a trigger on the wire is not a trigger.
///
/// A small enum rather than a `Response` for the reason `lists::IdFault`
/// exists: an axum `Response` is large, and putting one in a helper's `Err`
/// makes every caller's `Result` enormous (clippy `result_large_err`).
#[derive(Debug, PartialEq, Eq)]
enum TriggerFault {
    MinuteOutOfRange,
    EmptyEntityOrState,
}

impl TriggerFault {
    fn detail(self) -> &'static str {
        match self {
            Self::MinuteOutOfRange => "minutesSinceMidnight must be 0–1439",
            Self::EmptyEntityOrState => "entityId and state must be non-empty",
        }
    }
}

fn trigger_from(dto: TriggerDto) -> Result<Trigger, TriggerFault> {
    Ok(match dto {
        TriggerDto::DailyAt {
            minutes_since_midnight,
        } => {
            if minutes_since_midnight >= 1440 {
                return Err(TriggerFault::MinuteOutOfRange);
            }
            Trigger::DailyAt {
                minutes_since_midnight,
            }
        }
        TriggerDto::HomeAssistantState { entity_id, state } => {
            if entity_id.trim().is_empty() || state.trim().is_empty() {
                return Err(TriggerFault::EmptyEntityOrState);
            }
            Trigger::HomeAssistantState { entity_id, state }
        }
    })
}

pub fn to_dto(automation: &Automation) -> AutomationDto {
    AutomationDto {
        id: automation.id().clone(),
        name: automation.name().as_str().to_owned(),
        trigger: trigger_dto(automation.trigger()),
        tool_id: automation.action().tool_id.as_str().to_owned(),
        arguments: jarvis_infra::canonical::canonical_to_json(&automation.action().arguments),
        enabled: automation.is_enabled(),
        created_by_device_id: automation.created_by().clone(),
        created_at: rfc3339(automation.created_at()),
        last_fired_at: automation.last_fired_at().map(rfc3339),
    }
}

fn execution_dto(execution: &AutomationExecution) -> AutomationExecutionDto {
    let detail = match &execution.outcome {
        ExecutionOutcome::Executed => None,
        ExecutionOutcome::NeedsApproval { exact_effect } => Some(exact_effect.clone()),
        ExecutionOutcome::Denied { reason } | ExecutionOutcome::Failed { reason } => {
            Some(reason.clone())
        }
    };
    AutomationExecutionDto {
        occurred_at: rfc3339(execution.occurred_at),
        outcome: execution.outcome.as_str().to_owned(),
        detail,
    }
}

fn storage_problem(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "automation storage failed");
    problem(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::ProviderUnavailable,
        "automations are unavailable",
        None,
    )
}

/// `GET /api/v1/automations`
pub async fn index(
    State(api): State<AutomationApi>,
) -> Result<Json<AutomationListResponse>, Response> {
    let automations = api.store.list_all().await.map_err(storage_problem)?;
    Ok(Json(AutomationListResponse {
        automations: automations.iter().map(to_dto).collect(),
    }))
}

/// `POST /api/v1/automations`
pub async fn create(
    State(api): State<AutomationApi>,
    axum::Extension(device): axum::Extension<crate::auth::DeviceContext>,
    Json(request): Json<CreateAutomationRequest>,
) -> Result<(StatusCode, Json<AutomationDto>), Response> {
    let name = AutomationName::new(&request.name).map_err(|e| {
        problem(
            StatusCode::BAD_REQUEST,
            ErrorCode::ValidationFailed,
            &format!("name: {e}"),
            None,
        )
    })?;
    let trigger = trigger_from(request.trigger).map_err(|fault| {
        problem(
            StatusCode::BAD_REQUEST,
            ErrorCode::ValidationFailed,
            fault.detail(),
            None,
        )
    })?;
    let tool_id = request.tool_id.parse().map_err(|_| {
        problem(
            StatusCode::BAD_REQUEST,
            ErrorCode::ValidationFailed,
            "toolId is not a valid tool identifier",
            None,
        )
    })?;

    let now = SystemTime::now();
    let automation = Automation::create(
        crate::auth::fresh_id(),
        name,
        trigger,
        AutomationAction {
            tool_id,
            arguments: jarvis_infra::canonical::json_to_canonical(request.arguments),
        },
        // From the authenticated device, never from the body: otherwise a
        // client could create an automation that borrows another device's
        // authority at every future fire time.
        device.device_id.clone(),
        now,
    );

    let audit = AuditEvent {
        occurred_at: now,
        actor: format!("device:{}", device.device_id),
        event_type: "automation.created".into(),
        target: format!("automation:{}", automation.id()),
        correlation_id: None,
        // Closed-vocabulary values only — never the automation's name, which is
        // human text and has no business in a hashed audit payload.
        payload_json: serde_json::json!({
            "triggerKind": automation.trigger().kind(),
            "toolId": automation.action().tool_id.as_str(),
        })
        .to_string(),
    };

    api.store
        .create(&automation, &audit)
        .await
        .map_err(storage_problem)?;
    Ok((StatusCode::CREATED, Json(to_dto(&automation))))
}

/// `PATCH /api/v1/automations/{id}` — enable or disable, and nothing else.
pub async fn update(
    State(api): State<AutomationApi>,
    Path(id): Path<String>,
    Json(request): Json<UpdateAutomationRequest>,
) -> Result<StatusCode, Response> {
    let id: AutomationId = id.parse().map_err(|_| not_found())?;
    api.store
        .set_enabled(&id, request.enabled)
        .await
        .map_err(storage_problem)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/v1/automations/{id}`
pub async fn delete(
    State(api): State<AutomationApi>,
    Path(id): Path<String>,
) -> Result<StatusCode, Response> {
    let id: AutomationId = id.parse().map_err(|_| not_found())?;
    api.store.delete(&id).await.map_err(storage_problem)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/automations/{id}/history`
pub async fn history(
    State(api): State<AutomationApi>,
    Path(id): Path<String>,
) -> Result<Json<AutomationHistoryResponse>, Response> {
    let id: AutomationId = id.parse().map_err(|_| not_found())?;
    let executions = api.store.history(&id, 50).await.map_err(storage_problem)?;
    Ok(Json(AutomationHistoryResponse {
        executions: executions.iter().map(execution_dto).collect(),
    }))
}

fn not_found() -> Response {
    problem(
        StatusCode::NOT_FOUND,
        ErrorCode::ResourceNotFound,
        "no such automation",
        None,
    )
}

/// Minutes since local midnight.
fn minutes_since_midnight(now: OffsetDateTime) -> u16 {
    u16::from(now.hour()) * 60 + u16::from(now.minute())
}

/// The clock sweep (F8.7).
///
/// Ticks once a minute and fires whatever fell in the window since the last
/// tick — a *window* rather than an equality test, so an automation is not
/// skipped because nothing asked at exactly 07:00:00.
///
/// A storage failure logs and continues rather than killing the loop: the whole
/// point of this task is that it is still here tomorrow morning.
pub async fn run_scheduler(service: Arc<AutomationService>, shutdown: CancellationToken) {
    tracing::info!("automation scheduler started");
    let mut previous = minutes_since_midnight(OffsetDateTime::now_utc());

    // The restart sweep, in the same shape timers already use (M8b): anything
    // whose moment passed while this process was not running is ANNOUNCED, not
    // fired. Acting on "turn the lights on at 07:00" at 11:00 is worse than
    // not acting — the reason the owner wanted it has passed — but saying
    // nothing is worse still, because "the automation is broken" and "the
    // daemon was off" need very different responses from them.
    //
    // The stamp is read from storage rather than handed in (M8b gate D2). It
    // used to be a `None` this binary had nothing to fill in with, which made
    // the whole restart report inert in production while its tests passed.
    let down_since = match service.last_heartbeat().await {
        Ok(stamp) => stamp,
        Err(e) => {
            tracing::error!(error = %e, "could not read the last-seen stamp");
            None
        }
    };
    match down_since {
        // A first start has no downtime to report, only an uptime to begin.
        None => tracing::info!("no previous run recorded; nothing to report as missed"),
        Some(down_since) => match service
            .missed_between(down_since, std::time::SystemTime::now())
            .await
        {
            Ok(missed) => {
                for item in &missed {
                    tracing::warn!(
                        automation_id = %item.automation_id,
                        name = %item.name,
                        "automation was missed while the daemon was down; not run late"
                    );
                }
                if missed.is_empty() {
                    tracing::info!("no automations were missed while the daemon was down");
                }
            }
            Err(e) => tracing::error!(error = %e, "could not check for missed automations"),
        },
    }

    while !shutdown.is_cancelled() {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(SWEEP_INTERVAL) => {}
        }

        let now_local = OffsetDateTime::now_utc();
        let now_minutes = minutes_since_midnight(now_local);
        match service
            .sweep_clock(previous, now_minutes, SystemTime::now())
            .await
        {
            Ok(fired) => {
                for f in &fired {
                    // A refusal is logged as loudly as a success: an automation
                    // that quietly stopped working is the failure people
                    // actually hit.
                    match &f.outcome {
                        ExecutionOutcome::Executed => {
                            tracing::info!(automation_id = %f.automation_id, "automation fired");
                        }
                        other => tracing::warn!(
                            automation_id = %f.automation_id,
                            outcome = other.as_str(),
                            "automation fired but did not act"
                        ),
                    }
                }
            }
            Err(e) => tracing::error!(error = %e, "automation sweep failed; continuing"),
        }
        previous = now_minutes;

        // Stamp liveness *after* the sweep, so the recorded instant is one the
        // daemon actually reached rather than one it merely started. Written on
        // the tick rather than at shutdown on purpose: the downtime worth
        // reporting is the one nobody planned, and a daemon that was killed is
        // exactly the case a shutdown hook does not cover. A failure here is
        // logged and skipped — losing a minute of resolution on the next
        // restart report is not worth stopping the scheduler for.
        if let Err(e) = service.record_heartbeat(SystemTime::now()).await {
            tracing::warn!(error = %e, "could not record the last-seen stamp");
        }
    }
    tracing::info!("automation scheduler stopped");
}

// ---------------------------------------------------------------------------
// The daemon's implementations of the two fire-time ports
// ---------------------------------------------------------------------------

/// Resolves a device's authority from the **live** device row (F8.6).
///
/// This is where "policy re-evaluated at fire time" stops being a slogan: the
/// row is read on every firing, a revoked device is simply absent, and the
/// scopes come from the device *class* exactly as they do for a live request
/// (F7.1 — a device never names its own authority).
pub struct StoreAuthority {
    identity: Arc<dyn jarvis_application::ports::IdentityStore>,
}

impl StoreAuthority {
    pub fn new(identity: Arc<dyn jarvis_application::ports::IdentityStore>) -> Self {
        Self { identity }
    }
}

#[async_trait::async_trait]
impl jarvis_application::automations::DeviceAuthority for StoreAuthority {
    async fn scopes_of(
        &self,
        device: &jarvis_domain::ids::DeviceId,
    ) -> Option<(
        jarvis_domain::ids::UserId,
        std::collections::BTreeSet<jarvis_domain::policy::Scope>,
    )> {
        // A storage failure reads as "no authority", not as "carry on". Failing
        // closed is the only safe direction here: the alternative is acting on
        // a database blip.
        let device = self
            .identity
            .find_active_device_by_id(device)
            .await
            .ok()
            .flatten()?;
        let scopes = device
            .effective_scopes()
            .into_iter()
            .filter_map(|s| jarvis_domain::policy::Scope::new(s).ok())
            .collect();
        Some((device.user_id, scopes))
    }
}

/// Runs an authorized proposal through the ordinary tool registry.
///
/// The same registry, the same executors, the same timeouts as any other tool
/// call — an automation is not a second execution path, which is what keeps
/// invariant 1 from having a back door.
pub struct RegistryExecutor {
    registry: Arc<jarvis_application::policy::ToolRegistry>,
}

impl RegistryExecutor {
    pub fn new(registry: Arc<jarvis_application::policy::ToolRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait::async_trait]
impl jarvis_application::automations::AutomationExecutor for RegistryExecutor {
    async fn execute(&self, proposal: &jarvis_domain::tools::ToolProposal) -> Result<(), String> {
        let Some((tool_version, executor)) = self.registry.resolve(&proposal.tool_id) else {
            return Err("tool is not registered".to_owned());
        };
        let invocation = jarvis_domain::tools::ToolInvocation {
            tool_id: proposal.tool_id.clone(),
            tool_version,
            arguments: proposal.arguments.clone(),
        };
        executor
            // No grant: `decide_at_fire_time` only returns a proposal for an
            // `Auto` decision, and anything needing a grant was already
            // recorded as a refusal (nobody is awake to approve it).
            .execute(invocation, None, CancellationToken::new())
            .await
            // Neutral text only — never a raw adapter string (docs/06 §5).
            .map(|_| ())
            .map_err(|e: jarvis_domain::tools::ToolError| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_since_midnight_is_local_wall_clock() {
        let at_seven = OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .expect("timestamp")
            .replace_hour(7)
            .expect("hour")
            .replace_minute(0)
            .expect("minute");
        assert_eq!(minutes_since_midnight(at_seven), 420);

        let midnight = at_seven.replace_hour(0).expect("hour");
        assert_eq!(minutes_since_midnight(midnight), 0);

        let last_minute = at_seven
            .replace_hour(23)
            .expect("hour")
            .replace_minute(59)
            .expect("minute");
        assert_eq!(minutes_since_midnight(last_minute), 1439);
    }

    #[test]
    fn a_trigger_dto_round_trips_through_the_domain() {
        for dto in [
            TriggerDto::DailyAt {
                minutes_since_midnight: 420,
            },
            TriggerDto::HomeAssistantState {
                entity_id: "person.owner".into(),
                state: "home".into(),
            },
        ] {
            let domain = trigger_from(dto.clone()).expect("valid");
            assert_eq!(trigger_dto(&domain), dto);
        }
    }

    /// The wire is validated, not trusted: an out-of-range minute would store a
    /// trigger the database CHECK would reject, or worse, one that never fires.
    #[test]
    fn an_out_of_range_trigger_is_refused() {
        assert!(
            trigger_from(TriggerDto::DailyAt {
                minutes_since_midnight: 1440
            })
            .is_err()
        );
        assert!(
            trigger_from(TriggerDto::HomeAssistantState {
                entity_id: "  ".into(),
                state: "home".into()
            })
            .is_err()
        );
    }
}
