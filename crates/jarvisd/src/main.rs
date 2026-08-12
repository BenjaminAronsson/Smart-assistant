#![deny(unsafe_code)]
//! jarvisd entry point: config → telemetry → serve → graceful shutdown
//! (docs/02 §12). Cold start to healthy must stay < 2 s (NFR-15).

use std::sync::Arc;

use anyhow::Context as _;
use axum::serve::ListenerExt;
use jarvis_adapters::claude_cli::ClaudeCliModel;
use jarvis_adapters::embeddings::FastEmbedProvider;
use jarvis_application::deterministic::DeterministicFirstProvider;
use jarvis_application::memory::MemoryRetrievalService;
use jarvis_application::orchestrator::RunInput;
use jarvis_application::ports::{MessageStore, RunStore};
use jarvis_application::voice::{SpeechSynthesizer, SpeechTranscriber};
use jarvis_domain::conversations::MessageRole;
use jarvis_domain::ids::SessionId;
use jarvis_domain::run::Run;
use jarvis_infra::dispatcher::OutboxDispatcher;
use jarvisd::api::RunWiring;
use jarvisd::runs::{MemoryAssembler, RunApi, RunEngine, SystemClock};
use jarvisd::ws::{WsHub, WsState};
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;

fn main() -> anyhow::Result<()> {
    let config = jarvisd::config::Config::load()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(config))
}

