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
//! Results are validated at this boundary ([`map_call_result`]:
//! control-char stripping, size cap, non-text rejection) and the child is reaped
//! on shutdown/drop. jarvisd wiring + CF-15/CF-12 are F2.7 Slice 3.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::ToolPolicy;
use jarvis_domain::tools::{
    CanonicalValue, SanitizedContent, ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion,
    sanitize_result_content,
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
/// depth against tool-result smuggling (docs/06 §5). Applied together with
/// control-char stripping in [`map_call_result`].
const MAX_MCP_RESULT_BYTES: usize = 16 * 1024;

/// Wall-clock bound on the MCP initialize handshake. A wedged or hostile child
/// (docs/06 §5) must not hang host startup indefinitely (invariant #4): if the
/// handshake does not complete in time the transport is dropped, which reaps the
/// child.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Wall-clock bound on importing the server's tool list. `list_all_tools`
/// follows MCP pagination, so a malicious server could otherwise stream pages
/// forever; the timeout caps that, and [`MAX_IMPORTED_TOOLS`] caps how many
/// tools are accepted.
const LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on the number of tools imported from one server — a defensive cap
/// at the untrusted Z3 boundary so a server cannot flood the host catalogue.
const MAX_IMPORTED_TOOLS: usize = 256;

/// Cap on an error string that may embed server-controlled text (a JSON-RPC
/// error `message`, a protocol failure). Kept short: error text is diagnostic,
/// not a payload channel.
const MAX_MCP_ERROR_BYTES: usize = 512;

/// Strip control characters and cap length from an error string derived from a
/// server response before it becomes a `ToolError`/`McpHostError`. rmcp folds a
/// hostile server's JSON-RPC error `message` into `Display`, and the orchestrator
/// reserves the full error for the host span/log (F1.5) — so a raw string here
/// could smuggle terminal escapes / control bytes into a log (docs/06 §5,
/// invariant #5). Reuses the domain result validator.
fn sanitized_error(raw: String) -> String {
    sanitize_result_content(&raw, MAX_MCP_ERROR_BYTES).text
}

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
/// `McpHost` reaps the child — rmcp's child-process transport kills it on drop —
/// and [`McpHost::shutdown`] is the graceful cancel-then-reap path (invariant
/// #4). The child runs under the host's identity for now; OS-identity/container
/// isolation (docs/06 §5) is ops/host configuration applied when the child is
/// launched (Slice 3).
pub struct McpHost {
    service: RunningService<RoleClient, ()>,
    child_pid: Option<u32>,
}

impl McpHost {
    /// Spawn `command` as an MCP tool server, connect over its stdio, and
    /// complete the MCP initialize handshake. `command` is host-authored
    /// (pinned binary/args — docs/06 §5 "pinned version/hash"); this adapter
    /// never derives the command from model or tool text.
    ///
    /// The handshake is bounded by [`CONNECT_TIMEOUT`] and cancelled by `cancel`
    /// (invariant #4): a wedged or hostile child cannot hang host startup — on
    /// timeout or cancellation the transport is dropped, reaping the child.
    pub async fn connect(
        command: tokio::process::Command,
        cancel: CancellationToken,
    ) -> Result<Self, McpHostError> {
        let transport = TokioChildProcess::new(command).map_err(McpHostError::Spawn)?;
        // Capture the pid before the transport is consumed by `serve`, so the
        // child can be identified for lifecycle/tests.
        let child_pid = transport.id();
        // `()` is the no-op client handler: the host issues requests and does not
        // serve any back to the child. Passing `cancel` lets an external cancel
        // abort the handshake (and later tear the service down).
        let service = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            ().serve_with_ct(transport, cancel),
        )
        .await
        {
            Err(_elapsed) => {
                return Err(McpHostError::Initialize(
                    "initialize handshake timed out".to_owned(),
                ));
            }
            Ok(result) => {
                result.map_err(|e| McpHostError::Initialize(sanitized_error(e.to_string())))?
            }
        };
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
    ///
    /// The list fetch is bounded by [`LIST_TOOLS_TIMEOUT`], cancelled by
    /// `cancel`, and capped at [`MAX_IMPORTED_TOOLS`] — a hostile server can
    /// neither hang the import nor flood the catalogue (invariant #4, docs/06
    /// §5). Duplicate server tool names are de-duplicated (keep first) so a
    /// server cannot smuggle a second mapping for one host [`ToolId`].
    pub async fn import_tools(
        &self,
        table: &HostPolicyTable,
        cancel: CancellationToken,
    ) -> Result<Vec<ToolDescriptor>, McpHostError> {
        let server_tools = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(McpHostError::ListTools("cancelled".to_owned()));
            }
            result = tokio::time::timeout(LIST_TOOLS_TIMEOUT, self.service.peer().list_all_tools()) => {
                match result {
                    Err(_elapsed) => {
                        return Err(McpHostError::ListTools("list_tools timed out".to_owned()));
                    }
                    Ok(listing) => {
                        listing.map_err(|e| McpHostError::ListTools(sanitized_error(e.to_string())))?
                    }
                }
            }
        };

        // Cap and de-duplicate at the untrusted boundary before overlaying policy.
        let mut seen = BTreeSet::new();
        let names: Vec<&str> = server_tools
            .iter()
            .map(|t| t.name.as_ref())
            .filter(|name| seen.insert(*name))
            .take(MAX_IMPORTED_TOOLS)
            .collect();
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

        // Per-call cancellation aborts the in-flight await promptly (invariant
        // #4); the shared child is reaped on host shutdown/drop, not per call.
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ToolError::Cancelled),
            outcome = self.peer.call_tool(params) => {
                outcome.map_err(|e| ToolError::ExecutionFailed(sanitized_error(e.to_string())))?
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

/// Map an MCP `call_tool` response into a domain [`ToolResult`], treating the
/// server as untrusted (docs/06 §5 "Tool-result smuggling", invariant #1).
///
/// The concatenated text is run through the domain result validator
/// [`sanitize_result_content`] — stripping C0/C1/DEL control characters and
/// hard-capping at [`MAX_MCP_RESULT_BYTES`] — **at this boundary**, before the
/// text can reach a host log/span or the model, rather than relying solely on
/// the orchestrator's later CF-3 sanitisation. (Unicode bidi/zero-width
/// spoofing is CF-13, handled with F2.8 `web.fetch`.)
///
/// A tool-level error (`is_error == true`) becomes [`ToolError::ExecutionFailed`]
/// carrying the sanitised text. Schema validation (docs/06 §5): M2 forwards only
/// text, so a *success* result that yields **no forwardable text** yet carried
/// non-text blocks (image/audio/resource) or `structured_content` is **rejected**
/// ([`ToolError::SchemaInvalid`]) rather than silently returned as empty — the
/// executor fails closed. The gate keys on the *sanitised* text being empty, not
/// on whether a text block was present, so a server cannot dodge it with an empty
/// (or all-control) text block alongside hostile non-text content. A genuinely
/// empty result (no blocks, no structured content) is a valid empty success.
///
/// Peak-memory note: rmcp has already fully deserialised `result` before this
/// runs, so the cap here bounds only what is forwarded downstream, not the host
/// memory a single oversized response can occupy in transit. Bounding that
/// belongs at the transport/framing layer; for M2 the child is host-launched on
/// loopback, so this is an accepted, documented limit (see the F2.7 plan).
fn map_call_result(result: CallToolResult) -> Result<ToolResult, ToolError> {
    let mut raw = String::new();
    let mut saw_non_text = false;
    for block in &result.content {
        match block {
            ContentBlock::Text(t) => {
                if !raw.is_empty() {
                    raw.push('\n');
                }
                raw.push_str(&t.text);
            }
            _ => saw_non_text = true,
        }
    }

    let SanitizedContent { text, truncated } = sanitize_result_content(&raw, MAX_MCP_RESULT_BYTES);

    if result.is_error == Some(true) {
        return Err(ToolError::ExecutionFailed(if text.is_empty() {
            "MCP tool reported an error".to_owned()
        } else {
            text
        }));
    }

    // Fail closed when nothing forwardable survives but the server did send
    // something we do not model (non-text blocks, or `structured_content` — which
    // M2 ignores entirely and must sanitise/validate before ever forwarding).
    if text.is_empty() && (saw_non_text || result.structured_content.is_some()) {
        return Err(ToolError::SchemaInvalid(
            "MCP result carried no forwardable text; non-text/structured content is unsupported in M2".to_owned(),
        ));
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
    fn control_characters_are_stripped_at_the_boundary() {
        // A server that smuggles control bytes (terminal escapes, NUL, bell) into
        // its result text cannot get them into a host log or the model: they are
        // stripped here before forwarding (docs/06 §5). \n and \t are preserved.
        // The control bytes (BEL, NUL, ESC) are stripped; \t and \n survive. The
        // literal "[31m" after the stripped ESC is ordinary text, not a control.
        let hostile = "clean\u{0007}\u{0000}\u{001b}[31mred\ttab\nnl";
        let ok =
            map_call_result(CallToolResult::success(vec![ContentBlock::text(hostile)])).unwrap();
        assert_eq!(ok.content, "clean[31mred\ttab\nnl");
    }

    #[test]
    fn non_text_only_success_is_rejected() {
        // A success result carrying only an image block (no text, no structured
        // content) is rejected rather than silently returned empty (fail closed).
        let err = map_call_result(CallToolResult::success(vec![ContentBlock::image(
            "aGVsbG8=",
            "image/png",
        )]))
        .unwrap_err();
        assert!(matches!(err, ToolError::SchemaInvalid(_)), "got {err:?}");
    }

    #[test]
    fn empty_success_is_an_empty_result_not_an_error() {
        let ok = map_call_result(CallToolResult::success(vec![])).unwrap();
        assert_eq!(ok.content, "");
        assert!(!ok.truncated);
    }

    #[test]
    fn an_empty_text_block_cannot_mask_a_dropped_non_text_block() {
        // Bypass attempt: pair an empty text block with a hostile image so a
        // `saw_text`-based gate would pass. The sanitised text is still empty and
        // a non-text block was present → reject (fail closed).
        let err = map_call_result(CallToolResult::success(vec![
            ContentBlock::text(""),
            ContentBlock::image("aGVsbG8=", "image/png"),
        ]))
        .unwrap_err();
        assert!(matches!(err, ToolError::SchemaInvalid(_)), "got {err:?}");
    }

    #[test]
    fn a_structured_only_result_is_rejected() {
        // M2 ignores `structured_content`; a result with no text but structured
        // content must not silently forward as empty.
        let mut result = CallToolResult::success(vec![]);
        result.structured_content = Some(serde_json::json!({ "answer": 42 }));
        let err = map_call_result(result).unwrap_err();
        assert!(matches!(err, ToolError::SchemaInvalid(_)), "got {err:?}");
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
