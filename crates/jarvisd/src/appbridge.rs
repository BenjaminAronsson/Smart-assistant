//! The capability bridge's HTTP surface (F6.5, FR-18, docs/06 §6).
//!
//! Two routes, both behind the bearer middleware, both scoped to one app
//! version:
//!
//! * `POST …/capability-tokens` — mint a short-lived, single-use token for one
//!   **declared** capability;
//! * `POST …/invoke` — exchange a token for one operation.
//!
//! The generated app never talks to either. It runs in an opaque origin (F6.4,
//! ADR-030) with `connect-src 'none'`, so it cannot reach the network at all —
//! it `postMessage`s the shell, and the shell, holding the device token, calls
//! these routes. That indirection is deliberate: the app has no credential, so
//! there is nothing in it to steal, and every call is attributable to a paired
//! device.
//!
//! Nothing here decides anything. The whole decision lives in
//! [`jarvis_application::appbridge::AppBridge`]; this module parses, maps the
//! authenticated device onto a [`BridgeActor`], and turns typed refusals into
//! RFC 9457 problem bodies.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use jarvis_application::appbridge::{
    AppBridge, BridgeActor, BridgeError, BridgeRequest, CapabilityTokenStore,
};
use jarvis_application::orchestrator::Clock;
use jarvis_application::policy::{
    ApprovalGate, AuditSink, GrantMinter, GrantValidator, PolicyContext, ToolRegistry,
};
use jarvis_application::ports::{ArgumentDigest, ArtifactStore};
use jarvis_contracts::appbridge::{
    CapabilityResultDto, CapabilityTokenDto, InvokeCapabilityRequest, MintCapabilityTokenRequest,
};
use jarvis_contracts::errors::ErrorCode;
use jarvis_domain::appbridge::{BridgeDenial, CapabilityTokenId};
use jarvis_domain::artifact::ArtifactVersion;
use jarvis_domain::ids::{ArtifactId, RunId};
use jarvis_domain::policy::Scope;

use crate::auth::{DeviceContext, fresh_id};
use crate::problem::{not_found, problem};
use crate::time::rfc3339;

/// Everything the bridge needs, assembled once at wiring time.
#[derive(Clone)]
pub struct AppBridgeApi {
    pub artifacts: Arc<dyn ArtifactStore>,
    pub tokens: Arc<dyn CapabilityTokenStore>,
    pub registry: Arc<ToolRegistry>,
    pub audit: Arc<dyn AuditSink>,
    pub clock: Arc<dyn Clock>,
    pub approval_gate: Arc<dyn ApprovalGate>,
    pub grant_minter: Arc<dyn GrantMinter>,
    pub grant_validator: Arc<dyn GrantValidator>,
    pub arg_digest: Arc<dyn ArgumentDigest>,
}

impl AppBridgeApi {
    fn bridge<'a>(&'a self, context: PolicyContext) -> AppBridge<'a> {
        AppBridge {
            artifacts: &*self.artifacts,
            tokens: &*self.tokens,
            registry: &self.registry,
            audit: &*self.audit,
            clock: &*self.clock,
            approval_gate: &*self.approval_gate,
            grant_minter: &*self.grant_minter,
            grant_validator: &*self.grant_validator,
            arg_digest: &*self.arg_digest,
            granted_scopes: context,
        }
    }
}

/// The device's own scopes, verbatim. The bridge widens nothing: an app can
/// never reach a tool whose scope the owner's session does not already hold.
fn context_of(device: &DeviceContext) -> PolicyContext {
    PolicyContext {
        user_id: device.user_id.clone(),
        device_id: device.device_id.clone(),
        granted_scopes: device
            .scopes
            .iter()
            .filter_map(|s| Scope::new(s).ok())
            .collect(),
    }
}

fn actor_of(device: &DeviceContext) -> BridgeActor {
    BridgeActor {
        user_id: device.user_id.clone(),
        device_id: device.device_id.clone(),
        // Host-minted correlation id: there is no conversational run behind an
        // app-originated call, but a grant binds to one and the audit trail
        // correlates by one.
        run_id: fresh_id::<RunId>(),
    }
}

/// Which path segment failed to parse. A unit-sized enum rather than a prebuilt
/// `Response`, for the same reason as `IdFault` in [`crate::lists`]: an axum
/// `Response` is large, and returning one in a helper's `Err` makes the result
/// enormous (clippy `result_large_err`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathFault {
    App,
    Version,
}

fn parse_path(id: &str, version: &str) -> Result<(ArtifactId, ArtifactVersion), PathFault> {
    let id = id.parse::<ArtifactId>().map_err(|_| PathFault::App)?;
    let version = version
        .parse::<u32>()
        .ok()
        .and_then(ArtifactVersion::new)
        .ok_or(PathFault::Version)?;
    Ok((id, version))
}

