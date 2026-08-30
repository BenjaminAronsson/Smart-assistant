//! Moved verbatim from the monolithic home_assistant.rs (F9.5) — content
//! byte-identical, only the file location changed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use jarvis_application::policy::ToolExecutor;
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope};
use jarvis_domain::tools::{
    CanonicalValue, ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion,
};
use tokio_util::sync::CancellationToken;

use super::*;
use jarvis_domain::grants::GrantId;
use jarvis_domain::ids::{DeviceId, RunId, UserId};
use jarvis_domain::policy::ResourcePattern;

/// A scripted transport that records every request. Its very existence is
/// the assertion that no test reaches the network.
#[derive(Default)]
struct FakeTransport {
    calls: Mutex<Vec<HomeRequest>>,
    state_body: Mutex<String>,
    all_states_body: Mutex<String>,
    fail: Mutex<Option<HomeAssistantError>>,
    block_until_cancelled: bool,
    /// Signalled once the transport has actually been entered, so the
    /// cancellation test observes in-flight cancellation rather than racing
    /// the executor's entry guard.
    entered: tokio::sync::Notify,
}

impl FakeTransport {
    fn with_state(entity: &str, state: &str, name: &str) -> Self {
        let this = Self::default();
        this.set_state(entity, state, name);
        *this.all_states_body.lock().unwrap() = format!(
            r#"[{{"entity_id":"{entity}","state":"{state}","attributes":{{"friendly_name":"{name}"}}}}]"#
        );
        this
    }

    fn set_state(&self, entity: &str, state: &str, name: &str) {
        *self.state_body.lock().unwrap() = format!(
            r#"{{"entity_id":"{entity}","state":"{state}","attributes":{{"friendly_name":"{name}","area_id":"kitchen"}}}}"#
        );
    }

