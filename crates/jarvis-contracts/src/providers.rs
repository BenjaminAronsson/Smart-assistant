//! Model-provider health DTOs (docs/05 §1, FR-11/12). Surfaces profile health,
//! quota state, and the reset window so the client can show a visible degraded
//! mode rather than a silent stall (ADR-011).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderState {
    Healthy,
    /// Reachable but degraded (e.g. rate-limited); runs may queue.
    Degraded,
    /// Unusable (quota exhausted / auth missing / network down); LLM runs queue.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuotaState {
    /// RFC 3339 instant the quota/rate window resets, when the provider
    /// reports one — the user sees *when* service returns (docs/03 §4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDto {
    /// Profile id, e.g. `claude-cli` (docs/03 §4).
    pub id: String,
    pub state: ProviderState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota: Option<QuotaState>,
    /// Stable reason code only — never raw provider/driver text (docs/06 §5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProvidersResponse {
    pub providers: Vec<ProviderDto>,
}
