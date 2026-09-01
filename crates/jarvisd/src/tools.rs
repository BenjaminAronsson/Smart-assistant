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
use jarvis_adapters::home_assistant::{
    EntityAllowlist, HomeAssistantClient, HomeBroadTool, HomeGetStateTool, HomeSetAreaLightsTool,
    HomeSetLightTool,
};
use jarvis_adapters::mcp_host::{HostPolicyTable, McpHost};
use jarvis_adapters::smtp::{SmtpConfig, SmtpTool};
use jarvis_adapters::spotify::{SpotifyClient, descriptors as spotify_descriptors};
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

/// Register `app.generate` against a live app-builder host (F6.6, FR-18).
///
/// jarvisd calls this **only** when `[apps]` is enabled — the same opt-in stance
/// as every other capability that spawns a process. No configured builder ⇒ no
/// `app.generate` ⇒ a model that proposes it gets `policy.unknown_tool`, which
/// is the correct answer on a host that cannot build.
pub fn register_app_tools(
    registry: &mut ToolRegistry,
    builder: Arc<jarvis_adapters::app_builder::AppBuilderHost>,
) -> anyhow::Result<()> {
    registry
        .register(wrap_with_timeout(
            crate::apptool::AppGenerateTool::descriptor(builder),
        ))
        .map_err(|e| anyhow::anyhow!("registering app.generate: {e}"))?;
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

/// Register the curated Home Assistant tools (F5.3, FR-14, ADR-006, docs/02
/// §10). jarvisd calls this **only** when `[integrations.home_assistant]` is
/// enabled — no configured HA ⇒ no home tools ⇒ no ambient authority over
/// physical devices.
///
/// All four register together and share one `allowlist`. The set is *curated*,
/// never a passthrough to HA's service namespace: `home.get_state` (R0),
/// `home.set_light` (R1, reversible), and `home.execute_scene` /
/// `home.run_script` (R2, approval). Because `policy::evaluate` does not
/// inspect arguments, the allowlist is enforced inside each executor rather
/// than by the tier — the registry only guarantees the tier and the timeout.
pub fn register_home_assistant_tools(
    registry: &mut ToolRegistry,
    client: Arc<HomeAssistantClient>,
    allowlist: Arc<EntityAllowlist>,
) -> anyhow::Result<()> {
    for descriptor in [
        HomeGetStateTool::descriptor(client.clone(), allowlist.clone()),
        HomeSetLightTool::descriptor(client.clone(), allowlist.clone()),
        // Plural/area form (F5.4, FR-28). Same R1 tier as the singular tool —
        // it reaches no entity the singular one could not — but it bounds its
        // own fan-out in-executor, since the tier cannot see arguments.
        HomeSetAreaLightsTool::descriptor(client.clone(), allowlist.clone()),
        HomeBroadTool::scene_descriptor(client.clone(), allowlist.clone()),
        HomeBroadTool::script_descriptor(client, allowlist),
    ] {
        let id = descriptor.id.clone();
        registry
            .register(wrap_with_timeout(descriptor))
            .map_err(|e| anyhow::anyhow!("registering {id}: {e}"))?;
    }
    Ok(())
}

/// Register the Spotify tools (F5.6, FR-21, ADR-012/022, docs/02 §11a).
/// Opt-in on `[integrations.spotify]`, same stance as the other external
/// integrations.
///
/// The volume cap lives in the *tools*, not the tier: `spotify.volume` (R1)
/// refuses anything above the configured cap and names `spotify.volume_boost`
/// (R2, approval) as the authorized path — the same pairing `media.playback` /
/// `media.volume_boost` uses, and for the same reason (argument-blind policy).
/// The set contains no library-mutating tool by construction.
pub fn register_spotify_tools(
    registry: &mut ToolRegistry,
    client: Arc<SpotifyClient>,
) -> anyhow::Result<()> {
    for descriptor in spotify_descriptors(client) {
        let id = descriptor.id.clone();
        registry
            .register(wrap_with_timeout(descriptor))
            .map_err(|e| anyhow::anyhow!("registering {id}: {e}"))?;
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

    /// F6.1: every [`Capability`] in the host's closed vocabulary must name a
    /// tool that is **actually registrable here**, and its declared risk tier
    /// must equal that tool's host-owned `ToolPolicy.risk`.
    ///
    /// The domain's `Capability::risk()` is only a preview — the authoritative
    /// decision is `policy::evaluate` against the live registry — but a preview
    /// that disagrees with the real tier is worse than none: the approval card
    /// for a generated app would understate what the app can do. This is the
    /// fixture-vs-caller check (the M5 lesson) applied at the vocabulary
    /// boundary: it compares the domain's claim against the descriptors the
    /// **real** registration site builds, not against a hand-written table.
    #[test]
    fn every_capability_maps_to_a_registered_tool_at_the_risk_it_declares() {
        use jarvis_adapters::home_assistant::{HomeBroadTool, HomeGetStateTool, HomeSetLightTool};
        use jarvis_domain::artifact::Capability;

        for capability in Capability::ALL {
            // Exhaustive: a new capability with no descriptor here fails to
            // compile rather than shipping unbacked.
            let (id, policy) = match capability {
                Capability::HomeReadState => (HomeGetStateTool::id(), HomeGetStateTool::policy()),
                Capability::HomeSetLight => (HomeSetLightTool::id(), HomeSetLightTool::policy()),
                Capability::HomeExecuteScene => {
                    (HomeBroadTool::scene_id(), HomeBroadTool::policy())
                }
            };
            assert_eq!(
                capability.tool_id(),
                id,
                "{capability} must name the tool that actually backs it"
            );
            assert_eq!(
                capability.risk(),
                policy.risk,
                "{capability}'s declared tier must match the registered tool's host policy"
            );
        }
    }

    /// The stronger half of the same check: every capability's tool must be in
    /// a registry built by the **real** registration function, at the tier the
    /// capability declares.
    ///
    /// The test above compares against `Tool::policy()`; this one compares
    /// against what `register_home_assistant_tools` actually put in the
    /// registry, so dropping a descriptor from that array — which the previous
    /// test would not have noticed — fails here (F6.1 review). This is the
    /// fixture-vs-caller rule taken to its end: the input is built the way the
    /// real producer builds it.
    #[test]
    fn every_capability_resolves_in_a_registry_built_by_the_real_registration_path() {
        use jarvis_adapters::home_assistant::{
            EntityAllowlist, HomeAssistantClient, HomeAssistantTransport,
        };
        use jarvis_domain::artifact::Capability;

        // A transport that is never driven — registration performs no I/O, and
        // this test asserts about the registry, not about talking to HA.
        struct UnusedTransport;
        #[async_trait::async_trait]
        impl HomeAssistantTransport for UnusedTransport {
            async fn send(
                &self,
                _request: jarvis_adapters::home_assistant::HomeRequest,
                _cancel: tokio_util::sync::CancellationToken,
            ) -> Result<String, jarvis_adapters::home_assistant::HomeAssistantError> {
                unreachable!("registration performs no I/O")
            }
        }

        let mut registry = build_registry(None).expect("builds");
        register_home_assistant_tools(
            &mut registry,
            Arc::new(HomeAssistantClient::with_transport(Arc::new(
                UnusedTransport,
            ))),
            Arc::new(EntityAllowlist::default()),
        )
        .expect("the real registration path registers the home tools");

        for capability in Capability::ALL {
            let policy = registry
                .policy_of(&capability.tool_id())
                .unwrap_or_else(|| {
                    panic!(
                        "{capability} names {}, which the real registration path does not register",
                        capability.tool_id()
                    )
                });
            assert_eq!(
                capability.risk(),
                policy.risk,
                "{capability}'s declared tier must match its registered tier"
            );
        }
    }

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

#[cfg(test)]
mod scope_coverage_tests {
    use super::*;
    use jarvis_domain::identity::DeviceClass;
    use std::collections::BTreeSet;

    /// **M6 gate finding B1.** Every tool a real registry registers must have
    /// its `required_scopes` covered by what pairing actually grants.
    ///
    /// `policy::evaluate` rejects on the missing-scope arm *before* any risk
    /// logic, so an uncovered scope is not a subtle degradation — the tool is
    /// simply unreachable for every real caller. That went unnoticed for three
    /// milestones because every golden and acceptance suite builds
    /// `PolicyContext` by hand with the scopes it needs; the real paired device
    /// never appeared in a test until golden 8.
    ///
    /// This is the fixture-vs-caller rule taken to its end: the input is built
    /// the way the **real** producer builds it, and the assertion is that it is
    /// ACCEPTED. Adding a tool with a new scope now fails here until the scope
    /// is granted deliberately.
    #[test]
    fn every_registered_tools_scope_is_one_a_paired_device_actually_holds() {
        let owner = DeviceClass::OwnerUi.scopes();
        let granted: BTreeSet<&str> = owner.iter().map(String::as_str).collect();
        let mut registry = build_registry(Some(std::env::temp_dir())).expect("builds");
        // The web tools are conditionally registered too, and their descriptors
        // are what the real registration path installs — so drive that path
        // rather than restating their policies.
        register_web_tools(&mut registry, "unused-key".to_owned(), 1024).expect("registers");

        let mut missing: Vec<String> = Vec::new();
        let mut check = |what: String, policy: &jarvis_domain::policy::ToolPolicy| {
            for scope in &policy.required_scopes {
                if !granted.contains(scope.as_str()) {
                    missing.push(format!("{what} needs `{scope}`"));
                }
            }
        };

        for id in registry.tool_ids() {
            let policy = registry
                .policy_of(id)
                .expect("a registered tool has policy");
            check(id.to_string(), policy);
        }

        // The conditionally-registered tools are not in `build_registry` — they
        // depend on configured integrations — but their scopes must be granted
        // all the same, or enabling the integration produces a tool nobody can
        // call. Reached through each tool's own public `policy()`, which is the
        // very value its registration site installs.
        use jarvis_adapters::home_assistant as ha;
        use jarvis_adapters::spotify as sp;
        use jarvis_adapters::tools::media_playback as mp;
        // web.search/web.fetch are already in `build_registry`'s successors —
        // they are registered by `register_web_tools`, whose descriptors carry
        // the same policies; covered via the registry below when configured, and
        // by their scopes being granted explicitly.
        for (what, policy) in [
            ("home.get_state", ha::HomeGetStateTool::policy()),
            ("home.set_light", ha::HomeSetLightTool::policy()),
            ("home.set_area_lights", ha::HomeSetAreaLightsTool::policy()),
            ("home.execute_broad", ha::HomeBroadTool::policy()),
            ("media.playback", mp::MediaPlaybackTool::policy()),
            ("spotify.search", sp::SpotifySearchTool::policy()),
            ("message.send", jarvis_adapters::smtp::SmtpTool::policy()),
            ("app.generate", crate::apptool::AppGenerateTool::policy()),
        ] {
            check(what.to_owned(), &policy);
        }
        assert!(
            missing.is_empty(),
            "a paired device cannot execute these tools — grant the scope in \
             jarvis_domain::identity::OWNER_TOOL_SCOPES, deliberately: {missing:?}"
        );
    }

    /// **F7.1, the inverse of B1.** A room satellite must not be able to
    /// execute anything. B1 was "the owner's device holds too little"; the
    /// failure this milestone introduces the opportunity for is "a node holds
    /// too much" — and it would be just as invisible, because no fixture
    /// builds a node's context by accident.
    ///
    /// Driven from the real registry, so a tool registered later is covered
    /// without anyone remembering to come back here.
    #[test]
    fn no_node_class_can_execute_any_registered_tool() {
        let mut registry = build_registry(Some(std::env::temp_dir())).expect("builds");
        register_web_tools(&mut registry, "unused-key".to_owned(), 1024).expect("registers");

        let mut reachable: Vec<String> = Vec::new();
        for class in [
            DeviceClass::DisplayNode,
            DeviceClass::VoiceNode,
            DeviceClass::RoomNode,
        ] {
            let held: BTreeSet<String> = class.scopes().into_iter().collect();
            for id in registry.tool_ids() {
                let policy = registry
                    .policy_of(id)
                    .expect("a registered tool has policy");
                // A tool is reachable for this class when the class holds
                // every scope the tool requires — exactly `policy::evaluate`'s
                // missing-scope arm, restated from the caller's side.
                if policy
                    .required_scopes
                    .iter()
                    .all(|scope| held.contains(scope.as_str()))
                {
                    reachable.push(format!("{class} can execute {id}"));
                }
            }
        }
        assert!(
            reachable.is_empty(),
            "a room satellite must present and capture, never act: {reachable:?}"
        );
    }

    /// The same claim, made at the place that actually enforces it. The test
    /// above reasons about scope sets; this one calls `policy::evaluate` with a
    /// `PolicyContext` built exactly the way the gateway builds one from a
    /// paired device (`Scope::new(..).ok()` over its effective scopes — see
    /// `jarvisd::appbridge::context_of`). Restating the enforcer's rule in a
    /// test is how a test and its enforcer drift apart while both look green.
    ///
    /// The HTTP half of the same property — that a node cannot reach the
    /// owner's routes at all — lives in `tests/sessions_api.rs` and
    /// `tests/devices_api.rs`, against the real router.
    #[test]
    fn a_node_is_denied_at_the_policy_engine_not_merely_by_arithmetic() {
        use jarvis_application::policy::{DenyReason, PolicyContext, PolicyDecision, evaluate};
        use jarvis_domain::policy::Scope;
        use jarvis_domain::tools::{CanonicalValue, ToolProposal};

        let mut registry = build_registry(Some(std::env::temp_dir())).expect("builds");
        register_web_tools(&mut registry, "unused-key".to_owned(), 1024).expect("registers");

        let node = DeviceClass::RoomNode;
        let ctx = PolicyContext {
            user_id: crate::auth::fresh_id(),
            device_id: crate::auth::fresh_id(),
            granted_scopes: node
                .scopes()
                .iter()
                .filter_map(|s| Scope::new(s.as_str()).ok())
                .collect(),
        };
        assert!(
            ctx.granted_scopes.is_empty(),
            "a node's class scopes are not tool scopes, so its policy context is empty"
        );

        for id in registry.tool_ids() {
            let proposal = ToolProposal {
                tool_id: id.clone(),
                arguments: CanonicalValue::obj([]),
            };
            match evaluate(&proposal, &registry, &ctx) {
                PolicyDecision::Reject {
                    reason: DenyReason::MissingScope(_) | DenyReason::Prohibited,
                } => {}
                other => panic!("a room node got {other:?} for {id} — it must be denied"),
            }
        }
    }
}

/// F9.6: `declare_tool_id!` generates only `id()` — it must not be able to
/// drift a tool's `policy()`. This snapshot pins the `(id, risk, egress,
/// required_scopes)` of every tool that adopted the macro (plus the two
/// `HomeBroadTool` ids, which did not, for full registry coverage) to the
/// values in place immediately before F9.6, captured by reading the still-
/// unmodified `policy()` bodies. A future change that lets the macro imply a
/// tier — the exact regression the milestone doc warns against — fails here
/// before it fails an auditor.
#[cfg(test)]
mod f9_6_policy_snapshot_tests {
    use jarvis_adapters::home_assistant::{
        HomeBroadTool, HomeGetStateTool, HomeSetAreaLightsTool, HomeSetLightTool,
    };
    use jarvis_adapters::smtp::SmtpTool;
    use jarvis_adapters::spotify::{
        SpotifyPlayPlaylistTool, SpotifyPlayTool, SpotifyQueueAddTool, SpotifySearchTool,
        SpotifyVolumeBoostTool, SpotifyVolumeTool,
    };
    use jarvis_adapters::tools::example_light::ExampleLightTool;
    use jarvis_adapters::tools::example_message::ExampleMessageTool;
    use jarvis_adapters::tools::fs_read::FsReadTool;
    use jarvis_adapters::tools::media_playback::{
        MediaOpenUrlTool, MediaPlaybackTool, MediaVolumeBoostTool,
    };
    use jarvis_adapters::web::{BraveSearchProvider, HttpPageFetcher, WebFetchTool, WebSearchTool};
    use jarvis_domain::policy::{DataEgress, RiskLevel, SpeechSensitivity, ToolPolicy};
    use jarvis_domain::tools::ToolId;

    fn scopes_of(policy: &ToolPolicy) -> Vec<&str> {
        policy.required_scopes.iter().map(|s| s.as_str()).collect()
    }

    struct Case {
        id: ToolId,
        policy: ToolPolicy,
        expected_id: &'static str,
        expected_risk: RiskLevel,
        expected_egress: DataEgress,
        expected_scopes: &'static [&'static str],
        // is_reversible and requires_user_presence, not just risk/egress/scope:
        // both feed the same auto-authorize/approval decision invariant 1
        // protects (docs/06 §3), so a macro-migration typo flipping either is
        // exactly the "policy regression, not a cleanup" F9.6 warns against.
        expected_is_reversible: bool,
        expected_requires_user_presence: bool,
        // S3: pinned here for the same reason as `risk` — the field is
        // declared per tool and must never start being *derived*. Nothing in
        // the tiers below predicts this column: `fs.read` is the least
        // dangerous tool in the table and one of only two `Sensitive` ones.
        expected_speech_sensitivity: SpeechSensitivity,
    }

    #[test]
    fn every_declare_tool_id_site_keeps_its_pre_f9_6_id_and_policy() {
        let cases: Vec<Case> = vec![
            Case {
                id: HomeGetStateTool::id(),
                policy: HomeGetStateTool::policy(),
                expected_id: "home.get_state",
                expected_is_reversible: true,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R0,
                expected_egress: DataEgress::Local,
                expected_scopes: &["home:read"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: HomeSetLightTool::id(),
                policy: HomeSetLightTool::policy(),
                expected_id: "home.set_light",
                expected_is_reversible: true,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R1,
                expected_egress: DataEgress::Local,
                expected_scopes: &["home:control"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: HomeSetAreaLightsTool::id(),
                policy: HomeSetAreaLightsTool::policy(),
                expected_id: "home.set_area_lights",
                expected_is_reversible: true,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R1,
                expected_egress: DataEgress::Local,
                expected_scopes: &["home:control"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: HomeBroadTool::scene_id(),
                policy: HomeBroadTool::policy(),
                expected_id: "home.execute_scene",
                expected_is_reversible: false,
                expected_requires_user_presence: true,
                expected_risk: RiskLevel::R2,
                expected_egress: DataEgress::Local,
                expected_scopes: &["home:control"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: HomeBroadTool::script_id(),
                policy: HomeBroadTool::policy(),
                expected_id: "home.run_script",
                expected_is_reversible: false,
                expected_requires_user_presence: true,
                expected_risk: RiskLevel::R2,
                expected_egress: DataEgress::Local,
                expected_scopes: &["home:control"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: SmtpTool::id(),
                policy: SmtpTool::policy(),
                expected_id: "message.send",
                expected_is_reversible: false,
                expected_requires_user_presence: true,
                expected_risk: RiskLevel::R2,
                expected_egress: DataEgress::External,
                expected_scopes: &["message:send"],
                expected_speech_sensitivity: SpeechSensitivity::Sensitive,
            },
            Case {
                id: SpotifyPlayTool::id(),
                policy: SpotifyPlayTool::policy(),
                expected_id: "spotify.play",
                expected_is_reversible: true,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R1,
                expected_egress: DataEgress::External,
                expected_scopes: &["media:control"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: SpotifyPlayPlaylistTool::id(),
                policy: SpotifyPlayPlaylistTool::policy(),
                expected_id: "spotify.play_playlist",
                expected_is_reversible: true,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R1,
                expected_egress: DataEgress::External,
                expected_scopes: &["media:control"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: SpotifyQueueAddTool::id(),
                policy: SpotifyQueueAddTool::policy(),
                expected_id: "spotify.queue_add",
                expected_is_reversible: true,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R1,
                expected_egress: DataEgress::External,
                expected_scopes: &["media:control"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: SpotifySearchTool::id(),
                policy: SpotifySearchTool::policy(),
                expected_id: "spotify.search",
                expected_is_reversible: true,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R0,
                expected_egress: DataEgress::External,
                expected_scopes: &["media:search"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: SpotifyVolumeTool::id(),
                policy: SpotifyVolumeTool::policy(),
                expected_id: "spotify.volume",
                expected_is_reversible: true,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R1,
                expected_egress: DataEgress::External,
                expected_scopes: &["media:control"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: SpotifyVolumeBoostTool::id(),
                policy: SpotifyVolumeBoostTool::policy(),
                expected_id: "spotify.volume_boost",
                expected_is_reversible: false,
                expected_requires_user_presence: true,
                expected_risk: RiskLevel::R2,
                expected_egress: DataEgress::External,
                expected_scopes: &["media:control"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: ExampleLightTool::id(),
                policy: ExampleLightTool::policy(),
                expected_id: "example.light",
                expected_is_reversible: true,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R1,
                expected_egress: DataEgress::Local,
                expected_scopes: &["demo:light"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: ExampleMessageTool::id(),
                policy: ExampleMessageTool::policy(),
                expected_id: "message.send",
                expected_is_reversible: false,
                expected_requires_user_presence: true,
                expected_risk: RiskLevel::R2,
                expected_egress: DataEgress::External,
                expected_scopes: &["message:send"],
                expected_speech_sensitivity: SpeechSensitivity::Sensitive,
            },
            Case {
                id: FsReadTool::id(),
                policy: FsReadTool::policy(),
                expected_id: "fs.read",
                expected_is_reversible: true,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R0,
                expected_egress: DataEgress::None,
                expected_scopes: &["files:read"],
                expected_speech_sensitivity: SpeechSensitivity::Sensitive,
            },
            Case {
                id: MediaPlaybackTool::id(),
                policy: MediaPlaybackTool::policy(),
                expected_id: "media.playback",
                expected_is_reversible: true,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R1,
                expected_egress: DataEgress::Local,
                expected_scopes: &["media:control"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: MediaVolumeBoostTool::id(),
                policy: MediaVolumeBoostTool::policy(),
                expected_id: "media.volume_boost",
                expected_is_reversible: false,
                expected_requires_user_presence: true,
                expected_risk: RiskLevel::R2,
                expected_egress: DataEgress::Local,
                expected_scopes: &["media:control"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: MediaOpenUrlTool::id(),
                policy: MediaOpenUrlTool::policy(),
                expected_id: "media.open_url",
                expected_is_reversible: true,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R1,
                expected_egress: DataEgress::External,
                expected_scopes: &["media:control"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: WebSearchTool::<BraveSearchProvider>::id(),
                policy: WebSearchTool::<BraveSearchProvider>::policy(),
                expected_id: "web.search",
                expected_is_reversible: false,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R0,
                expected_egress: DataEgress::External,
                expected_scopes: &["web:search"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: WebFetchTool::<HttpPageFetcher>::id(),
                policy: WebFetchTool::<HttpPageFetcher>::policy(),
                expected_id: "web.fetch",
                expected_is_reversible: false,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R0,
                expected_egress: DataEgress::External,
                expected_scopes: &["web:fetch"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
            Case {
                id: crate::apptool::AppGenerateTool::id(),
                policy: crate::apptool::AppGenerateTool::policy(),
                expected_id: "app.generate",
                expected_is_reversible: false,
                expected_requires_user_presence: false,
                expected_risk: RiskLevel::R1,
                expected_egress: DataEgress::Local,
                expected_scopes: &["app:build"],
                expected_speech_sensitivity: SpeechSensitivity::Normal,
            },
        ];

        // S3: the table's guarantee is "no registered tool's policy can drift
        // unnoticed", and that only holds while the table covers the registry.
        // Without this, registering a new tool adds an unpinned
        // `speech_sensitivity` — and the unpinned direction is `Normal`, i.e.
        // spoken by a third-party voice. Compared by id set, not count, so the
        // failure names the tool that was added.
        {
            use std::collections::BTreeSet;
            let pinned: BTreeSet<&str> = cases.iter().map(|c| c.expected_id).collect();
            let mut registry = super::build_registry(Some(std::env::temp_dir())).expect("builds");
            super::register_web_tools(&mut registry, "unused-key".to_owned(), 1024)
                .expect("registers");
            let registered: BTreeSet<String> =
                registry.tool_ids().map(ToString::to_string).collect();
            let missing: Vec<&String> = registered
                .iter()
                .filter(|id| !pinned.contains(id.as_str()))
                .collect();
            assert!(
                missing.is_empty(),
                "these tools are registered but not pinned by this snapshot, so their \
                 risk/egress/scopes/speech_sensitivity can drift unnoticed: {missing:?}"
            );
        }

        for case in cases {
            assert_eq!(
                case.id.as_str(),
                case.expected_id,
                "tool id string must be unchanged"
            );
            assert_eq!(
                case.policy.risk, case.expected_risk,
                "{}: risk tier drifted",
                case.expected_id
            );
            assert_eq!(
                case.policy.egress, case.expected_egress,
                "{}: egress drifted",
                case.expected_id
            );
            assert_eq!(
                scopes_of(&case.policy),
                case.expected_scopes,
                "{}: required_scopes drifted",
                case.expected_id
            );
            assert_eq!(
                case.policy.is_reversible, case.expected_is_reversible,
                "{}: is_reversible drifted",
                case.expected_id
            );
            assert_eq!(
                case.policy.requires_user_presence, case.expected_requires_user_presence,
                "{}: requires_user_presence drifted",
                case.expected_id
            );
            assert_eq!(
                case.policy.speech_sensitivity, case.expected_speech_sensitivity,
                "{}: speech_sensitivity drifted — a tool that reads private content \
                 aloud in a third-party voice is a privacy regression, not a cleanup",
                case.expected_id
            );
        }
    }
}