    fn calls(&self) -> Vec<HomeRequest> {
        self.calls.lock().unwrap().clone()
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl HomeAssistantTransport for FakeTransport {
    async fn send(
        &self,
        request: HomeRequest,
        cancel: CancellationToken,
    ) -> Result<String, HomeAssistantError> {
        self.calls.lock().unwrap().push(request.clone());
        self.entered.notify_one();
        if self.block_until_cancelled {
            cancel.cancelled().await;
            return Err(HomeAssistantError::Cancelled);
        }
        if let Some(error) = *self.fail.lock().unwrap() {
            return Err(error);
        }
        Ok(match request {
            HomeRequest::AllStates => self.all_states_body.lock().unwrap().clone(),
            HomeRequest::State(_) => self.state_body.lock().unwrap().clone(),
            HomeRequest::Service { .. } => "[]".to_owned(),
        })
    }
}

fn allowlist() -> Arc<EntityAllowlist> {
    Arc::new(
        EntityAllowlist::new(
            &["sensor.kitchen_temperature".to_owned()],
            &["light.kitchen_lamp".to_owned()],
            &["scene.movie_night".to_owned()],
            &["script.goodnight".to_owned()],
        )
        .unwrap(),
    )
}

fn client(transport: Arc<FakeTransport>) -> Arc<HomeAssistantClient> {
    Arc::new(HomeAssistantClient::with_transport(transport))
}

fn invocation(id: ToolId, arguments: CanonicalValue) -> ToolInvocation {
    ToolInvocation {
        tool_id: id,
        tool_version: ToolVersion::new(1, 0, 0),
        arguments,
    }
}

fn scene_args(entity: &str, name: &str) -> CanonicalValue {
    CanonicalValue::obj([
        ("entity_id", CanonicalValue::str(entity)),
        ("friendly_name", CanonicalValue::str(name)),
    ])
}

fn light_args(entity: &str, state: &str) -> CanonicalValue {
    CanonicalValue::obj([
        ("entity_id", CanonicalValue::str(entity)),
        ("state", CanonicalValue::str(state)),
    ])
}

/// The grant the orchestrator really mints: `target_resource` is derived
/// from the proposal's tool id (`orchestrator.rs`, the `WaitingApproval`
/// arm), so the fixture builds it the same way instead of hand-writing an
/// entity-scoped string that no minting site produces. Building it wrong is
/// what let the executor deny every approved R2 home action in production
/// while the tests stayed green.
fn grant_for(id: ToolId, args: &CanonicalValue) -> ExecutionGrant {
    let resource = grant_target_resource(&id);
    grant_with(id, args, &resource)
}

fn grant_with(id: ToolId, args: &CanonicalValue, resource: &str) -> ExecutionGrant {
    ExecutionGrant {
        grant_id: GrantId::from_bytes([9; 32]),
        user_id: "00000000000000000000000001".parse::<UserId>().unwrap(),
        device_id: "00000000000000000000000002".parse::<DeviceId>().unwrap(),
        run_id: "00000000000000000000000003".parse::<RunId>().unwrap(),
        tool_id: id,
        tool_version: ToolVersion::new(1, 0, 0),
        normalized_args_sha256: arguments_fingerprint(args),
        target_resource: resource.parse::<ResourcePattern>().unwrap(),
        expires_at: SystemTime::now() + Duration::from_secs(300),
        single_use: true,
    }
}

// ---- policy assertions -------------------------------------------------

#[test]
fn get_state_policy_is_r0_read_only_and_local() {
    let policy = HomeGetStateTool::policy();
    assert_eq!(policy.risk, RiskLevel::R0);
    assert!(!policy.requires_grant());
    assert!(policy.is_reversible);
    assert!(!policy.requires_user_presence);
    assert_eq!(policy.egress, DataEgress::Local);
    assert!(
        policy
            .required_scopes
            .contains(&Scope::new(READ_SCOPE).unwrap())
    );
}

#[test]
fn set_light_policy_is_r1_reversible_local_and_control_scoped() {
    let policy = HomeSetLightTool::policy();
    assert_eq!(policy.risk, RiskLevel::R1);
    assert!(policy.is_reversible);
    assert!(!policy.requires_grant());
    assert!(!policy.requires_user_presence);
    assert_eq!(policy.egress, DataEgress::Local);
    assert!(
        policy
            .required_scopes
            .contains(&Scope::new(CONTROL_SCOPE).unwrap())
    );
}

#[test]
fn scene_and_script_policies_are_r2_irreversible_and_require_a_grant() {
    let policy = HomeBroadTool::policy();
    assert_eq!(policy.risk, RiskLevel::R2);
    assert!(policy.requires_grant());
    assert!(!policy.is_reversible);
    assert!(policy.requires_user_presence);
    assert_eq!(policy.egress, DataEgress::Local);
    // Distinct ids: a scene approval can never execute a script.
    assert_ne!(HomeBroadTool::scene_id(), HomeBroadTool::script_id());
}

// ---- allowlist enforcement (policy cannot see arguments) ---------------

#[tokio::test]
async fn set_light_on_a_non_allowlisted_entity_is_denied_before_any_request() {
    let transport = Arc::new(FakeTransport::with_state(
        "light.bedroom_lamp",
        "off",
        "Bedroom lamp",
    ));
    let tool = HomeSetLightTool::new(client(transport.clone()), allowlist());
    let error = tool
        .execute(
            invocation(
                HomeSetLightTool::id(),
                light_args("light.bedroom_lamp", "on"),
            ),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
    assert_eq!(
        transport.call_count(),
        0,
        "denied before any transport call"
    );
}

#[tokio::test]
async fn set_light_refuses_a_non_light_entity_even_if_otherwise_allowlisted() {
    // `switch.kitchen_kettle` is on no list, and even a mis-typed config
    // could not put it on the light list (`EntityAllowlist::new` rejects it).
    let transport = Arc::new(FakeTransport::default());
    let tool = HomeSetLightTool::new(client(transport.clone()), allowlist());
    let error = tool
        .execute(
            invocation(
                HomeSetLightTool::id(),
                light_args("switch.kitchen_kettle", "on"),
            ),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
    assert_eq!(transport.call_count(), 0);
    assert_eq!(
        EntityAllowlist::new(&[], &["switch.kitchen_kettle".to_owned()], &[], &[]).err(),
        Some(AllowlistError::WrongDomain("light"))
    );
}

#[tokio::test]
async fn validate_args_rejects_a_non_allowlisted_entity_before_a_grant_is_minted() {
    // CF-9: the orchestrator calls this on the human's approved arguments,
    // so an edited entity id never reaches a minted grant.
    let transport = Arc::new(FakeTransport::default());
    let tool = HomeBroadTool::scene(client(transport.clone()), allowlist());
    let error = tool
        .validate_args(&scene_args("scene.away_mode", "Away mode"))
        .unwrap_err();
    assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
    assert_eq!(transport.call_count(), 0);
}

#[tokio::test]
async fn get_state_on_a_non_allowlisted_entity_is_denied_before_any_request() {
    let transport = Arc::new(FakeTransport::default());
    let tool = HomeGetStateTool::new(client(transport.clone()), allowlist());
    let error = tool
        .execute(
            invocation(
                HomeGetStateTool::id(),
                CanonicalValue::obj([(
                    "entity_id",
                    CanonicalValue::str("binary_sensor.front_door"),
                )]),
            ),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
    assert_eq!(transport.call_count(), 0);
}

#[tokio::test]
async fn run_script_on_a_non_allowlisted_entity_is_denied_before_any_request() {
    let transport = Arc::new(FakeTransport::default());
    let tool = HomeBroadTool::script(client(transport.clone()), allowlist());
    let args = scene_args("script.open_garage", "Open garage");
    let grant = grant_for(HomeBroadTool::script_id(), &args);
    let error = tool
        .execute(
            invocation(HomeBroadTool::script_id(), args),
            Some(grant),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
    assert_eq!(
        transport.call_count(),
        0,
        "allowlist precedes the grant path"
    );
}

// ---- grant enforcement -------------------------------------------------

#[tokio::test]
async fn r2_tool_without_a_grant_is_denied_before_any_request() {
    let transport = Arc::new(FakeTransport::with_state(
        "scene.movie_night",
        "unknown",
        "Movie night",
    ));
    let tool = HomeBroadTool::scene(client(transport.clone()), allowlist());
    let error = tool
        .execute(
            invocation(
                HomeBroadTool::scene_id(),
                scene_args("scene.movie_night", "Movie night"),
            ),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
    assert_eq!(transport.call_count(), 0);
}

#[tokio::test]
async fn a_grant_bound_to_different_arguments_is_denied_before_any_request() {
    let transport = Arc::new(FakeTransport::with_state(
        "scene.movie_night",
        "unknown",
        "Movie night",
    ));
    let tool = HomeBroadTool::scene(client(transport.clone()), allowlist());
    let approved = scene_args("scene.movie_night", "Movie night");
    let executed = scene_args("scene.movie_night", "Movie Night");
    let error = tool
        .execute(
            invocation(HomeBroadTool::scene_id(), executed),
            Some(grant_for(HomeBroadTool::scene_id(), &approved)),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
    assert_eq!(transport.call_count(), 0);
}

#[tokio::test]
async fn a_grant_for_another_resource_is_denied_before_any_request() {
    let transport = Arc::new(FakeTransport::with_state(
        "scene.movie_night",
        "unknown",
        "Movie night",
    ));
    let tool = HomeBroadTool::scene(client(transport.clone()), allowlist());
    let args = scene_args("scene.movie_night", "Movie night");
    let error = tool
        .execute(
            invocation(HomeBroadTool::scene_id(), args.clone()),
            // Right args, right tool — a resource pattern that covers a
            // different tool's grant.
            Some(grant_with(
                HomeBroadTool::scene_id(),
                &args,
                &grant_target_resource(&HomeBroadTool::script_id()),
            )),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
    assert_eq!(transport.call_count(), 0);
}

#[tokio::test]
async fn an_expired_grant_is_denied_before_any_request() {
    let transport = Arc::new(FakeTransport::with_state(
        "scene.movie_night",
        "unknown",
        "Movie night",
    ));
    let tool = HomeBroadTool::scene(client(transport.clone()), allowlist());
    let args = scene_args("scene.movie_night", "Movie night");
    let mut grant = grant_for(HomeBroadTool::scene_id(), &args);
    grant.expires_at = SystemTime::now() - Duration::from_secs(1);
    let error = tool
        .execute(
            invocation(HomeBroadTool::scene_id(), args),
            Some(grant),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
    assert_eq!(transport.call_count(), 0);
}

#[tokio::test]
async fn an_approved_scene_executes_exactly_the_curated_scene_service() {
    let transport = Arc::new(FakeTransport::with_state(
        "scene.movie_night",
        "unknown",
        "Movie night",
    ));
    let tool = HomeBroadTool::scene(client(transport.clone()), allowlist());
    let args = scene_args("scene.movie_night", "Movie night");
    let result = tool
        .execute(
            invocation(HomeBroadTool::scene_id(), args.clone()),
            Some(grant_for(HomeBroadTool::scene_id(), &args)),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // Friendly name AND entity id in the human-visible result (docs/02 §10).
    assert_eq!(result.content, "Activated Movie night (scene.movie_night).");
    assert_eq!(
        result.compensation, None,
        "R2 here is honestly irreversible"
    );
    assert_eq!(
        transport
            .calls()
            .iter()
            .filter(|call| matches!(call, HomeRequest::Service { .. }))
            .count(),
        1,
        "exactly one service call"
    );
    assert!(transport.calls().iter().any(|call| matches!(
        call,
        HomeRequest::Service {
            service: CuratedService::SceneTurnOn,
            entity,
        } if entity.as_str() == "scene.movie_night"
    )));
}

#[tokio::test]
async fn a_claimed_friendly_name_that_home_assistant_disagrees_with_is_denied() {
    // Text never grants authority: the model cannot relabel a script as
    // something benign on the approval card.
    let transport = Arc::new(FakeTransport::with_state(
        "script.goodnight",
        "off",
        "Goodnight routine",
    ));
    let tool = HomeBroadTool::script(client(transport.clone()), allowlist());
    let args = scene_args("script.goodnight", "Kitchen timer");
    let error = tool
        .execute(
            invocation(HomeBroadTool::script_id(), args.clone()),
            Some(grant_for(HomeBroadTool::script_id(), &args)),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
    assert!(
        !transport
            .calls()
            .iter()
            .any(|call| matches!(call, HomeRequest::Service { .. })),
        "no service call after a name mismatch"
    );
}

// ---- HA stays authoritative -------------------------------------------

#[tokio::test]
async fn get_state_always_reads_live_and_never_serves_a_cached_value() {
    let transport = Arc::new(FakeTransport::with_state(
        "light.kitchen_lamp",
        "off",
        "Kitchen lamp",
    ));
    let tool = HomeGetStateTool::new(client(transport.clone()), allowlist());
    let args = CanonicalValue::obj([("entity_id", CanonicalValue::str("light.kitchen_lamp"))]);

    let first = tool
        .execute(
            invocation(HomeGetStateTool::id(), args.clone()),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(first.content, "Kitchen lamp (light.kitchen_lamp) is off.");

    // Somebody flips the switch on the wall. HA is the system of record.
    transport.set_state("light.kitchen_lamp", "on", "Kitchen lamp");
    let second = tool
        .execute(
            invocation(HomeGetStateTool::id(), args),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(second.content, "Kitchen lamp (light.kitchen_lamp) is on.");
    assert_eq!(transport.call_count(), 2, "one live read per get_state");
}

#[tokio::test]
async fn metadata_is_cached_while_state_is_not() {
    let transport = Arc::new(FakeTransport::with_state(
        "scene.movie_night",
        "unknown",
        "Movie night",
    ));
    let client = client(transport.clone());
    let entity: EntityId = "scene.movie_night".parse().unwrap();
    let cancel = CancellationToken::new();

    let first = client.metadata(&entity, &cancel).await.unwrap();
    let second = client.metadata(&entity, &cancel).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(first.friendly_name, "Movie night");
    assert_eq!(first.area.as_deref(), Some("kitchen"));
    assert_eq!(transport.call_count(), 1, "second lookup hit the cache");

    // …but a state read still goes to HA every time.
    client.state(&entity, &cancel).await.unwrap();
    assert_eq!(transport.call_count(), 2);
}

#[tokio::test]
async fn set_light_registers_an_undo_derived_from_the_live_prior_state() {
    let transport = Arc::new(FakeTransport::with_state(
        "light.kitchen_lamp",
        "off",
        "Kitchen lamp",
    ));
    let tool = HomeSetLightTool::new(client(transport.clone()), allowlist());
    let result = tool
        .execute(
            invocation(
                HomeSetLightTool::id(),
                light_args("light.kitchen_lamp", "on"),
            ),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        result.content,
        "Kitchen lamp (light.kitchen_lamp) is now on."
    );
    assert_eq!(
        result.compensation.as_deref(),
        Some("Set Kitchen lamp (light.kitchen_lamp) back to off.")
    );
    assert!(transport.calls().iter().any(|call| matches!(
        call,
        HomeRequest::Service {
            service: CuratedService::LightTurnOn,
            ..
        }
    )));
}

#[tokio::test]
async fn a_failed_prior_state_read_does_not_mutate_the_light() {
    let transport = Arc::new(FakeTransport::with_state(
        "light.kitchen_lamp",
        "off",
        "Kitchen lamp",
    ));
    *transport.fail.lock().unwrap() = Some(HomeAssistantError::Unavailable);
    let tool = HomeSetLightTool::new(client(transport.clone()), allowlist());
    let error = tool
        .execute(
            invocation(
                HomeSetLightTool::id(),
                light_args("light.kitchen_lamp", "on"),
            ),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, ToolError::ExecutionFailed(_)),
        "got {error:?}"
    );
    assert!(
        !transport
            .calls()
            .iter()
            .any(|call| matches!(call, HomeRequest::Service { .. })),
        "no mutation when the undo cannot be described"
    );
}

// ---- hostile input -----------------------------------------------------

#[test]
fn entity_ids_cannot_carry_path_traversal_or_query_smuggling() {
    for hostile in [
        "../../api/services/homeassistant/restart",
        "light.kitchen_lamp/../../config",
        "light.kitchen_lamp?token=x",
        "light.kitchen_lamp#frag",
        "light.kitchen lamp",
        "light.KITCHEN",
        "light.kitchen.lamp",
        "light.",
        ".lamp",
        "lamp",
        "light.kitchen%2flamp",
        "light.kitchen\nlamp",
        &"light.".to_owned().repeat(64),
    ] {
        assert!(
            hostile.parse::<EntityId>().is_err(),
            "accepted hostile entity id: {hostile}"
        );
    }
    assert!("light.kitchen_lamp2".parse::<EntityId>().is_ok());
}

#[test]
fn a_hostile_friendly_name_is_stripped_of_control_and_bidi_characters() {
    let raw = "Kitchen\u{202E}lamp\n\u{200B}<script>";
    let cleaned = clean_text(raw, MAX_FRIENDLY_NAME_CHARS);
    assert!(!cleaned.contains('\u{202E}'));
    assert!(!cleaned.contains('\u{200B}'));
    assert!(!cleaned.contains('\n'));
    assert_eq!(cleaned, "Kitchenlamp <script>");
}

#[test]
fn an_oversized_response_body_is_bounded_rather_than_accumulated() {
    let mut body = BoundedBody::new(1024);
    assert!(body.push(&vec![b'a'; 1024]).is_ok());
    assert_eq!(
        body.push(b"one byte too many"),
        Err(HomeAssistantError::ResponseTooLarge)
    );
}

#[tokio::test]
async fn an_enormous_states_document_is_refused_by_entity_count() {
    let transport = Arc::new(FakeTransport::default());
    let entities: Vec<String> = (0..MAX_PARSED_ENTITIES + 1)
        .map(|i| format!(r#"{{"entity_id":"light.l{i}","state":"off","attributes":{{}}}}"#))
        .collect();
    *transport.all_states_body.lock().unwrap() = format!("[{}]", entities.join(","));
    let client = HomeAssistantClient::with_transport(transport);
    assert_eq!(
        client.refresh_metadata(&CancellationToken::new()).await,
        Err(HomeAssistantError::ResponseTooLarge)
    );
}

#[tokio::test]
async fn a_response_about_a_different_entity_is_rejected() {
    let transport = Arc::new(FakeTransport::with_state(
        "light.bedroom_lamp",
        "on",
        "Bedroom lamp",
    ));
    let client = HomeAssistantClient::with_transport(transport);
    let entity: EntityId = "light.kitchen_lamp".parse().unwrap();
    assert_eq!(
        client.state(&entity, &CancellationToken::new()).await,
        Err(HomeAssistantError::InvalidResponse)
    );
}

// ---- secrets -----------------------------------------------------------

#[test]
fn no_error_string_can_carry_the_token_or_the_base_url() {
    const TOKEN: &str = "ha-super-secret-token";
    let config = HomeAssistantConfig::new("https://home.example.test:8123", TOKEN).unwrap();
    // The config holds the token but exposes no Debug/Display and no getter.
    assert_eq!(config.token, TOKEN);

    for error in [
        HomeAssistantError::InvalidConfiguration,
        HomeAssistantError::Unavailable,
        HomeAssistantError::Rejected,
        HomeAssistantError::UnknownEntity,
        HomeAssistantError::InvalidResponse,
        HomeAssistantError::ResponseTooLarge,
        HomeAssistantError::Cancelled,
    ] {
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(TOKEN), "leaked token: {rendered}");
        assert!(!rendered.contains("home.example.test"), "leaked host");
        let tool_error: ToolError = error.into();
        let rendered = format!("{tool_error} {tool_error:?}");
        assert!(!rendered.contains(TOKEN), "leaked token: {rendered}");
    }
}

#[test]
fn configuration_refuses_plaintext_http_and_credential_shaped_urls() {
    for bad in [
        "http://home.example.test:8123",
        "https://user:pass@home.example.test",
        "https://home.example.test/?token=abc",
        "not a url",
    ] {
        assert_eq!(
            HomeAssistantConfig::new(bad, "token").err(),
            Some(HomeAssistantError::InvalidConfiguration),
            "accepted {bad}"
        );
    }
    for bad_token in ["", "tok en", "tok\nen"] {
        assert_eq!(
            HomeAssistantConfig::new("https://home.example.test", bad_token).err(),
            Some(HomeAssistantError::InvalidConfiguration),
        );
    }
}

#[test]
fn routing_only_ever_targets_the_configured_origin_and_curated_services() {
    let transport = RestTransport::new(
        HomeAssistantConfig::new("https://home.example.test:8123/base", "token").unwrap(),
    )
    .unwrap();
    let entity: EntityId = "light.kitchen_lamp".parse().unwrap();
    let (url, body) = transport
        .route(&HomeRequest::State(entity.clone()))
        .unwrap();
    assert_eq!(
        url.as_str(),
        "https://home.example.test:8123/base/api/states/light.kitchen_lamp"
    );
    assert!(body.is_none());

    let (url, body) = transport
        .route(&HomeRequest::Service {
            service: CuratedService::LightTurnOff,
            entity,
        })
        .unwrap();
    assert_eq!(
        url.as_str(),
        "https://home.example.test:8123/base/api/services/light/turn_off"
    );
    assert_eq!(
        body.as_deref(),
        Some(r#"{"entity_id":"light.kitchen_lamp"}"#)
    );
}

// ---- cancellation ------------------------------------------------------

#[tokio::test]
async fn cancellation_before_execution_performs_no_request() {
    let transport = Arc::new(FakeTransport::with_state(
        "light.kitchen_lamp",
        "off",
        "Kitchen lamp",
    ));
    let cancel = CancellationToken::new();
    cancel.cancel();
    for (tool, args) in [(
        HomeSetLightTool::new(client(transport.clone()), allowlist()),
        light_args("light.kitchen_lamp", "on"),
    )] {
        let error = tool
            .execute(
                invocation(HomeSetLightTool::id(), args),
                None,
                cancel.clone(),
            )
            .await
            .unwrap_err();
        assert_eq!(error, ToolError::Cancelled);
    }
    assert_eq!(transport.call_count(), 0);
}

#[tokio::test]
async fn cancellation_during_a_request_returns_promptly() {
    let transport = Arc::new(FakeTransport {
        block_until_cancelled: true,
        ..FakeTransport::with_state("light.kitchen_lamp", "off", "Kitchen lamp")
    });
    let tool = HomeGetStateTool::new(client(transport.clone()), allowlist());
    let cancel = CancellationToken::new();
    let cancel_handle = cancel.clone();
    let task = tokio::spawn(async move {
        tool.execute(
            invocation(
                HomeGetStateTool::id(),
                CanonicalValue::obj([("entity_id", CanonicalValue::str("light.kitchen_lamp"))]),
            ),
            None,
            cancel,
        )
        .await
    });
    // Cancel only once the request is genuinely in flight.
    tokio::time::timeout(Duration::from_secs(5), transport.entered.notified())
        .await
        .expect("transport was never entered");
    cancel_handle.cancel();
    let error = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("executor did not observe cancellation promptly")
        .unwrap()
        .unwrap_err();
    assert_eq!(error, ToolError::Cancelled);
    assert_eq!(
        transport.call_count(),
        1,
        "one in-flight call, then cancelled"
    );
}

// ---- argument schema ---------------------------------------------------

#[test]
fn argument_shapes_are_exact() {
    let tool = HomeSetLightTool::new(client(Arc::new(FakeTransport::default())), allowlist());
    assert!(
        tool.validate_args(&light_args("light.kitchen_lamp", "on"))
            .is_ok()
    );
    for bad in [
        CanonicalValue::obj([("entity_id", CanonicalValue::str("light.kitchen_lamp"))]),
        CanonicalValue::obj([
            ("entity_id", CanonicalValue::str("light.kitchen_lamp")),
            ("state", CanonicalValue::str("on")),
            ("brightness", CanonicalValue::int(255)),
        ]),
        CanonicalValue::obj([
            ("entity_id", CanonicalValue::str("light.kitchen_lamp")),
            ("state", CanonicalValue::int(1)),
        ]),
        CanonicalValue::str("light.kitchen_lamp"),
    ] {
        assert!(
            matches!(tool.validate_args(&bad), Err(ToolError::SchemaInvalid(_))),
            "accepted {bad:?}"
        );
    }
    assert!(matches!(
        tool.validate_args(&light_args("light.kitchen_lamp", "dim")),
        Err(ToolError::SchemaInvalid(_))
    ));
}

#[test]
fn the_grant_resource_helper_matches_what_the_orchestrator_actually_mints() {
    // Regression: the executor used to check `home:{entity}` while the only
    // minting site parses the *tool id*, and `ResourcePattern::matches` is
    // exact equality without a trailing `*`. Every approved R2 home action
    // was therefore denied after its single-use grant had been consumed.
    for tool in [HomeBroadTool::scene_id(), HomeBroadTool::script_id()] {
        // Exactly `orchestrator.rs`: `proposal.tool_id.as_str().parse()`.
        let minted: ResourcePattern = tool.as_str().parse().unwrap();
        assert_eq!(minted.as_str(), grant_target_resource(&tool));
        assert!(minted.matches(&grant_target_resource(&tool)));
        // The old, entity-scoped string is matched by nothing minted.
        assert!(!minted.matches(&format!("home:{}", "scene.movie_night")));
    }
    // …and the two tools' resources do not cover each other.
    assert!(
        !HomeBroadTool::scene_id()
            .as_str()
            .parse::<ResourcePattern>()
            .unwrap()
            .matches(&grant_target_resource(&HomeBroadTool::script_id()))
    );
}

#[tokio::test]
async fn a_grant_minted_exactly_as_the_orchestrator_mints_it_is_accepted() {
    // The test that was missing: build the grant the way the *validator*
    // builds it — resource parsed from the proposal's tool id — and assert
    // the executor honours it end to end.
    let transport = Arc::new(FakeTransport::with_state(
        "script.goodnight",
        "off",
        "Goodnight routine",
    ));
    let tool = HomeBroadTool::script(client(transport.clone()), allowlist());
    let args = scene_args("script.goodnight", "Goodnight routine");
    // Literally `proposal.tool_id.as_str().parse()`, as orchestrator.rs does.
    let grant = grant_with(
        HomeBroadTool::script_id(),
        &args,
        HomeBroadTool::script_id().as_str(),
    );

    let result = tool
        .execute(
            invocation(HomeBroadTool::script_id(), args),
            Some(grant),
            CancellationToken::new(),
        )
        .await
        .expect("an orchestrator-minted grant must be accepted");
    assert_eq!(result.content, "Ran Goodnight routine (script.goodnight).");
    assert!(transport.calls().iter().any(|call| matches!(
        call,
        HomeRequest::Service {
            service: CuratedService::ScriptTurnOn,
            entity,
        } if entity.as_str() == "script.goodnight"
    )));
}

#[tokio::test]
async fn a_grant_approved_for_another_entity_is_still_denied_by_the_fingerprint() {
    // The entity binding survives the resource-pattern change: `entity_id`
    // is a required argument, so it is inside `normalized_args_sha256`.
    let transport = Arc::new(FakeTransport::with_state(
        "scene.movie_night",
        "unknown",
        "Movie night",
    ));
    let tool = HomeBroadTool::scene(client(transport.clone()), allowlist());
    let approved = scene_args("scene.away_mode", "Away mode");
    let executed = scene_args("scene.movie_night", "Movie night");
    // Same tool, same (orchestrator-shaped) resource — different entity.
    let grant = grant_for(HomeBroadTool::scene_id(), &approved);
    let error = tool
        .execute(
            invocation(HomeBroadTool::scene_id(), executed),
            Some(grant),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Denied(_)), "got {error:?}");
    assert_eq!(transport.call_count(), 0);
}

// ---- F5.4: area resolution and partial-failure honesty -----------------

struct FakeEntity {
    state: String,
    name: String,
    area: Option<String>,
}

/// A multi-entity fixture house. Separate from [`FakeTransport`] on purpose:
/// the F5.3 tests keep their fixture untouched.
#[derive(Default)]
struct FakeHome {
    entities: Mutex<BTreeMap<String, FakeEntity>>,
    calls: Mutex<Vec<HomeRequest>>,
    state_failures: Mutex<BTreeSet<String>>,
    service_failures: Mutex<BTreeSet<String>>,
    /// Per-round-trip latency, for the M5 audit S1 deadline test. Zero
    /// everywhere else, so every other fixture keeps behaving instantly.
    latency: Mutex<Duration>,
}

impl FakeHome {
    fn add(&self, entity: &str, state: &str, name: &str, area: Option<&str>) {
        self.entities.lock().unwrap().insert(
            entity.to_owned(),
            FakeEntity {
                state: state.to_owned(),
                name: name.to_owned(),
                area: area.map(str::to_owned),
            },
        );
    }

    fn fail_service(&self, entity: &str) {
        self.service_failures
            .lock()
            .unwrap()
            .insert(entity.to_owned());
    }

    fn fail_state(&self, entity: &str) {
        self.state_failures
            .lock()
            .unwrap()
            .insert(entity.to_owned());
    }

    /// Make every round trip take `delay` — a slow Home Assistant, not a
    /// broken one: each request still succeeds, it just takes its time.
    fn set_latency(&self, delay: Duration) {
        *self.latency.lock().unwrap() = delay;
    }

    fn render(id: &str, entity: &FakeEntity) -> String {
        match &entity.area {
            Some(area) => format!(
                r#"{{"entity_id":"{id}","state":"{}","attributes":{{"friendly_name":"{}","area_id":"{area}"}}}}"#,
                entity.state, entity.name
            ),
            None => format!(
                r#"{{"entity_id":"{id}","state":"{}","attributes":{{"friendly_name":"{}"}}}}"#,
                entity.state, entity.name
            ),
        }
    }

    fn service_calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|call| match call {
                HomeRequest::Service { entity, .. } => Some(entity.as_str().to_owned()),
                _ => None,
            })
            .collect()
    }

    /// True if the transport ever saw a request *naming* this entity. The
    /// `/api/states` index names nobody, so this is exactly "was this entity
    /// read or driven".
    fn touched(&self, entity: &str) -> bool {
        self.calls.lock().unwrap().iter().any(|call| match call {
            HomeRequest::State(id) => id.as_str() == entity,
            HomeRequest::Service { entity: id, .. } => id.as_str() == entity,
            HomeRequest::AllStates => false,
        })
    }
}

#[async_trait]
impl HomeAssistantTransport for FakeHome {
    async fn send(
        &self,
        request: HomeRequest,
        cancel: CancellationToken,
    ) -> Result<String, HomeAssistantError> {
        self.calls.lock().unwrap().push(request.clone());
        if cancel.is_cancelled() {
            return Err(HomeAssistantError::Cancelled);
        }
        // Read-and-drop before awaiting: a `std::sync` guard held across an
        // await would make this future non-`Send`.
        let latency = *self.latency.lock().unwrap();
        if !latency.is_zero() {
            tokio::time::sleep(latency).await;
        }
        let entities = self.entities.lock().unwrap();
        match request {
            HomeRequest::AllStates => Ok(format!(
                "[{}]",
                entities
                    .iter()
                    .map(|(id, entity)| Self::render(id, entity))
                    .collect::<Vec<_>>()
                    .join(",")
            )),
            HomeRequest::State(id) => {
                if self.state_failures.lock().unwrap().contains(id.as_str()) {
                    return Err(HomeAssistantError::Unavailable);
                }
                entities
                    .get(id.as_str())
                    .map(|entity| Self::render(id.as_str(), entity))
                    .ok_or(HomeAssistantError::UnknownEntity)
            }
            HomeRequest::Service { entity, .. } => {
                if self
                    .service_failures
                    .lock()
                    .unwrap()
                    .contains(entity.as_str())
                {
                    return Err(HomeAssistantError::Rejected);
                }
                Ok("[]".to_owned())
            }
        }
    }
}

fn area_args(area: &str, state: &str) -> CanonicalValue {
    CanonicalValue::obj([
        ("area", CanonicalValue::str(area)),
        ("state", CanonicalValue::str(state)),
    ])
}

fn lights_allowlist(lights: &[&str]) -> Arc<EntityAllowlist> {
    Arc::new(
        EntityAllowlist::new(
            &[],
            &lights.iter().map(|l| (*l).to_owned()).collect::<Vec<_>>(),
            &[],
            &[],
        )
        .unwrap(),
    )
}

/// Three allowlisted living-room lamps plus a fourth, non-allowlisted one in
/// the same room and a lamp in another room.
fn living_room() -> (Arc<FakeHome>, Arc<EntityAllowlist>) {
    let home = Arc::new(FakeHome::default());
    home.add("light.sofa_lamp", "off", "Sofa lamp", Some("living_room"));
    home.add(
        "light.corner_lamp",
        "off",
        "Corner lamp",
        Some("living_room"),
    );
    home.add(
        "light.reading_lamp",
        "off",
        "Reading lamp",
        Some("living_room"),
    );
    // In the room, never allowlisted: must remain untouchable.
    home.add(
        "light.tv_backlight",
        "off",
        "TV backlight",
        Some("living_room"),
    );
    home.add("light.kitchen_lamp", "off", "Kitchen lamp", Some("kitchen"));
    let allowlist = lights_allowlist(&[
        "light.sofa_lamp",
        "light.corner_lamp",
        "light.reading_lamp",
        "light.kitchen_lamp",
    ]);
    (home, allowlist)
}

fn area_tool(home: Arc<FakeHome>, allowlist: Arc<EntityAllowlist>) -> HomeSetAreaLightsTool {
    HomeSetAreaLightsTool::new(
        Arc::new(HomeAssistantClient::with_transport(home)),
        allowlist,
    )
}

async fn run_area(
    tool: &HomeSetAreaLightsTool,
    area: &str,
    state: &str,
) -> Result<ToolResult, ToolError> {
    tool.execute(
        invocation(HomeSetAreaLightsTool::id(), area_args(area, state)),
        None,
        CancellationToken::new(),
    )
    .await
}

#[test]
fn set_area_lights_policy_is_r1_reversible_local_and_needs_no_grant() {
    // The F5.4 tier decision, pinned: plural stays R1 (see `policy` for the
    // argument), with the fan-out bounded in-executor instead.
    let policy = HomeSetAreaLightsTool::policy();
    assert_eq!(policy.risk, RiskLevel::R1);
    assert_eq!(policy.risk, HomeSetLightTool::policy().risk);
    assert!(policy.is_reversible);
    assert!(!policy.requires_grant());
    assert!(!policy.requires_user_presence);
    assert_eq!(policy.egress, DataEgress::Local);
    assert!(
        policy
            .required_scopes
            .contains(&Scope::new(CONTROL_SCOPE).unwrap())
    );
}

#[tokio::test]
async fn an_area_command_resolves_to_every_allowlisted_light_in_that_area() {
    let (home, allowlist) = living_room();
    let tool = area_tool(home.clone(), allowlist);
    let result = run_area(&tool, "the living room", "on").await.unwrap();

    assert_eq!(
        result.content,
        "Turned on all 3 lights in the living room: Corner lamp (light.corner_lamp), \
             Reading lamp (light.reading_lamp) and Sofa lamp (light.sofa_lamp)."
    );
    let mut driven = home.service_calls();
    driven.sort();
    assert_eq!(
        driven,
        vec![
            "light.corner_lamp".to_owned(),
            "light.reading_lamp".to_owned(),
            "light.sofa_lamp".to_owned()
        ]
    );
    // The kitchen lamp is allowlisted but in another area.
    assert!(!home.touched("light.kitchen_lamp"));
}

#[tokio::test]
async fn a_non_allowlisted_light_in_the_same_area_is_never_touched() {
    let (home, allowlist) = living_room();
    let tool = area_tool(home.clone(), allowlist);
    run_area(&tool, "living room", "on").await.unwrap();

    assert!(
        !home.touched("light.tv_backlight"),
        "sharing an area must not grant reachability"
    );
    assert!(
        !home
            .service_calls()
            .contains(&"light.tv_backlight".to_owned())
    );
}

/// M5 audit S1: a fan-out that runs long must still report what it actually
/// did.
///
/// Before this fix the tool ran up to 32 HA round trips inside a 10 s host
/// wrapper. On a slow HA the wrapper dropped the whole `execute` future
/// mid-loop: the lights already switched stayed switched, the partial report
/// was discarded, and the run was audited `tool.failed` — the owner told
/// nothing happened while half the room was lit. Now the loop watches its own
/// deadline and stops, and the entities it never reached are named.
///
/// The clock is paused, so the latency below is virtual: the test asserts a
/// 20 s budget without taking 20 s.
#[tokio::test(start_paused = true)]
async fn a_fan_out_that_outruns_its_budget_still_reports_what_it_did() {
    let (home, allowlist) = living_room();
    // 8 s per round trip. Resolution costs one (t=8 s), then each entity
    // costs two: the first finishes at 24 s and the second at 40 s, both
    // started inside the 20 s budget that opened at 8 s; the third is past
    // the deadline and is never attempted.
    home.set_latency(Duration::from_secs(8));
    let tool = area_tool(home.clone(), allowlist);
    let result = run_area(&tool, "living room", "on")
        .await
        .expect("a slow fan-out reports; it must not fail wholesale");

    assert_eq!(
        result.content,
        "Turned on 2 of 3 lights in the living room: Corner lamp (light.corner_lamp) and \
             Reading lamp (light.reading_lamp). Sofa lamp (light.sofa_lamp) was not attempted \
             because Home Assistant was too slow to reach them all in time."
    );
    assert!(result.content.contains("2 of 3"), "the count leads");
    assert!(
        !result.content.contains("all 3"),
        "a truncated fan-out is never rounded up to a success"
    );
    // The physical effect and the report agree: exactly the two lights the
    // sentence names were driven, and the undo covers exactly those two.
    assert_eq!(
        home.service_calls(),
        vec![
            "light.corner_lamp".to_owned(),
            "light.reading_lamp".to_owned()
        ]
    );
    assert!(
        !home.touched("light.sofa_lamp"),
        "an entity reported as not attempted must really not have been touched"
    );
    let compensation = result.compensation.unwrap();
    assert_eq!(compensation.matches("Set ").count(), 2);
    assert!(!compensation.contains("light.sofa_lamp"));
}

/// The other half of S1: the host wrapper must not be able to fire before the
/// tool's own deadline, or the graceful stop above never gets to happen. The
/// policy timeout is therefore derived from the work this tool may do, not
/// copied from the single-request timeout.
#[test]
fn the_host_timeout_cannot_preempt_the_fan_out_deadline() {
    let wrapper = HomeSetAreaLightsTool::policy().timeout;
    assert!(
        wrapper >= AREA_FANOUT_BUDGET + AREA_ENTITY_WORST_CASE,
        "the wrapper must outlast the budget plus the entity in flight when it \
             expires, or `execute` is dropped mid-service-call: {wrapper:?}"
    );
    assert!(
        wrapper > HomeSetLightTool::policy().timeout,
        "a fan-out is not one request; sharing REQUEST_TIMEOUT with the singular \
             tool is the defect this pins"
    );
    // …and not absurdly long either: the naive bound (one full request
    // timeout per round trip per entity) would leave an owner waiting.
    assert!(
        wrapper < REQUEST_TIMEOUT * 2 * (MAX_AREA_ENTITIES as u32),
        "the backstop must stay far below the naive worst case: {wrapper:?}"
    );
}

#[tokio::test]
async fn a_partial_failure_reports_two_of_three_and_names_the_light_that_failed() {
    // M5 exit evidence #8. The assertions below are deliberately exact: the
    // point of this test is that the wording cannot drift into a blanket
    // "done" without failing here.
    let (home, allowlist) = living_room();
    home.fail_service("light.reading_lamp");
    let tool = area_tool(home.clone(), allowlist);
    let result = run_area(&tool, "living room", "on").await.unwrap();

    assert_eq!(
        result.content,
        "Turned on 2 of 3 lights in the living room: Corner lamp (light.corner_lamp) and \
             Sofa lamp (light.sofa_lamp). Reading lamp (light.reading_lamp) did not respond."
    );
    assert!(result.content.contains("2 of 3"), "the count leads");
    assert!(
        result.content.contains("Reading lamp (light.reading_lamp)"),
        "the failure is named"
    );
    assert!(
        !result.content.contains("all 3"),
        "a partial result is never rounded up to a success"
    );
    // The undo covers only what actually changed.
    let compensation = result.compensation.unwrap();
    assert!(!compensation.contains("light.reading_lamp"));
    assert_eq!(compensation.matches("Set ").count(), 2);
    // One failure did not abort the rest: all three were attempted.
    assert_eq!(home.service_calls().len(), 3);
}

#[tokio::test]
async fn a_light_whose_prior_state_cannot_be_read_fails_without_being_mutated() {
    let (home, allowlist) = living_room();
    home.fail_state("light.reading_lamp");
    let tool = area_tool(home.clone(), allowlist);
    let result = run_area(&tool, "living room", "on").await.unwrap();

    assert!(result.content.contains("2 of 3"));
    assert!(result.content.contains("Reading lamp (light.reading_lamp)"));
    assert!(
        !home
            .service_calls()
            .contains(&"light.reading_lamp".to_owned()),
        "no mutation when that entity's undo cannot be described"
    );
}

#[tokio::test]
async fn a_total_failure_is_an_error_not_a_partial_success() {
    let (home, allowlist) = living_room();
    for entity in ["light.sofa_lamp", "light.corner_lamp", "light.reading_lamp"] {
        home.fail_service(entity);
    }
    let tool = area_tool(home.clone(), allowlist);
    let error = run_area(&tool, "living room", "on").await.unwrap_err();

    let ToolError::ExecutionFailed(message) = &error else {
        panic!("total failure must be an error, got {error:?}");
    };
    assert_eq!(
        message,
        "None of the 3 lights in the living room responded: Corner lamp (light.corner_lamp), \
             Reading lamp (light.reading_lamp) and Sofa lamp (light.sofa_lamp)."
    );
    assert!(!message.contains("Turned on"), "no success wording");
    assert!(!message.contains(" of 3"), "not reported as partial");
}

#[tokio::test]
async fn an_area_with_no_allowlisted_lights_says_so_rather_than_succeeding() {
    let (home, allowlist) = living_room();
    let tool = area_tool(home.clone(), allowlist);
    let error = run_area(&tool, "the garage", "on").await.unwrap_err();

    let ToolError::ExecutionFailed(message) = &error else {
        panic!("got {error:?}");
    };
    assert_eq!(message, "No allowlisted lights are in the garage.");
    assert!(home.service_calls().is_empty(), "nothing was driven");
}

#[tokio::test]
async fn an_area_that_home_assistant_reports_no_area_for_refuses_and_names_the_gap() {
    // The `area_id` limitation: rather than quietly resolving to nothing,
    // the refusal says how many lights could not be judged.
    let home = Arc::new(FakeHome::default());
    home.add("light.sofa_lamp", "off", "Sofa lamp", None);
    home.add("light.corner_lamp", "off", "Corner lamp", None);
    let allowlist = lights_allowlist(&["light.sofa_lamp", "light.corner_lamp"]);
    let tool = area_tool(home.clone(), allowlist);
    let error = run_area(&tool, "living room", "on").await.unwrap_err();

    let ToolError::ExecutionFailed(message) = &error else {
        panic!("got {error:?}");
    };
    assert_eq!(
        message,
        "No allowlisted lights are in the living room. 2 allowlisted lights have no known \
             area in Home Assistant and could not be considered."
    );
    assert!(home.service_calls().is_empty());
}

#[tokio::test]
async fn lights_with_no_known_area_are_surfaced_as_a_caveat_on_a_successful_result() {
    let home = Arc::new(FakeHome::default());
    home.add("light.sofa_lamp", "off", "Sofa lamp", Some("living_room"));
    home.add("light.corner_lamp", "off", "Corner lamp", None);
    let allowlist = lights_allowlist(&["light.sofa_lamp", "light.corner_lamp"]);
    let tool = area_tool(home.clone(), allowlist);
    let result = run_area(&tool, "living room", "on").await.unwrap();

    assert_eq!(
        result.content,
        "Turned on Sofa lamp (light.sofa_lamp) in the living room. 1 allowlisted light has \
             no known area in Home Assistant and could not be considered."
    );
    assert_eq!(home.service_calls(), vec!["light.sofa_lamp".to_owned()]);
}

#[tokio::test]
async fn an_allowlisted_light_home_assistant_does_not_know_counts_as_unknown_not_absent() {
    let home = Arc::new(FakeHome::default());
    home.add("light.sofa_lamp", "off", "Sofa lamp", Some("living_room"));
    // `light.ghost_lamp` is allowlisted but absent from HA entirely.
    let allowlist = lights_allowlist(&["light.sofa_lamp", "light.ghost_lamp"]);
    let tool = area_tool(home.clone(), allowlist);
    let result = run_area(&tool, "living room", "on").await.unwrap();

    assert!(
        result
            .content
            .contains("1 allowlisted light has no known area"),
        "got {}",
        result.content
    );
    assert!(!home.touched("light.ghost_lamp"));
}

#[tokio::test]
async fn the_undo_restores_each_light_to_its_own_prior_state() {
    // A blanket "turn them all off" would be wrong for the lamp that was
    // already on before the command.
    let home = Arc::new(FakeHome::default());
    home.add("light.sofa_lamp", "on", "Sofa lamp", Some("living_room"));
    home.add(
        "light.corner_lamp",
        "off",
        "Corner lamp",
        Some("living_room"),
    );
    let allowlist = lights_allowlist(&["light.sofa_lamp", "light.corner_lamp"]);
    let tool = area_tool(home.clone(), allowlist);
    let result = run_area(&tool, "living room", "on").await.unwrap();

    assert_eq!(
        result.compensation.as_deref(),
        Some(
            "Set Corner lamp (light.corner_lamp) back to off. \
                 Set Sofa lamp (light.sofa_lamp) back to on."
        )
    );
}

#[tokio::test]
async fn an_oversized_area_is_refused_rather_than_swept() {
    let home = Arc::new(FakeHome::default());
    let ids: Vec<String> = (0..=MAX_AREA_ENTITIES)
        .map(|index| format!("light.lamp_{index}"))
        .collect();
    for id in &ids {
        home.add(id, "off", "Lamp", Some("living_room"));
    }
    let allowlist = lights_allowlist(&ids.iter().map(String::as_str).collect::<Vec<_>>()[..]);
    let tool = area_tool(home.clone(), allowlist);
    let error = run_area(&tool, "living room", "on").await.unwrap_err();

    let ToolError::Denied(message) = &error else {
        panic!("got {error:?}");
    };
    assert!(message.contains("at most 16"), "got {message}");
    assert!(home.service_calls().is_empty(), "refused before any effect");
}

#[tokio::test]
async fn cancellation_before_an_area_command_performs_no_request() {
    let (home, allowlist) = living_room();
    let tool = area_tool(home.clone(), allowlist);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = tool
        .execute(
            invocation(HomeSetAreaLightsTool::id(), area_args("living room", "off")),
            None,
            cancel,
        )
        .await
        .unwrap_err();
    assert_eq!(error, ToolError::Cancelled);
    assert!(home.calls.lock().unwrap().is_empty());
}

#[test]
fn area_names_normalize_across_spoken_and_slug_forms() {
    let slug = normalize_area("living_room").unwrap();
    for spoken in ["Living Room", "living room", "living-room", "LIVING  ROOM"] {
        assert_eq!(normalize_area(spoken).as_ref(), Some(&slug), "{spoken}");
    }
    // Normalization alone keeps the article — dropping it is the label
    // pass's job, so the two stages stay independently checkable.
    assert_eq!(
        normalize_area("the living room"),
        Some(AreaKey("the_living_room".to_owned()))
    );
    assert_eq!(
        normalize_area(&area_label("The Living Room")).as_ref(),
        Some(&slug)
    );
    assert_eq!(area_label("the living room"), "living room");
    for empty in ["", "   ", "!!!"] {
        assert!(normalize_area(empty).is_none(), "{empty}");
    }
}

#[test]
fn a_hostile_area_argument_cannot_carry_markup_or_control_characters() {
    let tool = area_tool(Arc::new(FakeHome::default()), lights_allowlist(&[]));
    let label = area_label("Living\u{202E}Room\n\u{200B}<script>");
    assert!(!label.contains('\u{202E}'));
    assert!(!label.contains('\u{200B}'));
    assert!(!label.contains('\n'));
    // …and it still parses to a usable key rather than being silently empty.
    assert!(tool.validate_args(&area_args("Living Room", "on")).is_ok());
}

#[test]
fn the_area_argument_shape_is_exact() {
    let tool = area_tool(Arc::new(FakeHome::default()), lights_allowlist(&[]));
    assert!(tool.validate_args(&area_args("living room", "off")).is_ok());
    for bad in [
        CanonicalValue::obj([("area", CanonicalValue::str("living room"))]),
        CanonicalValue::obj([
            ("area", CanonicalValue::str("living room")),
            ("state", CanonicalValue::str("on")),
            ("brightness", CanonicalValue::int(255)),
        ]),
        CanonicalValue::obj([
            ("entity_id", CanonicalValue::str("light.sofa_lamp")),
            ("state", CanonicalValue::str("on")),
        ]),
        CanonicalValue::str("living room"),
        area_args("living room", "dim"),
        // Not a usable area name.
        area_args("!!!", "on"),
    ] {
        assert!(tool.validate_args(&bad).is_err(), "accepted {bad:?}");
    }
}
