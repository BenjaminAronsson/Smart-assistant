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
use jarvis_adapters::mcp_host::{HostPolicyTable, McpHost};
use jarvis_adapters::smtp::{SmtpConfig, SmtpTool};
use jarvis_adapters::tools::example_light::ExampleLightTool;
use jarvis_adapters::tools::example_message::ExampleMessageTool;
use jarvis_adapters::tools::fs_read::FsReadTool;
use jarvis_adapters::tools::media_playback::{
    MediaOpenUrlTool, MediaPlaybackTool, MediaVolumeBoostTool,
};
use jarvis_adapters::tools::timeout::TimeoutExecutor;
use jarvis_adapters::web::{BraveSearchProvider, HttpPageFetcher, WebFetchTool, WebSearchTool};
use jarvis_application::policy::{ToolDescriptor, ToolRegistry};
use tokio_util::sync::CancellationToken;

/// Build the M2 tool registry. `fs_root`, when present, is the allowlisted root
/// for the R0 `fs.read` tool; when `None`, `fs.read` is **not** registered — the
/// stricter default (no ambient filesystem-read authority until the host
/// explicitly configures a root). The reversible R1 `example.light` and the
/// external R2 `message.send` demonstrations need no host configuration and are
/// always registered.
///
/// Every executor is timeout-wrapped ([`wrap_with_timeout`]) at registration.
pub fn build_registry(fs_root: Option<PathBuf>) -> anyhow::Result<ToolRegistry> {
    build_registry_with_smtp(fs_root, None)
}

/// Build the registry with the configured SMTP executor. SMTP is opt-in: when
/// absent, the M2 no-op message tool remains available for policy/grant tests;
/// when present, the same `message.send` id is backed by real SMTP transport.
pub fn build_registry_with_smtp(
    fs_root: Option<PathBuf>,
    smtp: Option<SmtpConfig>,
) -> anyhow::Result<ToolRegistry> {
    let mut registry = ToolRegistry::new();

    if let Some(root) = fs_root {
        let descriptor = FsReadTool::descriptor(&root)
            .with_context(|| format!("fs.read root {} is unreadable", root.display()))?;
        registry.register(wrap_with_timeout(descriptor))?;
    }
    registry.register(wrap_with_timeout(ExampleLightTool::descriptor()))?;
    let message = smtp
        .map(SmtpTool::descriptor)
        .unwrap_or_else(ExampleMessageTool::descriptor);
    registry.register(wrap_with_timeout(message))?;

    Ok(registry)
}

/// A pinned out-of-process MCP tool server to launch (F2.7, docs/06 §5): the
/// **host-authored** command (never derived from model or tool text — docs/06 §5
/// "pinned version/hash") and the host-owned [`HostPolicyTable`] overlaid on the
/// tools it exports. The server's self-declared safety is discarded; only tools
/// the table sanctions are registered.
pub struct McpServerSpec {
    pub command: tokio::process::Command,
    pub policy_table: HostPolicyTable,
}

/// Connect to each configured MCP tool server, import + host-policy-overlay its
/// tools, and register them (timeout-wrapped at the same single site as native
/// tools) into `registry`. Returns the live [`McpHost`] handles: the caller
/// **must** keep them alive for the process lifetime, because each registered
/// executor holds a peer into the running child — dropping a host tears its
/// child (and its tools' executors) down.
///
/// `specs` is empty by default: no configured server means no MCP tool authority,
/// the stricter default (mirroring `fs.read`'s unconfigured-root behaviour). The
/// connect/import of a wedged or hostile server is bounded and cancellable via
/// `cancel` (invariant #4); a server that fails to connect or import aborts
/// startup rather than silently yielding a partial tool set (fail closed).
pub async fn register_mcp_servers(
    registry: &mut ToolRegistry,
    specs: Vec<McpServerSpec>,
    cancel: CancellationToken,
) -> anyhow::Result<Vec<McpHost>> {
    let mut hosts = Vec::with_capacity(specs.len());
    for spec in specs {
        let host = McpHost::connect(spec.command, cancel.clone())
            .await
            .context("connecting to a configured MCP tool server")?;
        let descriptors = host
            .import_tools(&spec.policy_table, cancel.clone())
            .await
            .context("importing MCP tool descriptors")?;
        for descriptor in descriptors {
            // Same timeout wrap + `MissingPolicy` refusal as native tools; an
            // imported descriptor always carries host policy, so it registers.
            registry
                .register(wrap_with_timeout(descriptor))
                .map_err(|e| anyhow::anyhow!("registering an MCP tool: {e}"))?;
        }
        hosts.push(host);
    }
    Ok(hosts)
}

