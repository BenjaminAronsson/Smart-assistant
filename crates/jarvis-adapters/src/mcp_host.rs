//! MCP host (F2.7, docs/02 §8, docs/06 §5, ADR-001).
//!
//! Jarvis is an MCP **host** only (docs/03 §MCP): it launches an out-of-process
//! tool server as a child process, speaks MCP to it over stdio, imports the
//! tool **descriptors** the server exports, and **overlays host-owned
//! [`ToolPolicy`]** on each. A server's self-declared safety is never trusted
//! (docs/06 §5 "Malicious MCP/tool server"): the host decides which of the
//! server's tools exist at all and how risky each one is. Concretely:
//!
//! * A server tool the host has **no policy entry** for is **dropped** — it
//!   never becomes a registrable [`ToolDescriptor`], so it can never reach the
//!   policy engine or an executor (invariant #1). The only thing read from the
//!   server for a kept tool is its *name* (to look up the host's own policy);
//!   the server's declared annotations/descriptions never influence risk.
//! * The [`ToolPolicy`] attached to every imported descriptor comes from the
//!   host table, so a server cannot make a tool look safer (or exist) than the
//!   host allows.
//!
//! The pure overlay decision lives in [`overlay_policy`] so the security
//! property ("host policy wins / unknown tools dropped") is unit-testable
//! without spawning a child. Spawning, the initialize handshake, and
//! `list_tools`/`call_tool` live in [`McpHost`], which owns the running child.
//!
//! Result validation (schema/size/control-char hardening) and cancellation
//! reaping the child are F2.7 Slice 2; jarvisd wiring is Slice 3.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::ToolPolicy;
use jarvis_domain::tools::{
    CanonicalValue, ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion,
};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use tokio_util::sync::CancellationToken;

/// The largest tool-result text this executor forwards from an MCP `call_tool`
/// response. A hard cap at the adapter boundary bounds prompt growth from a
/// single (possibly hostile) server independently of the domain-level cap the
/// orchestrator also applies (`MAX_RESULT_PROMPT_BYTES`, CF-3) — defence in
/// depth against tool-result smuggling (docs/06 §5). Slice 2 adds full
/// control-char stripping / schema validation; this cap is the Slice-1 floor.
const MAX_MCP_RESULT_BYTES: usize = 16 * 1024;

/// Why the host could not talk to an MCP tool server. Carries no server-supplied
/// content beyond a short diagnostic string (invariant #5).
#[derive(Debug, thiserror::Error)]
pub enum McpHostError {
    #[error("failed to spawn MCP tool-server child: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("MCP initialize handshake failed: {0}")]
    Initialize(String),
    #[error("MCP list_tools failed: {0}")]
    ListTools(String),
}

/// The host's decision for one MCP tool it chooses to expose: the jarvis
/// [`ToolId`] it is registered under, the version the grant will bind, and the
/// host-owned [`ToolPolicy`]. All three are host-authored — none is derived from
/// anything the server declared (invariant #1, docs/06 §5).
#[derive(Debug, Clone)]
pub struct HostToolPolicy {
    pub tool_id: ToolId,
    pub version: ToolVersion,
    pub policy: ToolPolicy,
}

/// Host-owned overlay table, keyed by the tool **name** the MCP server exports.
/// A server-exported tool with no entry here is dropped by [`overlay_policy`];
/// the host, not the server, decides the tool catalogue (docs/06 §5).
#[derive(Debug, Default, Clone)]
pub struct HostPolicyTable {
    by_name: BTreeMap<String, HostToolPolicy>,
}

impl HostPolicyTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Map the server's `mcp_name` tool to a host [`ToolId`]/version/policy.
    pub fn insert(&mut self, mcp_name: impl Into<String>, mapping: HostToolPolicy) -> &mut Self {
        self.by_name.insert(mcp_name.into(), mapping);
        self
    }

    fn get(&self, mcp_name: &str) -> Option<&HostToolPolicy> {
        self.by_name.get(mcp_name)
    }
}

/// One server tool the host chose to keep, resolved to its host identity and
/// policy. Produced by the pure [`overlay_policy`]; [`McpHost::import_tools`]
/// turns each into a registrable [`ToolDescriptor`] by attaching an executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlaidTool {
    /// The tool name to call on the server (`CallToolRequestParams::name`).
    pub mcp_name: String,
    pub tool_id: ToolId,
    pub version: ToolVersion,
    pub policy: ToolPolicy,
}

