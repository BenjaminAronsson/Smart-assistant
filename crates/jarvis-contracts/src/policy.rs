//! Read-only policy view for `GET /api/v1/policy` (F10.5, FR-05, docs/12).
//!
//! What each tool may do, what needs approval, and what a given device class is
//! actually allowed — the thing an owner most needs to *see*, and which was
//! config-only until now.
//!
//! # Read-only, deliberately
//!
//! There is no write DTO here. Changing risk tiers from a web page is a far
//! bigger authority question than changing a wake word, and F8.8's consent-gate
//! amendment is the precedent for how carefully that has to be argued. The view
//! ships now; write access belongs in an ADR first.
//!
//! # The failure this shape exists to prevent
//!
//! **A UI that describes different rules than the engine enforces is worse than
//! none** — it converts "I don't know what this will do" into confident, wrong
//! belief. So [`ToolPolicyDto::outcomes`] does not carry a *description* of the
//! rules from which a reader might infer a decision; it carries the decision
//! itself, obtained by asking `policy::evaluate` and recording its answer. The
//! projection cannot drift from the engine by paraphrasing it, because it never
//! paraphrases it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What `policy::evaluate` returns for one (tool, device class) pair, flattened
/// for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PolicyOutcomeDto {
    /// Runs without asking (still evaluated and audited — there is no
    /// skip-policy path, docs/06 §3).
    Auto,
    /// Requires an explicit approval carrying the exact effect.
    NeedsApproval,
    /// Refused, with the engine's own reason.
    Denied {
        /// A stable, human-readable reason: `unknown_tool`, `prohibited`, or
        /// `missing_scope:<scope>`. Rendered from `DenyReason`, never invented.
        reason: String,
    },
}

/// One tool, as the engine treats it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolPolicyDto {
    pub tool_id: String,
    /// `R0`–`R4` (docs/06 §2).
    pub risk: String,
    /// Whether an execution can be undone. Shown because it is what an owner
    /// weighs, and it is host-owned metadata — never tool-declared.
    pub reversible: bool,
    /// Whether the tool may only run with the owner present.
    pub requires_user_presence: bool,
    /// `none` | `local` | `external` — how far this tool's data travels.
    pub egress: String,
    /// Scopes a caller must hold. A device class holding none of these is not
    /// merely restricted, it is refused.
    pub required_scopes: Vec<String>,
    /// **The decision itself**, per device class, straight from
    /// `policy::evaluate`. Keyed by class name (`owner-ui`, `room-node`, …).
    pub outcomes: Vec<ClassOutcomeDto>,
}

/// One device class's actual outcome for a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClassOutcomeDto {
    /// `owner-ui` | `display-node` | `voice-node` | `room-node`.
    pub device_class: String,
    #[serde(flatten)]
    pub outcome: PolicyOutcomeDto,
}

/// `GET /api/v1/policy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PolicyViewDto {
    /// Every registered tool, in the registry's stable order.
    ///
    /// A tool absent here is not callable at all — the registry *is* the
    /// catalogue, there is no ambient tool set — so a short list is a true
    /// answer to "what can this house do", not a truncated one.
    pub tools: Vec<ToolPolicyDto>,
}
