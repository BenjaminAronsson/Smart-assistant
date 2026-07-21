//! Integration tests for the MCP host adapter (F2.7 Slice 1) against a real
//! child process — the in-workspace `mcp-echo-fixture` server. These live in the
//! fixture crate so `CARGO_BIN_EXE_mcp-echo-fixture` resolves to the built
//! binary; the crate dev-depends on `jarvis-adapters` to drive `McpHost`.
//!
//! They prove the security-critical import behaviour end to end (docs/06 §5):
//! the host imports the server's descriptors, **drops** any tool it has no
//! policy for, and attaches **host-owned** policy to the rest — regardless of
//! the (deliberately mismatched) safety annotations the fixture declares. A
//! round-trip `echo` call proves the imported executor actually reaches the
//! child. The pure overlay decision is unit-tested in the adapter crate itself.

use std::time::Duration;

use jarvis_adapters::mcp_host::{HostPolicyTable, HostToolPolicy, McpHost};
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{CanonicalValue, ToolId, ToolInvocation, ToolVersion};
use tokio_util::sync::CancellationToken;

/// Path to the fixture server binary Cargo built for this test.
const FIXTURE_BIN: &str = env!("CARGO_BIN_EXE_mcp-echo-fixture");

fn fixture_command() -> tokio::process::Command {
    tokio::process::Command::new(FIXTURE_BIN)
}

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

fn mapping(tool_id: &str, scope: &str) -> HostToolPolicy {
    HostToolPolicy {
        tool_id: tool_id.parse().unwrap(),
        version: ToolVersion::new(1, 0, 0),
        policy: r0_policy(scope),
    }
}

/// Host table that sanctions `echo` and `read` but NOT the fixture's `danger`.
fn host_table() -> HostPolicyTable {
    let mut table = HostPolicyTable::new();
    table.insert("echo", mapping("mcp.echo", "mcp:echo"));
    table.insert("read", mapping("mcp.read", "mcp:read"));
    table
}

#[tokio::test]
async fn imports_only_sanctioned_tools_with_host_policy() {
    let host = McpHost::connect(fixture_command(), CancellationToken::new())
        .await
        .expect("connect to fixture");

    let mut descriptors = host
        .import_tools(&host_table(), CancellationToken::new())
        .await
        .expect("import tools");
    descriptors.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));

    // `danger` — which the fixture annotates as read-only/non-destructive ("safe")
    // — is absent: a server cannot introduce a tool the host never sanctioned.
    let ids: Vec<&str> = descriptors.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, ["mcp.echo", "mcp.read"], "danger must be dropped");

    // The overlaid policy is the host's, not the server's. The fixture annotates
    // `read` as **destructive**; the host classifies it R0 read-only anyway.
    let read = descriptors
        .iter()
        .find(|d| d.id.as_str() == "mcp.read")
        .unwrap();
    let policy = read.policy.as_ref().expect("host policy attached");
    assert_eq!(policy.risk, RiskLevel::R0);
    assert!(!policy.requires_grant());
    assert_eq!(policy.egress, DataEgress::Local);

    host.shutdown().await;
}

#[tokio::test]
async fn imported_executor_round_trips_a_call_to_the_child() {
    let host = McpHost::connect(fixture_command(), CancellationToken::new())
        .await
        .expect("connect to fixture");
    let descriptors = host
        .import_tools(&host_table(), CancellationToken::new())
        .await
        .expect("import tools");

    let echo = descriptors
        .iter()
        .find(|d| d.id.as_str() == "mcp.echo")
        .unwrap();
    let echo_id: ToolId = "mcp.echo".parse().unwrap();
    let invocation = ToolInvocation {
        tool_id: echo_id,
        tool_version: echo.version,
        arguments: CanonicalValue::obj([("message", CanonicalValue::str("ping"))]),
    };

    let result = echo
        .executor
        .execute(invocation, None, CancellationToken::new())
        .await
        .expect("echo executes");
    assert_eq!(result.content, "ping");
    assert!(!result.truncated);

    host.shutdown().await;
}