/// Pure overlay decision (docs/06 §5): keep exactly the server-exported tool
/// names the host has a policy entry for, mapped to the host's own
/// id/version/policy; drop every other server tool. Only the tool *name* is read
/// from the server — never its declared safety — so a server can neither
/// introduce a tool the host did not sanction nor soften a tool's risk.
///
/// Determinism: the output preserves the order names are supplied in; a name the
/// server lists twice is kept twice only if the caller passes it twice (the real
/// caller passes a server's `list_tools`, which the MCP spec keys by unique
/// name). Dropped names are surfaced only via a debug log by the caller, never
/// registered.
pub fn overlay_policy<'a>(
    server_tool_names: impl IntoIterator<Item = &'a str>,
    table: &HostPolicyTable,
) -> Vec<OverlaidTool> {
    server_tool_names
        .into_iter()
        .filter_map(|name| {
            table.get(name).map(|host| OverlaidTool {
                mcp_name: name.to_owned(),
                tool_id: host.tool_id.clone(),
                version: host.version,
                policy: host.policy.clone(),
            })
        })
        .collect()
}

/// A running MCP tool-server child the host drives. Owning the
/// [`RunningService`] keeps the child process and its IO task alive; dropping
/// `McpHost` tears the child down (invariant #4 — Slice 2 makes cancellation
/// reap it deterministically). The child runs under the host's identity for now;
/// OS-identity/container isolation (docs/06 §5) is ops/host configuration
/// applied when the child is launched (Slice 3).
pub struct McpHost {
    service: RunningService<RoleClient, ()>,
    child_pid: Option<u32>,
}

impl McpHost {
    /// Spawn `command` as an MCP tool server, connect over its stdio, and
    /// complete the MCP initialize handshake. `command` is host-authored
    /// (pinned binary/args — docs/06 §5 "pinned version/hash"); this adapter
    /// never derives the command from model or tool text.
    pub async fn connect(command: tokio::process::Command) -> Result<Self, McpHostError> {
        let transport = TokioChildProcess::new(command).map_err(McpHostError::Spawn)?;
        // Capture the pid before the transport is consumed by `serve`, so Slice 2
        // can assert the child is reaped on cancellation.
        let child_pid = transport.id();
        // `()` is the no-op client handler: the host issues requests and does not
        // serve any back to the child.
        let service =
            ().serve(transport)
                .await
                .map_err(|e| McpHostError::Initialize(e.to_string()))?;
        Ok(Self { service, child_pid })
    }

    /// The child process id, if it is still known (for lifecycle/tests).
    pub fn child_pid(&self) -> Option<u32> {
        self.child_pid
    }

    /// Import the server's tool list and overlay host policy: returns a
    /// registrable [`ToolDescriptor`] for every server tool the host sanctions,
    /// dropping the rest. Each descriptor carries host [`ToolPolicy`] and an
    /// executor bound to this host's peer.
    pub async fn import_tools(
        &self,
        table: &HostPolicyTable,
    ) -> Result<Vec<ToolDescriptor>, McpHostError> {
        let server_tools = self
            .service
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| McpHostError::ListTools(e.to_string()))?;

        let names: Vec<&str> = server_tools.iter().map(|t| t.name.as_ref()).collect();
        let kept = overlay_policy(names.iter().copied(), table);

        let peer = self.service.peer().clone();
        Ok(kept
            .into_iter()
            .map(|tool| ToolDescriptor {
                id: tool.tool_id,
                version: tool.version,
                policy: Some(tool.policy),
                executor: Arc::new(McpToolExecutor {
                    peer: peer.clone(),
                    mcp_name: tool.mcp_name,
                }),
            })
            .collect())
    }

    /// Cancel the running service and reap the child. Slice 2 also wires
    /// per-call cancellation; this is the explicit host-shutdown path.
    pub async fn shutdown(self) {
        let _ = self.service.cancel().await;
    }
}

/// Executor for one imported MCP tool: forwards a policy-authorized invocation to
/// the server via `call_tool` and maps the response into a [`ToolResult`]. It
/// never re-decides authorization — the policy engine and grant validator have
/// already run (invariant #1); this only marshals arguments out and results in.
struct McpToolExecutor {
    peer: Peer<RoleClient>,
    mcp_name: String,
}

#[async_trait]
impl ToolExecutor for McpToolExecutor {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R2+ grants are validated/consumed by the orchestrator before we run.
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let arguments = canonical_object_to_json(&invocation.arguments)?;
        let mut params = CallToolRequestParams::new(self.mcp_name.clone());
        params.arguments = arguments;

        // Cancellation aborts the in-flight call promptly (invariant #4). Slice 2
        // makes cancellation additionally reap the child; here it stops awaiting.
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ToolError::Cancelled),
            outcome = self.peer.call_tool(params) => {
                outcome.map_err(|e| ToolError::ExecutionFailed(e.to_string()))?
            }
        };

        map_call_result(result)
    }
}