/// Register the R0 `web.search`/`web.fetch` tools against the live Brave
/// provider + HTTP fetcher (F2.8 Slice 3, docs/02 §11b, ADR-014). jarvisd calls
/// this **only** when `[integrations.web_search]` is configured — the config
/// presence IS the external-egress consent gate (CF-5, docs/06 §5): no configured
/// provider ⇒ no web tools ⇒ no ambient external egress, the stricter default
/// (mirrors `fs.read`'s unconfigured root and the empty MCP server list).
/// `api_key` is a resolved secret (sent only as a provider header, never logged).
/// Both executors are timeout-wrapped at this single registration site.
pub fn register_web_tools(
    registry: &mut ToolRegistry,
    api_key: String,
    max_fetch_bytes: usize,
) -> anyhow::Result<()> {
    let search = WebSearchTool::descriptor(BraveSearchProvider::new(api_key));
    let fetch = WebFetchTool::descriptor(HttpPageFetcher::new(max_fetch_bytes));
    registry
        .register(wrap_with_timeout(search))
        .map_err(|e| anyhow::anyhow!("registering web.search: {e}"))?;
    registry
        .register(wrap_with_timeout(fetch))
        .map_err(|e| anyhow::anyhow!("registering web.fetch: {e}"))?;
    Ok(())
}

/// Register the media tools against a live [`MediaController`] (F3a.7, FR-22,
/// docs/02 §11a). jarvisd calls this **only** when `[integrations.media]` is
/// enabled and
/// a session bus was reachable — same opt-in stance as the web tools: no
/// configured media ⇒ no media tools ⇒ no ambient playback authority.
///
/// Both tiers register together and share the same cap: `media.playback` (R1,
/// transport + volume within `max_volume`) and `media.volume_boost` (R2, above
/// it). Registering them as a pair is deliberate — the R1 tool's denial names
/// the R2 tool as the authorized path, so shipping one without the other would
/// leave a dead end.
pub fn register_media_tools(
    registry: &mut ToolRegistry,
    controller: Arc<dyn jarvis_application::ports::MediaController>,
    max_volume: jarvis_domain::media::VolumePct,
    cast: Option<CastWiring>,
) -> anyhow::Result<()> {
    registry
        .register(wrap_with_timeout(MediaPlaybackTool::descriptor(
            controller.clone(),
            max_volume,
        )))
        .map_err(|e| anyhow::anyhow!("registering media.playback: {e}"))?;
    registry
        .register(wrap_with_timeout(MediaVolumeBoostTool::descriptor(
            controller, max_volume,
        )))
        .map_err(|e| anyhow::anyhow!("registering media.volume_boost: {e}"))?;
    // Cast-a-link is registered separately because it needs a *display* (a
    // monitor to cast onto) rather than a session bus. A host with media
    // control but no configured media-window monitor gets transport control and
    // no cast tool — better than a tool that always fails closed.
    if let Some(cast) = cast {
        registry
            .register(wrap_with_timeout(MediaOpenUrlTool::descriptor(
                cast.profile,
                cast.sink,
                cast.audit,
            )))
            .map_err(|e| anyhow::anyhow!("registering media.open_url: {e}"))?;
    }
    Ok(())
}

/// What `media.open_url` needs: the display profile that resolves the media
/// window's monitor, and the sink that carries the directive to the agent.
pub struct CastWiring {
    pub profile: Arc<jarvis_domain::display::DisplayProfile>,
    pub sink: Arc<dyn jarvis_application::ports::MediaWindowSink>,
    /// Durable audit for the cast itself: the URL is recorded verbatim before
    /// the window opens (docs/02 §11a, invariant 6).
    pub audit: Arc<dyn jarvis_application::ports::AuditLog>,
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

