//! Live tool-registry composition (F2.6 Slice 3b, docs/06 §3/§8).
//!
//! This is the **single registration site**. Every executor is wrapped in a
//! [`TimeoutExecutor`] built from the tool's own host-owned `ToolPolicy.timeout`
//! *before* it enters the registry, so no tool can ship without a deadline
//! (CF-11; docs/06 §8 gate 3, "every R2/R3 tool has a timeout"). The wrap is
//! applied uniformly here rather than per-tool so a newly added tool cannot
//! silently opt out.
//!
//! Registration never trusts a descriptor's declared safety: a descriptor
//! arriving without host `ToolPolicy` is refused by [`ToolRegistry::register`]
//! (invariant #1, docs/06 §5). The tools here are host-authored and always carry
//! policy, but the refusal path is what will guard MCP-imported descriptors
//! (F2.7).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use jarvis_adapters::tools::example_light::ExampleLightTool;
use jarvis_adapters::tools::example_message::ExampleMessageTool;
use jarvis_adapters::tools::fs_read::FsReadTool;
use jarvis_adapters::tools::timeout::TimeoutExecutor;
use jarvis_application::policy::{ToolDescriptor, ToolRegistry};

/// Build the M2 tool registry. `fs_root`, when present, is the allowlisted root
/// for the R0 `fs.read` tool; when `None`, `fs.read` is **not** registered — the
/// stricter default (no ambient filesystem-read authority until the host
/// explicitly configures a root). The reversible R1 `example.light` and the
/// external R2 `message.send` demonstrations need no host configuration and are
/// always registered.
///
/// Every executor is timeout-wrapped ([`wrap_with_timeout`]) at registration.
pub fn build_registry(fs_root: Option<PathBuf>) -> anyhow::Result<ToolRegistry> {
    let mut registry = ToolRegistry::new();

    if let Some(root) = fs_root {
        let descriptor = FsReadTool::descriptor(&root)
            .with_context(|| format!("fs.read root {} is unreadable", root.display()))?;
        registry.register(wrap_with_timeout(descriptor))?;
    }
    registry.register(wrap_with_timeout(ExampleLightTool::descriptor()))?;
    registry.register(wrap_with_timeout(ExampleMessageTool::descriptor()))?;

    Ok(registry)
}

/// Replace a descriptor's executor with one bounded by the tool's host-owned
/// `ToolPolicy.timeout`. A descriptor with no policy is left untouched so the
/// registry's own `MissingPolicy` refusal (not a silent unbounded execution) is
/// what rejects it.
fn wrap_with_timeout(descriptor: ToolDescriptor) -> ToolDescriptor {
    match descriptor.policy.as_ref().map(|p| p.timeout) {
        Some(timeout) => ToolDescriptor {
            executor: TimeoutExecutor::wrap(Arc::clone(&descriptor.executor), timeout),
            ..descriptor
        },
        None => descriptor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_the_two_config_free_tools_without_a_root() {
        let registry = build_registry(None).expect("builds");
        assert!(
            registry.policy_of(&ExampleLightTool::id()).is_some(),
            "example.light is registered"
        );
        assert!(
            registry.policy_of(&ExampleMessageTool::id()).is_some(),
            "message.send is registered"
        );
        assert!(
            registry.policy_of(&FsReadTool::id()).is_none(),
            "fs.read is absent without a configured root (stricter default)"
        );
    }

    #[test]
    fn registers_fs_read_when_a_root_is_configured() {
        // The crate root always exists and canonicalizes — a valid allowlist root.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let registry = build_registry(Some(root)).expect("builds");
        assert!(
            registry.policy_of(&FsReadTool::id()).is_some(),
            "fs.read is registered against the configured root"
        );
    }

    #[test]
    fn every_registered_tool_resolves_to_an_executor() {
        // The timeout wrap must not drop resolvability: each tool still resolves
        // (to its TimeoutExecutor-wrapped executor) after registration.
        let registry = build_registry(None).expect("builds");
        assert!(registry.resolve(&ExampleLightTool::id()).is_some());
        assert!(registry.resolve(&ExampleMessageTool::id()).is_some());
    }

    #[test]
    fn a_missing_fs_root_is_a_clean_error_not_a_panic() {
        let missing = PathBuf::from("/no/such/jarvis/root/at/all");
        match build_registry(Some(missing)) {
            Err(error) => assert!(error.to_string().contains("fs.read root"), "got {error:#}"),
            Ok(_) => panic!("expected an error for a missing fs.read root"),
        }
    }
}