async fn run(config: jarvisd::config::Config) -> anyhow::Result<()> {
    let telemetry = jarvisd::observability::init(config.observability.otlp_endpoint.as_deref())?;

    // Unresolvable secret reference = config error = fail fast (docs/09 §1).
    // An unREACHABLE database is different: the lazy pool lets jarvisd start
    // degraded and the health probe reports it (docs/02 §12).
    let db_url = jarvisd::config::resolve_secret_ref(&config.database.url_secret)?;
    let pool = jarvis_infra::db::connect_lazy(db_url.expose(), config.database.max_connections)?;

    let identity = Arc::new(jarvis_infra::identity::PgIdentityStore::new(pool.clone()));
    let auth = jarvisd::auth::AuthState::bootstrap(identity)
        .await
        .with_audit(Arc::new(jarvis_infra::audit_sink::PgAuditLog::new(
            pool.clone(),
        )));

    // Persistence adapters behind the application ports.
    let session_store = Arc::new(jarvis_infra::sessions::PgSessionStore::new(pool.clone()));
    let message_store = Arc::new(jarvis_infra::messages::PgMessageStore::new(pool.clone()));
    let run_store = Arc::new(jarvis_infra::runs::PgRunStore::new(pool.clone()));
    let memory_store = Arc::new(jarvis_infra::memory::PgMemoryStore::new(pool.clone()));
    let memory_retrieval = Arc::new(MemoryRetrievalService::new(
        Arc::new(FastEmbedProvider::new(
            config.providers.embeddings.to_adapter(),
        )),
        memory_store.clone(),
    ));
    let event_log = Arc::new(jarvis_infra::events::PgEventLog::new(pool.clone()));
    let sessions = jarvisd::sessions::SessionApi::new(session_store.clone());

    // Artifact read surface (F3a.3, FR-08): manifests in Postgres, blob bytes in
    // the content-addressed file store rooted at `[storage] artifacts_root`.
    let artifact_store = Arc::new(jarvis_infra::artifacts::PgArtifactStore::new(pool.clone()));
    let blob_store = Arc::new(jarvis_infra::artifact_cas::FileBlobStore::new(
        config.storage.artifacts_root.clone(),
    ));
    let artifacts =
        jarvisd::artifacts::ArtifactApi::new(artifact_store.clone(), blob_store.clone());

    // Display profile (F3a.4, FR-09/10): surface→monitor map from `[display]`.
    // A bad assignment (unknown surface / malformed monitor) fails startup fast.
    let display_profile = Arc::new(jarvisd::display::profile_from_config(
        &config.display.profile,
    )?);

    // The WS hub is both the outbox publisher (committed domain events) and the
    // orchestrator's run-event sink (transient deltas).
    let hub = WsHub::new();

    // Voice STT is an explicit out-of-process adapter. The browser's binary
    // stream is accepted by the WS only when this opt-in provider is wired;
    // no local recognizer or ambient process is created by default.
    let transcriber: Option<Arc<dyn SpeechTranscriber>> = if config.voice.enabled {
        let address = config
            .voice
            .wyoming_stt
            .strip_prefix("tcp://")
            .ok_or_else(|| anyhow::anyhow!("[voice].wyoming_stt must use tcp://"))?;
        Some(Arc::new(jarvis_adapters::wyoming::WyomingClient::new(
            "wyoming-stt",
            address,
        )))
    } else {
        None
    };

    // The spoken response leg (F5.2). A second, independent Wyoming endpoint —
    // VAD/STT/TTS are separately swappable services (ADR-007), so this is a
    // distinct client, not a mode of the STT one. Unconfigured `wyoming_tts` ⇒
    // no TTS at all: the voice round trip still completes, it is just not
    // spoken, which is the stricter default for an outbound capability.
    let synthesizer: Option<Arc<dyn SpeechSynthesizer>> =
        match (config.voice.enabled, config.voice.wyoming_tts.as_deref()) {
            (true, Some(url)) => {
                let address = url
                    .strip_prefix("tcp://")
                    .ok_or_else(|| anyhow::anyhow!("[voice].wyoming_tts must use tcp://"))?;
                Some(Arc::new(jarvis_adapters::wyoming::WyomingClient::new(
                    "wyoming-tts",
                    address,
                )))
            }
            _ => None,
        };

    // Display placement surface (F3a.4): the hub is the directive sink to agents;
    // placements are audited through the fallible audit log before dispatch.
    let display = jarvisd::display::DisplayApi::new(
        artifact_store.clone(),
        display_profile.clone(),
        Arc::new(jarvis_infra::audit_sink::PgAuditLog::new(pool.clone())),
        hub.clone(),
    );

    // Two shutdown tokens so the outbox dispatcher outlives the runs it must
    // publish for: `serve_shutdown` stops the HTTP server and cancels in-flight
    // runs; the dispatcher only stops once those runs have drained.
    let serve_shutdown = CancellationToken::new();
    let dispatch_shutdown = CancellationToken::new();
    // The timer scheduler is a tracked task (invariant 4): joined, bounded, on
    // the shutdown path below rather than left to be killed mid-fire.
    let mut timer_scheduler: Option<tokio::task::JoinHandle<()>> = None;
    spawn_signal_listener(serve_shutdown.clone());

    // The human-approval seam (F2.5), shared by the REST surface (resolve) and
    // the orchestrator's tool plane (park), so both rendezvous on the same
    // pending-approval map.
    let approval_gate = jarvisd::approvals::JarvisApprovalGate::new(pool.clone());

    // The live tool plane (F2.6): a registry with every executor timeout-wrapped
    // at its single registration site (`jarvisd::tools`), the durable audit sink,
    // and the grant mint/validate ports. `fs.read` is left unregistered — no
    // configured root is the stricter default (no ambient filesystem authority).
    let grant_store = Arc::new(jarvis_infra::grants::PgGrantStore::new(pool.clone()));
    let smtp = if config.integrations.smtp.enabled {
        let password =
            jarvisd::config::resolve_secret_ref(&config.integrations.smtp.password_secret)?;
        Some(jarvis_adapters::smtp::SmtpConfig::new(
            config.integrations.smtp.host.clone(),
            config.integrations.smtp.port,
            config.integrations.smtp.username.clone(),
            config.integrations.smtp.from_address.clone(),
            password.expose().to_owned(),
        ))
    } else {
        None
    };
    let calendar = if config.integrations.caldav.enabled {
        let password =
            jarvisd::config::resolve_secret_ref(&config.integrations.caldav.password_secret)?;
        let caldav = jarvis_adapters::caldav::CalDavConfig::new(
            config.integrations.caldav.server_url.clone(),
            config.integrations.caldav.username.clone(),
            password.expose().to_owned(),
        )
        .map_err(|error| anyhow::anyhow!("invalid CalDAV configuration: {error}"))?;
        Some(Arc::new(
            jarvis_adapters::caldav::CalDavReader::new(caldav)
                .map_err(|error| anyhow::anyhow!("could not initialize CalDAV reader: {error}"))?,
        )
            as Arc<dyn jarvis_application::calendar::CalendarReader>)
    } else {
        None
    };
    let mut registry = jarvisd::tools::build_registry_with_smtp(None, smtp)?;
    // MCP tool servers (F2.7): none configured in M2, so no ambient MCP tool
    // authority — the stricter default. `_mcp_hosts` must live for the process
    // lifetime: each registered MCP executor holds a peer into its child, and
    // dropping a host reaps that child. Held here in `run`'s scope until shutdown.
    let _mcp_hosts =
        jarvisd::tools::register_mcp_servers(&mut registry, Vec::new(), serve_shutdown.clone())
            .await?;
    // web.search/web.fetch (F2.8): registered ONLY when a provider is configured
    // — that config presence is the external-egress consent gate (CF-5). Absent
    // ⇒ no web tools, the stricter default.
    if let Some(web) = &config.integrations.web_search {
        anyhow::ensure!(
            web.provider == "brave",
            "integrations.web_search.provider {:?} is not supported (only \"brave\")",
            web.provider
        );
        let api_key = jarvisd::config::resolve_secret_ref(&web.api_key_secret)?;
        jarvisd::tools::register_web_tools(
            &mut registry,
            api_key.expose().to_owned(),
            web.max_fetch_bytes,
        )?;
    }
    // Local media control (F3a.7, FR-22, ADR-012): registered ONLY when
    // `[integrations.media].enabled` is set AND a session bus is reachable — the
    // same opt-in stance as the web tools. Absent ⇒ no media tools, no media
    // routes, no D-Bus subscription (nothing resident, docs/09 §5).
    let max_volume = config.integrations.media.max_volume()?;
    let media_controller: Option<Arc<jarvis_adapters::media_mpris::MprisController>> = if config
        .integrations
        .media
        .enabled
    {
        match jarvis_adapters::media_mpris::MprisController::connect().await {
            Ok(controller) => Some(Arc::new(controller)),
            Err(e) => {
                tracing::warn!(error = %e, "[integrations.media].enabled but no session bus; media control off");
                None
            }
        }
    } else {
        None
    };
    if let Some(controller) = &media_controller {
        // Cast-a-link needs a monitor to cast onto: registered only when the
        // display profile actually assigns one to the media window.
        let cast = display_profile
            .monitor_for(jarvis_domain::display::Surface::MediaWindow)
            .is_some()
            .then(|| jarvisd::tools::CastWiring {
                profile: display_profile.clone(),
                sink: hub.clone(),
                audit: Arc::new(jarvis_infra::audit_sink::PgAuditLog::new(pool.clone())),
            });
        jarvisd::tools::register_media_tools(&mut registry, controller.clone(), max_volume, cast)?;
    }

    // Home Assistant (F5.3, FR-14, ADR-006): opt-in, same consent stance as the
    // web/media tools. This is the first integration that changes *physical*
    // state, so absent config means no home tools at all — never a default-on
    // capability. The token is resolved here, at the composition root, and
    // handed to the adapter as a value (invariant 5); the allowlists come from
    // config, and an enabled section with empty lists controls nothing.
    if config.integrations.home_assistant.enabled {
        let ha = &config.integrations.home_assistant;
        let token = jarvisd::config::resolve_secret_ref(&ha.token_secret)?;
        let ha_config = jarvis_adapters::home_assistant::HomeAssistantConfig::new(
            &ha.base_url,
            token.expose().to_owned(),
        )
        .map_err(|error| anyhow::anyhow!("invalid Home Assistant configuration: {error}"))?;
        let allowlist = Arc::new(
            jarvis_adapters::home_assistant::EntityAllowlist::new(
                &ha.readable,
                &ha.lights,
                &ha.scenes,
                &ha.scripts,
            )
            .map_err(|error| anyhow::anyhow!("invalid Home Assistant allowlist: {error}"))?,
        );
        let client = Arc::new(
            jarvis_adapters::home_assistant::HomeAssistantClient::new(ha_config).map_err(
                |error| anyhow::anyhow!("could not initialize Home Assistant client: {error}"),
            )?,
        );
        jarvisd::tools::register_home_assistant_tools(&mut registry, client, allowlist)?;
    }

    // Spotify (F5.6, FR-21, ADR-012/022): opt-in. `max_volume_pct` is validated
    // at config load; the R1/R2 volume split is enforced inside the tools, not
    // by the tier, because `policy::evaluate` cannot see arguments.
    if config.integrations.spotify.enabled {
        let spotify = &config.integrations.spotify;
        let refresh = jarvisd::config::resolve_secret_ref(&spotify.refresh_token_secret)?;
        let max_volume = jarvis_domain::media::VolumePct::new(spotify.max_volume_pct)
            .map_err(|error| anyhow::anyhow!("invalid [integrations.spotify]: {error}"))?;
        let mut spotify_config = jarvis_adapters::spotify::SpotifyConfig::new(
            spotify.client_id.clone(),
            refresh.expose().to_owned(),
            max_volume,
        )
        .with_device_aliases(spotify.device_aliases.clone());
        if let Some(market) = &spotify.market {
            spotify_config = spotify_config.with_market(market.clone());
        }
        let client = Arc::new(jarvis_adapters::spotify::SpotifyClient::new(spotify_config));
        jarvisd::tools::register_spotify_tools(&mut registry, client)?;
    }

    // F6.6: the generated-app builder. Opt-in (`[apps].enabled`), and the whole
    // launch profile is ops': jarvisd spawns the command it is given and hands
    // the child's pipes to the transport (ADR-027). The **host** reads and hashes
    // the lockfile, and attests `network: disabled` only when a worker image says
    // a profile that could isolate the network was actually used (D-M6-1).
    if config.apps.enabled {
        match spawn_app_builder(&config, artifact_store.clone(), blob_store.clone()).await {
            Ok(builder) => jarvisd::tools::register_app_tools(&mut registry, builder)?,
            Err(e) => {
                // A builder that cannot start registers no tool: the model then
                // gets `policy.unknown_tool`, which is the honest answer, rather
                // than a proposal that fails deep inside a worker.
                tracing::error!(error = %e, "app builder unavailable; app.generate not registered");
            }
        }
    }

    let bridge_registry = Arc::new(registry);
    let bridge_audit: Arc<dyn jarvis_application::policy::AuditSink> =
        Arc::new(jarvis_infra::audit_sink::PgAuditSink::new(pool.clone()));
    let bridge_digest: Arc<dyn jarvis_application::ports::ArgumentDigest> =
        Arc::new(jarvis_infra::appbridge::Sha256ArgumentDigest);
    // F6.5: the capability bridge shares the tool plane's registry, audit sink,
    // approval gate and grant store — an app-originated call takes the identical
    // path a model-originated one takes (invariant 1). Tokens are in-memory by
    // design: one outliving a restart would outlive the app instance it was
    // minted for.
    let capability_tokens = Arc::new(jarvis_infra::appbridge::InMemoryCapabilityTokens::new());

    let tool_plane = jarvisd::runs::ToolPlane {
        registry: bridge_registry.clone(),
        audit: bridge_audit.clone(),
        approval_gate: approval_gate.clone(),
        grant_minter: grant_store.clone(),
        grant_validator: grant_store.clone(),
        // D-M5-4: the same sha256(canonical_form(args)) the grant table binds,
        // so an executed effect is answerable from the audit trail alone.
        arg_digest: bridge_digest.clone(),
    };

    let model = Arc::new(ClaudeCliModel::with_config(
        "claude-cli",
        config.providers.claude_cli.to_adapter(),
    ));
    // The deterministic grammar (F5.5) turns "turn on the desk lamp" into a
    // `home.set_light` proposal only when the host can name the target. The
    // map is built from the SAME allowlist the tools enforce, so a spoken
    // phrase can never reach an entity the owner did not authorize — an
    // unknown target simply is not recognized and goes to the provider.
    // Absent/disabled HA ⇒ no targets ⇒ the home route stays dormant while
    // media transport still works (fail-safe, not fail-open).
    let light_targets = Arc::new(
        jarvisd::light_targets::ConfiguredLightTargets::from_allowlist(
            if config.integrations.home_assistant.enabled {
                &config.integrations.home_assistant.lights
            } else {
                &[]
            },
        ),
    );
    // "What's playing" (F5.7, FR-32/ADR-022) reads the same MPRIS snapshot the
    // media bar shows and answers as text plus a now-playing card. Wired only
    // when media control is actually present: without a controller the query is
    // not recognized at all and goes to the provider, rather than answering
    // from nothing.
    let deterministic = {
        let provider = DeterministicFirstProvider::new(model).with_light_targets(light_targets);
        match &media_controller {
            Some(controller) => provider.with_now_playing(Arc::new(
                jarvisd::media::NowPlayingHud::new(controller.clone(), Some(hub.clone())),
            )),
            None => provider,
        }
    };
    let engine = RunEngine::new(
        Arc::new(deterministic),
        Arc::new({
            let assembler = MemoryAssembler::new(memory_retrieval)
                .with_context_store(memory_store.clone())
                .with_audit(Arc::new(jarvis_infra::audit_sink::PgAuditSink::new(
                    pool.clone(),
                )));
            if let Some(calendar) = calendar {
                assembler.with_calendar(calendar)
            } else {
                assembler
            }
        }),
        run_store.clone(),
        message_store.clone(),
        hub.clone(),
        Arc::new(SystemClock),
        serve_shutdown.clone(),
        Some(tool_plane),
    );

    // Deep-dive threads (F3b.6, FR-27, ADR-017). Deliberately NOT gated on any
    // external capability and not optional: the continuation-vs-new-topic
    // decision is a pure classifier over the owner's own utterance, so a thread
    // keeps working offline, in degraded mode, and with the model quota gone —
    // the same stance as timers and lists. `[ui] deepdive_promote_after`
    // (default 3) is the promotion threshold; zero disables the *offer* only,
    // which is the documented way to turn that half off.
    //
    // Promotion reuses the artifact ports above, so a Research Notes document is
    // an ordinary versioned artifact with no second code path — the same reuse
    // as a promoted list.
    let deepdive = jarvisd::deepdive::DeepDiveApi::new(
        Arc::new(jarvis_application::deepdive::DeepDiveService::new(
            blob_store.clone(),
            artifact_store.clone(),
            config.ui.deepdive_promote_after,
            "user:owner",
            Arc::new(SystemClock),
        )),
        // The same register the message path checks against: a deep-dive slot
        // (and the artifact promoting one mints) is only ever allocated for a
        // conversation that exists.
        session_store.clone(),
        hub.clone(),
    );

    let run_api = RunApi::new(
        session_store,
        message_store.clone(),
        run_store.clone(),
        event_log.clone(),
        engine.clone(),
        approval_gate.clone(),
        Some(deepdive.clone()),
    );
    // One pairing state for the process: the routes that open a window and the
    // routes that consume it must share it (F7.2).
    let pairing = jarvisd::pairing::PairingState::new();
    // Load the certificate once, here: the fingerprint a node pins must come
    // from the very bytes the listener serves (F7.3), and a certificate that
    // cannot be loaded must stop startup before anything binds.
    let server_tls = match &config.server.tls {
        Some(tls) => Some(
            jarvisd::tls::ServerTls::load(&tls.cert_path, &tls.key_path)
                .context("loading [server.tls]")?,
        ),
        None => None,
    };
    let server_fingerprint = server_tls.as_ref().map(|t| t.fingerprint.clone());
    // docs/05 §6.2 scopes the unauthenticated health page to loopback.
    let public_health = config.bind_addr().ip().is_loopback();
    let ws_state = WsState {
        hub: hub.clone(),
        events: event_log,
        shutdown: serve_shutdown.clone(),
        transcriber,
        synthesizer,
        // A final voice transcript starts its run through the SAME use case the
        // REST message endpoint calls (`RunApi::start_turn`), carrying the
        // socket's authenticated device context — never a second, weaker path
        // from "the microphone heard it" to a run (invariant #1).
        runs: Some(run_api.clone()),
        // The SAME bus `POST /devices/{id}/revoke` publishes on (F7.1) — a
        // second `RevocationBus::new()` here would compile, serve happily, and
        // silently never close a revoked device's socket.
        revocations: auth.revocations().clone(),
        // Closes the subscribe-after-authorize race: the socket re-reads its
        // own device once, after subscribing (F7.1).
        identity: Some(auth.identity().clone()),
    };

    // Start the event-driven outbox dispatcher (LISTEN/NOTIFY, not polling) and
    // re-drive any runs left unfinished by a previous crash (NFR-05).
    let dispatcher_task = tokio::spawn(run_dispatcher(
        pool.clone(),
        hub.clone(),
        dispatch_shutdown.clone(),
    ));
    recover_unfinished_runs(&engine, run_store.as_ref(), message_store.as_ref()).await;

    // Start the health polling loop (F1.7): periodically try to dequeue and
    // re-spawn runs when the provider recovers (minimal viable: no external checks).
    let polling_engine = engine.clone();
    let polling_shutdown = serve_shutdown.clone();
    let _polling_task = tokio::spawn(async move {
        poll_provider_health(polling_engine, polling_shutdown).await;
    });

    // The media surface + its event-driven state watcher (F3a.7). The watcher is
    // a tracked task bound to `serve_shutdown` (invariant 4); it reacts to D-Bus
    // signals and never polls.
    let media_api = media_controller.as_ref().map(|controller| {
        let sink = Arc::new(jarvisd::media::MediaBroadcaster::new(
            hub.clone(),
            max_volume,
        ));
        let watcher_controller = controller.clone();
        let watcher_shutdown = serve_shutdown.clone();
        tokio::spawn(async move {
            match watcher_controller.changes().await {
                Ok(changes) => {
                    jarvis_adapters::media_mpris::watch_media_state(
                        watcher_controller.as_ref(),
                        changes,
                        sink.as_ref(),
                        watcher_shutdown,
                    )
                    .await;
                }
                Err(e) => tracing::warn!(error = %e, "media change subscription failed"),
            }
        });
        jarvisd::media::MediaApi::new(
            controller.clone(),
            Arc::new(jarvis_infra::audit_sink::PgAuditLog::new(pool.clone())),
            max_volume,
        )
    });

    // Local map serving (F3b.5, ADR-013): mounted ONLY when `[maps]
    // pmtiles_path` names an archive that opens and validates. Absent ⇒ no map
    // routes at all and the HUD takes the docs/12 §3 coverage fallback.
    //
    // A configured-but-broken archive is a config error, not a degraded mode:
    // the operator asked for this file by name, so a wrong magic, a truncated
    // download or an unsupported codec fails startup with a precise message
    // (docs/09 §1) rather than surfacing as 503s on every tile.
    let maps = match config.maps.archive_path() {
        Some(path) => {
            let archive = jarvisd::pmtiles::Archive::open(path)
                .await
                .with_context(|| format!("[maps].pmtiles_path {}", path.display()))?;
            Some(jarvisd::maps::MapApi::new(
                Arc::new(archive),
                config.maps.attribution_override(),
            ))
        }
        None => None,
    };

    // Timers / alarms / reminders (F3b.7, FR-33, ADR-023). Deliberately NOT
    // gated on any external capability: the whole point is that a timer works
    // offline, in degraded mode, and with no voice pipeline. The scheduler's
    // first pass is the missed-alarm sweep — anything that came due while this
    // process was stopped fires immediately with a "missed" notice.
    //
    // `SilentAnnouncer` is the M5 seam: voice replaces that one binding and
    // nothing else here changes. The audible alert is deliberately NOT part of
    // that seam (an alarm must sound with voice absent entirely).
    let timer_api = if config.timers.enabled {
        let service = Arc::new(jarvis_application::timers::TimerService::new(
            Arc::new(jarvis_infra::timers::PgTimerStore::new(pool.clone())),
            Arc::new(jarvis_adapters::timer_alert::CommandAlertPlayer::new(
                config.timers.alert_command.clone(),
                config.timers.alert_args.clone(),
            )),
            Arc::new(jarvis_adapters::timer_alert::SilentAnnouncer),
            Arc::new(jarvisd::timers::TimerEncoder),
            Arc::new(SystemClock),
        ));
        let wake = Arc::new(tokio::sync::Notify::new());
        let scheduler = tokio::spawn(jarvisd::timers::run_scheduler(
            service.clone(),
            wake.clone(),
            serve_shutdown.clone(),
        ));
        timer_scheduler = Some(scheduler);
        Some(jarvisd::timers::TimerApi::new(service, wake))
    } else {
        None
    };

    // Lists and quick notes (F3b.8, FR-34, ADR-024). Like timers, deliberately
    // NOT gated on any external capability — the deterministic grammar is the
    // whole point: lists keep working offline, in degraded mode, and with the
    // model quota exhausted. Promotion reuses the artifact ports above, so a
    // promoted list is an ordinary versioned artifact with no second code path.
    let list_api = if config.lists.enabled {
        Some(jarvisd::lists::ListApi::new(
            Arc::new(jarvis_application::lists::ListsService::new(
                Arc::new(jarvis_infra::lists::PgListStore::new(pool.clone())),
                blob_store,
                artifact_store.clone(),
                Arc::new(SystemClock),
            )),
            // The list card rides the same canvas event as the deep-dive cards
            // (F3b.6) — one way onto the HUD, not a second one for lists.
            Some(hub.clone()),
        ))
    } else {
        None
    };

    let state = jarvisd::api::AppState::with_database(pool.clone(), auth).with_ui_settings(
        jarvis_contracts::health::UiSettingsDto {
            background: config.ui.background.clone(),
            panel_ttl_hours: config.ui.panel_ttl_hours,
            motion: config.ui.motion.clone(),
        },
    );
    let app = jarvisd::api::router_with(
        state,
        jarvisd::api::Wiring {
            // The same instance the pairing routes were built from (F7.2).
            pairing: pairing.clone(),
            server_fingerprint: server_fingerprint.clone(),
            public_health,
            sessions: Some(sessions),
            runs: Some(RunWiring {
                runs: run_api,
                ws: ws_state,
            }),
            artifacts: Some(artifacts),
            appbridge: Some(jarvisd::appbridge::AppBridgeApi {
                artifacts: artifact_store,
                tokens: capability_tokens,
                registry: bridge_registry,
                audit: bridge_audit,
                clock: Arc::new(SystemClock),
                approval_gate: approval_gate.clone(),
                grant_minter: grant_store.clone(),
                grant_validator: grant_store,
                arg_digest: bridge_digest,
            }),
            display: Some(display),
            media: media_api,
            maps,
            timers: timer_api,
            lists: list_api,
            memories: Some(jarvisd::memories::MemoryApi::new(
                memory_store,
            )),
            deepdive: Some(deepdive),
            web_assets: config.server.web_assets.clone(),
        },
    )
    .layer(
        TraceLayer::new_for_http().make_span_with(|req: &axum::http::Request<_>| {
            tracing::info_span!("http", method = %req.method(), path = %req.uri().path())
        }),
    );

    // Disable Nagle on every accepted connection (F5.2, NFR-04). The voice
    // channel interleaves many small frames — control frames, transcripts, text
    // deltas, PCM — and Nagle holds each small write until the previous one is
    // acknowledged; the voice latency harness measured that as ~40 ms per
    // exchange on loopback, twice over in one round trip. Voice is the only path
    // here with a sub-second human-perceptible budget, so latency wins over
    // coalescing. A kernel that refuses the option is not fatal — just slower.
    // Bounded drain (invariant 4): a wedged in-flight request must not block
    // shutdown — after the signal, connections get DRAIN_DEADLINE to finish.
    let deadline_token = serve_shutdown.clone();
    let deadline = async move {
        deadline_token.cancelled().await;
        tokio::time::sleep(DRAIN_DEADLINE).await;
    };

    match &config.server.tls {
        // TLS (F7.3): the only legal way to serve anything but loopback.
        // `axum::serve` has no TLS variant, so the accept loop comes from
        // `axum-server`, which owns its own graceful-shutdown handle.
        Some(_) => {
            let loaded = server_tls.expect("loaded above whenever [server.tls] is set");
            // The value a node pins at pairing (ADR-031). Logged deliberately:
            // it is public by construction, and the owner needs to read it off
            // the journal to compare with what the node displays.
            tracing::info!(
                bind = %config.bind_addr(),
                fingerprint = %loaded.fingerprint,
                "jarvisd listening over TLS"
            );
            let listener = tokio::net::TcpListener::bind(config.bind_addr()).await?;
            let serve = jarvisd::tls::serve(listener, &loaded, app, serve_shutdown.clone());
            tokio::select! {
                result = serve => result?,
                _ = deadline => tracing::warn!("drain deadline exceeded; forcing exit"),
            }
        }
        // Plaintext, loopback only — config validation refuses anything else.
        //
        // Disable Nagle on every accepted connection (F5.2, NFR-04). The voice
        // channel interleaves many small frames — control frames, transcripts,
        // text deltas, PCM — and Nagle holds each small write until the
        // previous one is acknowledged; the voice latency harness measured that
        // as ~40 ms per exchange on loopback, twice over in one round trip.
        // Voice is the only path here with a sub-second human-perceptible
        // budget, so latency wins over coalescing. A kernel that refuses the
        // option is not fatal — just slower.
        None => {
            let listener = tokio::net::TcpListener::bind(config.bind_addr())
                .await?
                .tap_io(|stream| {
                    let _ = stream.set_nodelay(true);
                });
            tracing::info!(bind = %config.bind_addr(), "jarvisd listening");
            let cancel = serve_shutdown.clone();
            let serve = axum::serve(listener, app)
                .with_graceful_shutdown(async move { cancel.cancelled().await });
            tokio::select! {
                result = serve => result?,
                _ = deadline => tracing::warn!("drain deadline exceeded; forcing exit"),
            }
        }
    }

    // Runs were signalled to cancel with `serve_shutdown`; wait (bounded) for
    // them to checkpoint their terminal state, THEN stop the dispatcher so those
    // final events are still published.
    let _ = tokio::time::timeout(DRAIN_DEADLINE, engine.drain()).await;
    if let Some(scheduler) = timer_scheduler {
        // Bounded join: the scheduler is already cancelled by `serve_shutdown`;
        // this only waits for the pass in flight to unwind.
        let _ = tokio::time::timeout(DRAIN_DEADLINE, scheduler).await;
    }
    dispatch_shutdown.cancel();
    let _ = tokio::time::timeout(DRAIN_DEADLINE, dispatcher_task).await;

    tracing::info!("jarvisd draining telemetry and exiting");
    telemetry.shutdown();
    Ok(())
}