    #[test]
    fn web_tools_register_only_when_configured() {
        // The default registry (no web config) has neither web tool — the CF-5
        // external-egress gate: web tools exist only once a provider is wired.
        let mut registry = build_registry(None).expect("builds");
        assert!(
            registry
                .policy_of(&WebSearchTool::<BraveSearchProvider>::id())
                .is_none()
        );
        assert!(
            registry
                .policy_of(&WebFetchTool::<HttpPageFetcher>::id())
                .is_none()
        );

        register_web_tools(&mut registry, "fake-key".to_owned(), 1024).expect("registers");
        let search = registry
            .policy_of(&WebSearchTool::<BraveSearchProvider>::id())
            .expect("web.search registered");
        assert_eq!(search.egress, jarvis_domain::policy::DataEgress::External);
        assert!(
            registry
                .resolve(&WebFetchTool::<HttpPageFetcher>::id())
                .is_some()
        );
    }

    #[test]
    fn media_tools_register_only_when_configured_and_keep_their_tiers() {
        // Same opt-in gate as the web tools: an unconfigured host has no media
        // authority at all.
        let mut registry = build_registry(None).expect("builds");
        assert!(registry.policy_of(&MediaPlaybackTool::id()).is_none());
        assert!(registry.policy_of(&MediaVolumeBoostTool::id()).is_none());

        struct DeadController;
        #[async_trait::async_trait]
        impl jarvis_application::ports::MediaController for DeadController {
            async fn snapshot(
                &self,
                _cancel: CancellationToken,
            ) -> Result<jarvis_domain::media::MediaSnapshot, jarvis_application::ports::MediaError>
            {
                Ok(jarvis_domain::media::MediaSnapshot::none())
            }
            async fn transport(
                &self,
                _player: &jarvis_domain::media::PlayerId,
                _command: jarvis_domain::media::TransportCommand,
                _cancel: CancellationToken,
            ) -> Result<(), jarvis_application::ports::MediaError> {
                Ok(())
            }
            async fn set_volume(
                &self,
                _player: &jarvis_domain::media::PlayerId,
                _volume: jarvis_domain::media::VolumePct,
                _cancel: CancellationToken,
            ) -> Result<(), jarvis_application::ports::MediaError> {
                Ok(())
            }
        }

        register_media_tools(
            &mut registry,
            Arc::new(DeadController),
            jarvis_domain::media::VolumePct::new(70).unwrap(),
            None,
        )
        .expect("registers");

        // Without cast wiring there is no cast tool — a host with no configured
        // media-window monitor gets transport control only.
        assert!(registry.policy_of(&MediaOpenUrlTool::id()).is_none());

        // The tiers survive registration: transport auto-authorizes, above-cap
        // volume parks for approval.
        let transport = registry
            .policy_of(&MediaPlaybackTool::id())
            .expect("media.playback registered");
        assert_eq!(transport.risk, jarvis_domain::policy::RiskLevel::R1);
        assert!(!transport.requires_grant());
        let boost = registry
            .policy_of(&MediaVolumeBoostTool::id())
            .expect("media.volume_boost registered");
        assert_eq!(boost.risk, jarvis_domain::policy::RiskLevel::R2);
        assert!(boost.requires_grant());
        assert!(registry.resolve(&MediaPlaybackTool::id()).is_some());
    }

    #[tokio::test]
    async fn no_configured_mcp_servers_registers_nothing_and_spawns_no_child() {
        // The stricter default: with no configured MCP server, the registry gains
        // no MCP tools and no child process is launched. (A real-child import is
        // covered end-to-end by the mcp-echo-fixture integration tests.)
        let mut registry = build_registry(None).expect("builds");
        let before = registry.resolve(&ExampleLightTool::id()).is_some();
        let hosts = register_mcp_servers(&mut registry, Vec::new(), CancellationToken::new())
            .await
            .expect("empty MCP config is a no-op");
        assert!(hosts.is_empty(), "no servers connected");
        assert!(before, "native tools remain registered unchanged");
    }
}
