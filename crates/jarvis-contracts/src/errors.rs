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
    /// No media player is running, so an untargeted transport command has
    /// nothing to act on (F3a.7, FR-22). 409. Distinct from
    /// `media.target_ambiguous`: the client shows an empty state rather than
    /// asking which player.
    #[serde(rename = "media.nothing_playing")]
    MediaNothingPlaying,
    /// Two or more players are active and the request named none, so the server
    /// refuses to guess (ADR-016 — the choice is asked, never inferred). 409.
    /// The client's cue to ask which player and retry with `player` set.
    #[serde(rename = "media.target_ambiguous")]
    MediaTargetAmbiguous,
    /// The named player left the bus between the snapshot and the command. 409.
    /// A normal race, and safely retryable after a state refresh — which is why
    /// it is not `resource.not_found` (that means "no such id, ever").
    #[serde(rename = "media.player_gone")]
    MediaPlayerGone,
    /// The player is present but reports it cannot perform this control
    /// (`CanGoNext = false`). 409 — the request was well-formed, so this is not
    /// `validation.failed`; retrying the same call will not help, but a
    /// different control on the same player may.
    #[serde(rename = "media.control_unsupported")]
    MediaControlUnsupported,
    /// The requested verb is not legal for the timer's current state — snoozing
    /// one that has not rung, cancelling one that already did (F3b.7, ADR-023).
    /// 409: the request was well-formed, so it is not `validation.failed`, and
    /// retrying it unchanged will not help; the client re-reads the timer first.
    #[serde(rename = "timer.invalid_transition")]
    TimerInvalidTransition,
    /// The timer moved between the client's read and its decision — it fired in
    /// the same instant, or another device answered first. 409 and **safely
    /// retryable** after a refresh, which is why it is distinct from
    /// `timer.invalid_transition`.
    #[serde(rename = "timer.stale")]
    TimerStale,
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