/// Convert an invocation's argument tree into the JSON object MCP expects.
/// Top-level `Null` sends no arguments; any non-object top level is a caller
/// (model) error surfaced without echoing the value (invariant #5).
fn canonical_object_to_json(
    args: &CanonicalValue,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, ToolError> {
    match args {
        CanonicalValue::Null => Ok(None),
        CanonicalValue::Object(map) => Ok(Some(
            map.iter()
                .map(|(k, v)| (k.clone(), canonical_to_json(v)))
                .collect(),
        )),
        _ => Err(ToolError::ExecutionFailed(
            "MCP tool arguments must be an object".to_owned(),
        )),
    }
}

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

/// Map an MCP `call_tool` response into a domain [`ToolResult`]. A tool-level
/// error (`is_error == true`) becomes [`ToolError::ExecutionFailed`]. Text
/// content blocks are concatenated and hard-capped at [`MAX_MCP_RESULT_BYTES`];
/// non-text blocks (images/audio/resources) are not forwarded to the model in
/// M2. Full control-char stripping and structured-schema validation are Slice 2;
/// the orchestrator additionally sanitizes this text at the CF-3 choke point.
fn map_call_result(result: CallToolResult) -> Result<ToolResult, ToolError> {
    let mut text = String::new();
    for block in &result.content {
        if let ContentBlock::Text(t) = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&t.text);
        }
    }

    let mut truncated = false;
    if text.len() > MAX_MCP_RESULT_BYTES {
        // Truncate on a char boundary so no partial code unit is forwarded.
        let mut end = MAX_MCP_RESULT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        truncated = true;
    }

    if result.is_error == Some(true) {
        return Err(ToolError::ExecutionFailed(if text.is_empty() {
            "MCP tool reported an error".to_owned()
        } else {
            text
        }));
    }

    Ok(ToolResult {
        content: text,
        truncated,
        compensation: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use jarvis_domain::policy::{DataEgress, RiskLevel, Scope};
    use rmcp::model::CallToolResult;

    fn r0_policy(scope: &str) -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R0,
            is_reversible: false,
            requires_user_presence: false,
            timeout: Duration::from_secs(5),
            required_scopes: [Scope::new(scope).unwrap()].into_iter().collect(),
            egress: DataEgress::Local,
        }
    }

    fn host_mapping(id: &str, scope: &str) -> HostToolPolicy {
        HostToolPolicy {
            tool_id: id.parse().unwrap(),
            version: ToolVersion::new(1, 0, 0),
            policy: r0_policy(scope),
        }
    }

    fn table() -> HostPolicyTable {
        let mut table = HostPolicyTable::new();
        table.insert("echo", host_mapping("mcp.echo", "mcp:echo"));
        table.insert("read", host_mapping("mcp.read", "mcp:read"));
        table
    }

    #[test]
    fn overlay_keeps_only_sanctioned_tools_and_drops_the_rest() {
        // The server exports a tool the host never sanctioned ("danger"); it is
        // dropped, however the server might have annotated it (docs/06 §5).
        let kept = overlay_policy(["echo", "read", "danger"], &table());
        let ids: Vec<&str> = kept.iter().map(|t| t.tool_id.as_str()).collect();
        assert_eq!(ids, ["mcp.echo", "mcp.read"], "danger must be dropped");
    }

    #[test]
    fn overlay_policy_comes_from_the_host_not_the_server() {
        // The resulting policy is exactly the host table's entry: the server has
        // no channel to influence risk, scopes, or reversibility.
        let kept = overlay_policy(["echo"], &table());
        let echo = kept.iter().find(|t| t.mcp_name == "echo").unwrap();
        assert_eq!(echo.policy.risk, RiskLevel::R0);
        assert_eq!(echo.policy.egress, DataEgress::Local);
        assert!(
            echo.policy
                .required_scopes
                .contains(&Scope::new("mcp:echo").unwrap())
        );
    }

    #[test]
    fn overlay_of_an_unknown_only_server_is_empty() {
        let kept = overlay_policy(["totally-unknown"], &table());
        assert!(kept.is_empty());
    }

    #[test]
    fn maps_text_content_and_flags_tool_errors() {
        let ok =
            map_call_result(CallToolResult::success(vec![ContentBlock::text("hello")])).unwrap();
        assert_eq!(ok.content, "hello");
        assert!(!ok.truncated);

        let err =
            map_call_result(CallToolResult::error(vec![ContentBlock::text("boom")])).unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(m) if m == "boom"));
    }

    #[test]
    fn oversized_result_text_is_capped_and_flagged() {
        let big = "x".repeat(MAX_MCP_RESULT_BYTES + 100);
        let result =
            map_call_result(CallToolResult::success(vec![ContentBlock::text(big)])).unwrap();
        assert!(result.truncated);
        assert!(result.content.len() <= MAX_MCP_RESULT_BYTES);
    }

    #[test]
    fn non_object_arguments_are_rejected() {
        let err = canonical_object_to_json(&CanonicalValue::str("nope")).unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
    }

    #[test]
    fn object_arguments_convert_to_json() {
        let args = CanonicalValue::obj([("message", CanonicalValue::str("hi"))]);
        let json = canonical_object_to_json(&args).unwrap().unwrap();
        assert_eq!(json.get("message").unwrap(), "hi");
    }
}