fn path_problem(fault: PathFault) -> Response {
    not_found(match fault {
        PathFault::App => "no such app",
        PathFault::Version => "no such app version",
    })
}

/// `POST /api/v1/apps/{id}/versions/{version}/capability-tokens`
pub async fn mint_token(
    State(api): State<AppBridgeApi>,
    Extension(device): Extension<DeviceContext>,
    Path((id, version)): Path<(String, String)>,
    Json(body): Json<MintCapabilityTokenRequest>,
) -> Result<Json<CapabilityTokenDto>, Response> {
    let (id, version) = parse_path(&id, &version).map_err(path_problem)?;
    let capability = body.capability.into();
    let token = api
        .bridge(context_of(&device))
        .mint_token(&id, version, capability, &actor_of(&device))
        .await
        .map_err(bridge_problem)?;

    Ok(Json(CapabilityTokenDto {
        token: token.id.to_string(),
        expires_at: rfc3339(token.expires_at),
        capability: body.capability,
    }))
}

/// `POST /api/v1/apps/{id}/versions/{version}/invoke`
pub async fn invoke(
    State(api): State<AppBridgeApi>,
    Extension(device): Extension<DeviceContext>,
    Path((id, version)): Path<(String, String)>,
    Json(body): Json<InvokeCapabilityRequest>,
) -> Result<Json<CapabilityResultDto>, Response> {
    let (id, version) = parse_path(&id, &version).map_err(path_problem)?;
    let token = body.token.parse::<CapabilityTokenId>().map_err(|_| {
        // A malformed token is refused with the same code a rejected one gets:
        // a caller learns nothing about the token space from the shape of the
        // refusal.
        problem(
            StatusCode::FORBIDDEN,
            ErrorCode::AppTokenRejected,
            "capability token rejected",
            None,
        )
    })?;

    let result = api
        .bridge(context_of(&device))
        .exchange(
            BridgeRequest {
                artifact_id: id,
                version,
                capability: body.capability.into(),
                target: body.target,
                value: body.value,
                token,
            },
            &actor_of(&device),
            // The request's own lifetime bounds the operation; the host-owned
            // per-tool timeout inside the registry is what stops a hung tool
            // (invariant 4).
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .map_err(bridge_problem)?;

    Ok(Json(CapabilityResultDto {
        content: result.content,
        truncated: result.truncated,
    }))
}

/// One mapping for every way the bridge can refuse (docs/05 §7). Notice what is
/// *not* here: no branch leaks which binding a token failed, and no branch
/// echoes tool or policy internals — a refused app learns that it was refused.
fn bridge_problem(error: BridgeError) -> Response {
    match error {
        BridgeError::Denied(BridgeDenial::UndeclaredCapability(_)) => problem(
            StatusCode::FORBIDDEN,
            ErrorCode::AppUndeclaredCapability,
            "this app does not declare that capability",
            None,
        ),
        BridgeError::Denied(BridgeDenial::Token(_)) => problem(
            StatusCode::FORBIDDEN,
            ErrorCode::AppTokenRejected,
            "capability token rejected",
            None,
        ),
        BridgeError::Denied(BridgeDenial::UnknownApp | BridgeDenial::NotAnApp(_)) => {
            not_found("no such app version")
        }
        BridgeError::Denied(BridgeDenial::InvalidTarget) => problem(
            StatusCode::BAD_REQUEST,
            ErrorCode::AppInvalidRequest,
            "the request named an unusable target or value",
            None,
        ),
        BridgeError::Policy(_) => problem(
            StatusCode::FORBIDDEN,
            ErrorCode::PolicyDenied,
            "policy denied this operation",
            None,
        ),
        BridgeError::NotApproved => problem(
            StatusCode::FORBIDDEN,
            ErrorCode::PolicyDenied,
            "the operation was not approved",
            None,
        ),
        BridgeError::Grant(_) | BridgeError::GrantRejected(_) => problem(
            StatusCode::FORBIDDEN,
            ErrorCode::GrantExpired,
            "no valid execution grant",
            None,
        ),
        BridgeError::Tool(_) => problem(
            StatusCode::BAD_GATEWAY,
            ErrorCode::ToolResultInvalid,
            "the operation failed",
            None,
        ),
        BridgeError::Cancelled => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::DegradedQueued,
            "the operation was cancelled",
            None,
        ),
        BridgeError::Storage => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::ProviderUnavailable,
            "storage unavailable",
            None,
        ),
    }
}
