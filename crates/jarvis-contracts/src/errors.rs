//! Error code registry (docs/05 §7). Stable machine codes for client logic;
//! grows additively, codes are never renamed or reused. Every new failure mode
//! registers a code here AND in docs/05 §7 in the same PR.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum ErrorCode {
    #[serde(rename = "auth.invalid_token")]
    AuthInvalidToken,
    #[serde(rename = "auth.scope_missing")]
    AuthScopeMissing,
    #[serde(rename = "auth.pairing_invalid")]
    AuthPairingInvalid,
    #[serde(rename = "validation.failed")]
    ValidationFailed,
    #[serde(rename = "idempotency.conflict")]
    IdempotencyConflict,
    #[serde(rename = "resource.version_conflict")]
    ResourceVersionConflict,
    #[serde(rename = "resource.not_found")]
    ResourceNotFound,
    #[serde(rename = "run.budget_exceeded")]
    RunBudgetExceeded,
    #[serde(rename = "run.not_cancellable")]
    RunNotCancellable,
    #[serde(rename = "provider.unavailable")]
    ProviderUnavailable,
    #[serde(rename = "provider.quota_exhausted")]
    ProviderQuotaExhausted,
    #[serde(rename = "policy.denied")]
    PolicyDenied,
    #[serde(rename = "grant.expired")]
    GrantExpired,
    #[serde(rename = "grant.args_mismatch")]
    GrantArgsMismatch,
    #[serde(rename = "grant.consumed")]
    GrantConsumed,
    #[serde(rename = "tool.timeout")]
    ToolTimeout,
    #[serde(rename = "tool.result_invalid")]
    ToolResultInvalid,
    #[serde(rename = "artifact.too_large")]
    ArtifactTooLarge,
    /// A stored artifact blob failed content-address verification on read —
    /// on-disk corruption or tampering (F3a.2 CAS verify-on-read). 500; the
    /// bytes are never returned (fail closed).
    #[serde(rename = "artifact.integrity_failed")]
    ArtifactIntegrityFailed,
    #[serde(rename = "degraded.queued")]
    DegradedQueued,
}

/// RFC 9457 problem details body plus the stable machine `code` (docs/05 §2).
/// The gateway maps every boundary-crossing error through one `IntoProblem`
/// impl — no inline problem bodies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProblemDetails {
    /// Problem type URI; `about:blank` when the code says it all.
    #[serde(rename = "type")]
    pub problem_type: String,
    pub title: String,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    pub code: ErrorCode,
}