/// Restart backoff so a persistent dispatcher failure cannot hot-loop (CPU +
/// log flood); short enough that recovery from a transient blip stays prompt.
const DISPATCH_RESTART_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// Run the outbox dispatcher, restarting it if a transient database/publish
/// error ends the loop; a cancelled `shutdown` ends it for good.
async fn run_dispatcher(pool: sqlx::PgPool, hub: Arc<WsHub>, shutdown: CancellationToken) {
    while !shutdown.is_cancelled() {
        let dispatcher = OutboxDispatcher::new(pool.clone());
        match dispatcher.run(&*hub, shutdown.clone()).await {
            Ok(()) => return, // cancelled
            Err(error) => {
                tracing::error!(%error, "outbox dispatcher stopped; restarting");
                // Back off before reconnecting, but wake immediately on shutdown.
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(DISPATCH_RESTART_BACKOFF) => {}
                }
            }
        }
    }
}

/// Health polling loop (F1.7): periodically attempt to dequeue and re-spawn runs.
/// For F1.7 minimal viable, we do not check external provider status; instead,
/// we assume recovery has happened if we successfully dequeue and re-spawn a run.
/// If the run succeeds, the provider is healthy; if it fails again, it re-queues
/// and we try again next interval.
async fn poll_provider_health(engine: Arc<RunEngine>, shutdown: CancellationToken) {
    while !shutdown.is_cancelled() {
        // Try to dequeue and re-spawn one run per interval.
        if let Some((run, input)) = engine.try_dequeue() {
            tracing::debug!("dequeued and re-spawning run after provider recovery");
            // A requeued run carries no device identity → no tool authority
            // (invariant #1); it re-runs the model turn that failed on quota.
            engine.spawn(run, input, None);
        }
        // Idle deployments wake only every five minutes; queued work uses the
        // shorter recovery interval so an empty queue is not polled frequently.
        let interval = if engine.queued_len() > 0 {
            HEALTH_POLL_INTERVAL
        } else {
            HEALTH_IDLE_INTERVAL
        };
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

/// Re-drive runs the previous process left mid-flight (NFR-05, docs/02 §12).
///
/// M1 has no external tool effects (invariant 1), so re-running the model
/// interaction is idempotent from the outside — the reconciliation is to restart
/// each run from its input rather than resume from a half-state with a lost model
/// stream. The input is the session's latest user message (M1 runs one exchange
/// at a time); a run whose input cannot be found re-drives with empty input and
/// completes trivially rather than hanging. Precise run→message linkage arrives
/// when runs reference their originating message (a later schema addition).
async fn recover_unfinished_runs(
    engine: &Arc<RunEngine>,
    runs: &dyn RunStore,
    messages: &dyn MessageStore,
) {
    let unfinished = match runs.load_unfinished().await {
        Ok(unfinished) => unfinished,
        Err(error) => {
            // A degraded start (DB unreachable) simply recovers nothing now; a
            // later restart re-runs this sweep (docs/02 §12).
            tracing::warn!(%error, "restart recovery skipped — runs unreadable");
            return;
        }
    };
    for run in unfinished {
        let text = latest_user_text(messages, &run.session_id).await;
        tracing::info!(run_id = %run.id, "re-driving unfinished run after restart");
        // Restart from the top: same id/session/budget, fresh Received state; the
        // durable row re-converges as the orchestrator re-checkpoints.
        let fresh = Run::new(run.id, run.session_id, run.budget);
        // A crash-recovered run has no device identity → no tool authority
        // (invariant #1); M1/M2 runs re-drive the model turn idempotently.
        engine.spawn(fresh, RunInput { text }, None);
    }
}

async fn latest_user_text(messages: &dyn MessageStore, session: &SessionId) -> String {
    messages
        .list_by_session(session, 100)
        .await
        .ok()
        .and_then(|msgs| {
            msgs.into_iter()
                .rev()
                .find(|m| m.role == MessageRole::User)
                .map(|m| m.text)
        })
        .unwrap_or_default()
}

/// Health polling interval (F1.7): check if queued runs can resume. For F1.7
/// minimal viable, this simply attempts to dequeue and re-spawn; the actual
/// provider health signal comes from whether the run succeeds or fails.
const HEALTH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
const HEALTH_IDLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

const DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

fn spawn_signal_listener(shutdown: CancellationToken) {
    // Deliberately untracked spawn: this listener's only effect is flipping
    // the cancellation token and its lifetime IS the process lifetime — there
    // is nothing to drain or join at shutdown (invariant 4 exemption).
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler install");
            tokio::select! {
                _ = ctrl_c => {},
                _ = term.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
        tracing::info!("shutdown signal received");
        shutdown.cancel();
    });
}

/// Spawn the app-builder worker and assemble its host (F6.6, ADR-027).
async fn spawn_app_builder(
    config: &jarvisd::config::Config,
    artifacts: Arc<dyn jarvis_application::ports::ArtifactStore>,
    blobs: Arc<dyn jarvis_application::ports::BlobStore>,
) -> anyhow::Result<Arc<jarvis_adapters::app_builder::AppBuilderHost>> {
    use jarvis_adapters::app_builder::{AppBuilderHost, ChildAppBuilderTransport, lockfile_hash};
    use jarvis_domain::artifact::{BuildNetwork, BuildProvenance};

    let lockfile = lockfile_hash(&config.apps.lockfile)
        .await
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", config.apps.lockfile.display()))?;

    let mut child = tokio::process::Command::new(&config.apps.worker_command)
        .args(&config.apps.worker_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // The worker's stderr is inherited so an operator sees a crash. It is
        // never read into the host: a build child's stderr can carry a
        // credential (invariant 5), which is why the worker itself never
        // forwards it either.
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawning {}: {e}", config.apps.worker_command))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("app builder stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("app builder stdout unavailable"))?;

    Ok(Arc::new(AppBuilderHost::new(
        Arc::new(ChildAppBuilderTransport::new(stdin, stdout)),
        blobs,
        artifacts,
        BuildProvenance {
            worker_image: config.apps.worker_image.clone(),
            lockfile_hash: Some(lockfile),
            // Honest, not aspirational: only a launch profile with an image
            // could have isolated the network (D-M6-1).
            network: match config.apps.worker_image {
                Some(_) => BuildNetwork::Disabled,
                None => BuildNetwork::Enabled,
            },
        },
        "system:app-builder",
    )))
}
