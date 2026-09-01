//! The capability bridge's behaviour table (F6.5, docs/06 §6, invariant 1).
//!
//! This is the milestone's security surface, so the table is written the way
//! the threat note reads: one test per way an app could try to obtain authority
//! it was not given, plus the one path where it legitimately obtains none and
//! asks anyway.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use jarvis_domain::appbridge::{
    CAPABILITY_TOKEN_TTL, CapabilityToken, CapabilityTokenError, CapabilityTokenId,
};
use jarvis_domain::artifact::{
    ArtifactContent, ArtifactKind, ArtifactManifest, ArtifactSource, ArtifactVersion,
    BuildProvenance, Capability, MediaType,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::grants::Sha256;
use jarvis_domain::ids::{ArtifactId, DeviceId, RunId, UserId};
use jarvis_domain::location::Sensitivity;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, SpeechSensitivity, ToolPolicy};
use jarvis_domain::tools::{CanonicalValue, ToolId, ToolVersion};
use tokio_util::sync::CancellationToken;

use crate::appbridge::{AppBridge, BridgeActor, BridgeError, BridgeRequest, CapabilityTokenStore};
use crate::policy::{PolicyContext, ToolDescriptor, ToolRegistry};
use crate::ports::{ArtifactStore, RepositoryError};
use crate::testing::{
    FakeApprovalGate, FakeGrantMinter, FakeGrantValidator, FakeTool, FoldingArgumentDigest,
    ManualClock, RecordingAuditSink,
};

const APP: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER_APP: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB9";
const DEVICE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
const OTHER_DEVICE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB2";
const USER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB3";
const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB4";
const NOW_SECS: u64 = 1_800_000_000;

fn v1() -> ArtifactVersion {
    ArtifactVersion::new(1).unwrap()
}

fn actor() -> BridgeActor {
    BridgeActor {
        user_id: USER.parse::<UserId>().unwrap(),
        device_id: DEVICE.parse::<DeviceId>().unwrap(),
        run_id: RUN.parse::<RunId>().unwrap(),
    }
}

// --- fakes ------------------------------------------------------------------

/// Holds one app manifest, built the way the real builder builds it: a `Bundle`
/// whose capabilities came from a validated spec (F6.2).
struct FakeArtifacts {
    manifests: Vec<ArtifactManifest>,
}

impl FakeArtifacts {
    fn with(kind: ArtifactKind, capabilities: Vec<Capability>) -> Self {
        let content = ArtifactContent {
            sha256: Sha256::from_bytes([3; 32]),
            media_type: "text/html".parse::<MediaType>().unwrap(),
            kind,
            sources: vec![ArtifactSource::Run(RUN.parse::<RunId>().unwrap())],
            sensitivity: Sensitivity::Normal,
            build: BuildProvenance::none(),
            capabilities,
        };
        Self {
            manifests: vec![ArtifactManifest::initial(
                APP.parse().unwrap(),
                RUN.parse().unwrap(),
                content,
            )],
        }
    }
}

#[async_trait]
impl ArtifactStore for FakeArtifacts {
    async fn create_version(
        &self,
        _manifest: &ArtifactManifest,
        _audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
    async fn get(
        &self,
        id: &ArtifactId,
        version: ArtifactVersion,
    ) -> Result<Option<ArtifactManifest>, RepositoryError> {
        Ok(self
            .manifests
            .iter()
            .find(|m| m.id() == id && m.version() == version)
            .cloned())
    }
    async fn latest(&self, _id: &ArtifactId) -> Result<Option<ArtifactManifest>, RepositoryError> {
        Ok(self.manifests.first().cloned())
    }
    async fn list_versions(
        &self,
        _id: &ArtifactId,
    ) -> Result<Vec<ArtifactManifest>, RepositoryError> {
        Ok(self.manifests.clone())
    }
}

/// Mirrors the infra store's contract: `consume` removes.
#[derive(Default)]
struct FakeTokens {
    tokens: Mutex<HashMap<[u8; 32], CapabilityToken>>,
    next: Mutex<u8>,
}

#[async_trait]
impl CapabilityTokenStore for FakeTokens {
    async fn put(&self, token: CapabilityToken) {
        self.tokens
            .lock()
            .unwrap()
            .insert(*token.id.as_bytes(), token);
    }
    async fn consume(&self, id: &CapabilityTokenId) -> Option<CapabilityToken> {
        self.tokens.lock().unwrap().remove(id.as_bytes())
    }
    async fn new_id(&self) -> CapabilityTokenId {
        let mut n = self.next.lock().unwrap();
        *n = n.wrapping_add(1);
        CapabilityTokenId::from_bytes([*n; 32])
    }
}

// --- harness ----------------------------------------------------------------

struct Harness {
    artifacts: FakeArtifacts,
    tokens: FakeTokens,
    registry: ToolRegistry,
    audit: RecordingAuditSink,
    clock: ManualClock,
    gate: FakeApprovalGate,
    minter: FakeGrantMinter,
    validator: FakeGrantValidator,
    digest: FoldingArgumentDigest,
    tool: Arc<FakeTool>,
    scopes: PolicyContext,
}

fn policy(risk: RiskLevel, scope: &str) -> ToolPolicy {
    ToolPolicy {
        risk,
        is_reversible: true,
        requires_user_presence: false,
        timeout: Duration::from_secs(5),
        required_scopes: [Scope::new(scope).unwrap()].into_iter().collect(),
        egress: DataEgress::Local,
        speech_sensitivity: SpeechSensitivity::Normal,
    }
}

impl Harness {
    /// The default world: an app declaring `home.read_state`, whose backing tool
    /// is registered at R0 with the scope the user's session holds.
    fn new() -> Self {
        Self::with(
            vec![Capability::HomeReadState],
            ToolId::home_get_state(),
            policy(RiskLevel::R0, "home:read"),
            &["home:read"],
            FakeApprovalGate::approving(),
        )
    }

    fn with(
        declared: Vec<Capability>,
        tool_id: ToolId,
        tool_policy: ToolPolicy,
        session_scopes: &[&str],
        gate: FakeApprovalGate,
    ) -> Self {
        let tool = FakeTool::returning("{\"state\":\"21.5\"}");
        let mut registry = ToolRegistry::new();
        registry
            .register(ToolDescriptor {
                id: tool_id,
                version: ToolVersion::new(1, 0, 0),
                policy: Some(tool_policy),
                executor: tool.clone(),
            })
            .expect("registers");
        Self {
            artifacts: FakeArtifacts::with(ArtifactKind::Bundle, declared),
            tokens: FakeTokens::default(),
            registry,
            audit: RecordingAuditSink::default(),
            clock: ManualClock::at_unix(NOW_SECS),
            gate,
            minter: FakeGrantMinter,
            validator: FakeGrantValidator::accepting(),
            digest: FoldingArgumentDigest,
            tool,
            scopes: PolicyContext {
                user_id: USER.parse().unwrap(),
                device_id: DEVICE.parse().unwrap(),
                granted_scopes: session_scopes
                    .iter()
                    .map(|s| Scope::new(*s).unwrap())
                    .collect(),
            },
        }
    }

    fn bridge(&self) -> AppBridge<'_> {
        AppBridge {
            artifacts: &self.artifacts,
            tokens: &self.tokens,
            registry: &self.registry,
            audit: &self.audit,
            clock: &self.clock,
            approval_gate: &self.gate,
            grant_minter: &self.minter,
            grant_validator: &self.validator,
            arg_digest: &self.digest,
            granted_scopes: self.scopes.clone(),
        }
    }

    /// A token the bridge itself minted — the way the real caller obtains one.
    async fn valid_token(&self, capability: Capability) -> CapabilityTokenId {
        self.bridge()
            .mint_token(&APP.parse().unwrap(), v1(), capability, &actor())
            .await
            .expect("mint")
            .id
    }

    /// Put a token in the store directly, for the cases a legitimate mint could
    /// never produce (another app's token, a stale one).
    async fn plant(&self, token: CapabilityToken) -> CapabilityTokenId {
        let id = token.id;
        self.tokens.put(token).await;
        id
    }

    fn request(&self, capability: Capability, token: CapabilityTokenId) -> BridgeRequest {
        BridgeRequest {
            artifact_id: APP.parse().unwrap(),
            version: v1(),
            capability,
            target: "sensor.kitchen_temperature".to_owned(),
            value: None,
            token,
        }
    }
}

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

// --- the path that works ----------------------------------------------------

/// A declared R0 capability with a valid token runs the ordinary path and
/// executes — and the audit trail carries the **argument binding** (D-M5-4), so
/// "which read ran" is answerable, not merely "a read ran".
#[tokio::test]
async fn a_declared_capability_with_a_valid_token_executes_through_the_policy_path() {
    let h = Harness::new();
    let token = h.valid_token(Capability::HomeReadState).await;

    let result = h
        .bridge()
        .exchange(
            h.request(Capability::HomeReadState, token),
            &actor(),
            CancellationToken::new(),
        )
        .await
        .expect("the operation runs");

    assert_eq!(result.content, "{\"state\":\"21.5\"}");
    assert_eq!(
        h.audit.event_types(),
        vec!["policy.auto_authorized", "tool.executed"]
    );
    let executed = h
        .audit
        .events()
        .into_iter()
        .find(|e| e.event_type == "tool.executed")
        .unwrap();
    assert!(
        executed.payload_json.contains("args_sha256"),
        "D-M5-4: an executed effect must be bound to its arguments — {}",
        executed.payload_json
    );
    // The arguments are HOST-built from the closed capability: exactly one key,
    // named by the host, never anything the app supplied by name.
    assert_eq!(
        h.tool.call_arguments(),
        vec![CanonicalValue::obj([(
            "entity_id",
            CanonicalValue::str("sensor.kitchen_temperature")
        )])]
    );
}

// --- the headline rejection (golden 8) --------------------------------------

/// **docs/06 §6 / golden 8.** An app asking for a capability its own manifest
/// does not declare is rejected, the tool never runs, and — the part that makes
/// it evidence rather than an implementation detail — an audit row says so.
#[tokio::test]
async fn an_undeclared_capability_is_rejected_and_audited() {
    // The app declares only the read; it asks for the light.
    let h = Harness::with(
        vec![Capability::HomeReadState],
        ToolId::home_set_light(),
        policy(RiskLevel::R1, "home:write"),
        &["home:write"],
        FakeApprovalGate::approving(),
    );
    // A token that is *perfect* except for the one thing that matters: bound to
    // this app, this version, this device, well within its TTL, and bound to the
    // very capability being asked for — so the token gate passes cleanly and the
    // manifest check is the only thing standing between the app and the light.
    // A legitimate mint could never produce this (mint checks declaration too),
    // which is the point: this is the gate that holds when the first one is
    // somehow satisfied.
    let token = h
        .plant(CapabilityToken {
            id: CapabilityTokenId::from_bytes([0xd4; 32]),
            artifact_id: APP.parse().unwrap(),
            version: v1(),
            capability: Capability::HomeSetLight,
            device_id: DEVICE.parse().unwrap(),
            expires_at: at(NOW_SECS) + CAPABILITY_TOKEN_TTL,
        })
        .await;

    let mut request = h.request(Capability::HomeSetLight, token);
    request.target = "light.kitchen".to_owned();
    request.value = Some("on".to_owned());

    let err = h
        .bridge()
        .exchange(request, &actor(), CancellationToken::new())
        .await
        .expect_err("an undeclared capability must be refused");

    assert_eq!(err.code(), "app.undeclared_capability");
    assert!(h.tool.call_arguments().is_empty(), "no tool may have run");

    let denial = h
        .audit
        .events()
        .into_iter()
        .find(|e| e.event_type == "app.capability_denied")
        .expect("the refusal must be observable in the audit trail");
    assert_eq!(denial.target, "capability:home.set_light");
    assert!(denial.payload_json.contains("app.undeclared_capability"));
    assert!(
        !h.audit
            .event_types()
            .iter()
            .any(|t| t.starts_with("policy.")),
        "the policy engine is never even reached"
    );
}

/// The same rule at mint time: an app cannot even *obtain* a token for a
/// capability it does not declare, so it is refused at the first step rather
/// than handed something that fails later.
#[tokio::test]
async fn a_token_cannot_be_minted_for_an_undeclared_capability() {
    let h = Harness::new();
    let err = h
        .bridge()
        .mint_token(
            &APP.parse().unwrap(),
            v1(),
            Capability::HomeExecuteScene,
            &actor(),
        )
        .await
        .expect_err("undeclared");
    assert_eq!(err.code(), "app.undeclared_capability");
}

// --- the token matrix -------------------------------------------------------

/// A forged token names nothing the store knows. The reason is deliberately the
/// same one a spent token gets: distinguishing them would let a caller probe.
#[tokio::test]
async fn a_forged_token_is_rejected_and_audited() {
    let h = Harness::new();
    let err = h
        .bridge()
        .exchange(
            h.request(
                Capability::HomeReadState,
                CapabilityTokenId::from_bytes([0xff; 32]),
            ),
            &actor(),
            CancellationToken::new(),
        )
        .await
        .expect_err("forged");
    assert_eq!(err.code(), CapabilityTokenError::Unusable.code());
    assert!(h.tool.call_arguments().is_empty());
    assert_eq!(h.audit.event_types(), vec!["app.capability_denied"]);
}

/// A replayed token: the first exchange consumes it, the second finds nothing.
/// Single use is structural — `consume` *is* the lookup — so there is no replay
/// check to forget.
#[tokio::test]
async fn a_replayed_token_is_rejected_the_second_time() {
    let h = Harness::new();
    let token = h.valid_token(Capability::HomeReadState).await;

    h.bridge()
        .exchange(
            h.request(Capability::HomeReadState, token),
            &actor(),
            CancellationToken::new(),
        )
        .await
        .expect("first use succeeds");

    let err = h
        .bridge()
        .exchange(
            h.request(Capability::HomeReadState, token),
            &actor(),
            CancellationToken::new(),
        )
        .await
        .expect_err("replay");
    assert_eq!(err.code(), CapabilityTokenError::Unusable.code());
    assert_eq!(
        h.tool.call_arguments().len(),
        1,
        "the tool ran exactly once"
    );
}

/// An expired token is refused even though every binding on it matches.
#[tokio::test]
async fn an_expired_token_is_rejected_and_audited() {
    let h = Harness::new();
    let stale = h
        .plant(CapabilityToken {
            id: CapabilityTokenId::from_bytes([0xa1; 32]),
            artifact_id: APP.parse().unwrap(),
            version: v1(),
            capability: Capability::HomeReadState,
            device_id: DEVICE.parse().unwrap(),
            // Minted a minute and a half ago on the harness clock.
            expires_at: at(NOW_SECS) - Duration::from_secs(30),
        })
        .await;

    let err = h
        .bridge()
        .exchange(
            h.request(Capability::HomeReadState, stale),
            &actor(),
            CancellationToken::new(),
        )
        .await
        .expect_err("expired");
    assert_eq!(err.code(), "app.token_expired");
    assert!(h.tool.call_arguments().is_empty());
    assert!(
        h.audit
            .events()
            .iter()
            .any(|e| e.payload_json.contains("app.token_expired"))
    );
}

/// A token minted for **another app** does not work here, even presented by a
/// legitimate device within its TTL. This is the containment property between
/// two generated apps open at once.
#[tokio::test]
async fn a_cross_artifact_token_is_rejected() {
    let h = Harness::new();
    let foreign = h
        .plant(CapabilityToken {
            id: CapabilityTokenId::from_bytes([0xb2; 32]),
            artifact_id: OTHER_APP.parse().unwrap(),
            version: v1(),
            capability: Capability::HomeReadState,
            device_id: DEVICE.parse().unwrap(),
            expires_at: at(NOW_SECS) + CAPABILITY_TOKEN_TTL,
        })
        .await;

    let err = h
        .bridge()
        .exchange(
            h.request(Capability::HomeReadState, foreign),
            &actor(),
            CancellationToken::new(),
        )
        .await
        .expect_err("cross-artifact");
    assert_eq!(err.code(), "app.token_wrong_artifact");
    assert!(h.tool.call_arguments().is_empty());
}

/// A token minted for another device is refused: the binding is what stops a
/// leaked token from being useful anywhere else.
#[tokio::test]
async fn a_token_from_another_device_is_rejected() {
    let h = Harness::new();
    let foreign = h
        .plant(CapabilityToken {
            id: CapabilityTokenId::from_bytes([0xc3; 32]),
            artifact_id: APP.parse().unwrap(),
            version: v1(),
            capability: Capability::HomeReadState,
            device_id: OTHER_DEVICE.parse().unwrap(),
            expires_at: at(NOW_SECS) + CAPABILITY_TOKEN_TTL,
        })
        .await;

    let err = h
        .bridge()
        .exchange(
            h.request(Capability::HomeReadState, foreign),
            &actor(),
            CancellationToken::new(),
        )
        .await
        .expect_err("wrong device");
    assert_eq!(err.code(), "app.token_wrong_device");
}

/// A token for one version of an app does not carry over to another: v2
/// declares its own capabilities, so a v1 token would be authority granted
/// against a manifest nobody checked.
#[tokio::test]
async fn a_token_for_another_version_is_rejected() {
    let h = Harness::new();
    let token = h.valid_token(Capability::HomeReadState).await;
    let mut request = h.request(Capability::HomeReadState, token);
    request.version = ArtifactVersion::new(2).unwrap();

    let err = h
        .bridge()
        .exchange(request, &actor(), CancellationToken::new())
        .await
        .expect_err("wrong version");
    assert_eq!(err.code(), "app.token_wrong_version");
}

// --- the policy engine, not the declared tier -------------------------------

/// **The invariant-1 test.** `Capability::risk()` says `home.read_state` is R0.
/// The *registry* says this deployment's read tool needs approval. The bridge
/// follows the registry: a declared tier is a display preview, never a decision
/// (ADR-029 §4). If this ever inverts, an app's own manifest would be choosing
/// its authorization level.
#[tokio::test]
async fn the_live_registry_decides_the_tier_not_the_declared_capability() {
    let h = Harness::with(
        vec![Capability::HomeReadState],
        ToolId::home_get_state(),
        policy(RiskLevel::R2, "home:read"),
        &["home:read"],
        FakeApprovalGate::denying(),
    );
    let token = h.valid_token(Capability::HomeReadState).await;

    let err = h
        .bridge()
        .exchange(
            h.request(Capability::HomeReadState, token),
            &actor(),
            CancellationToken::new(),
        )
        .await
        .expect_err("an R2-registered tool needs approval, whatever the capability declares");

    assert_eq!(err.code(), "approval.denied");
    assert!(h.tool.call_arguments().is_empty());
    assert_eq!(
        h.audit.event_types(),
        vec!["policy.approval_requested", "approval.denied"]
    );
}

/// Approved R2: a real grant is minted, validated at the executor boundary and
/// presented to the tool. An app-originated effect is grant-backed exactly like
/// a model-originated one.
#[tokio::test]
async fn an_approved_r2_operation_executes_under_a_real_grant() {
    let h = Harness::with(
        vec![Capability::HomeExecuteScene],
        ToolId::home_execute_scene(),
        policy(RiskLevel::R2, "home:write"),
        &["home:write"],
        FakeApprovalGate::approving(),
    );
    let token = h.valid_token(Capability::HomeExecuteScene).await;
    let mut request = h.request(Capability::HomeExecuteScene, token);
    request.target = "scene.movie_night".to_owned();
    request.value = Some("Movie night".to_owned());

    h.bridge()
        .exchange(request, &actor(), CancellationToken::new())
        .await
        .expect("approved");

    assert_eq!(
        h.tool.calls_with_grant(),
        vec![true],
        "an R2 effect must reach the executor with a grant"
    );
    assert_eq!(
        h.audit.event_types(),
        vec!["policy.approval_requested", "grant.minted", "tool.executed"]
    );
}

/// A grant rejected at the executor boundary stops execution — the bridge does
/// not get a second opinion.
#[tokio::test]
async fn a_rejected_grant_stops_execution() {
    let mut h = Harness::with(
        vec![Capability::HomeExecuteScene],
        ToolId::home_execute_scene(),
        policy(RiskLevel::R2, "home:write"),
        &["home:write"],
        FakeApprovalGate::approving(),
    );
    h.validator = FakeGrantValidator::rejecting(jarvis_domain::grants::GrantError::Expired);
    let token = h.valid_token(Capability::HomeExecuteScene).await;
    let mut request = h.request(Capability::HomeExecuteScene, token);
    request.target = "scene.movie_night".to_owned();
    request.value = Some("Movie night".to_owned());

    let err = h
        .bridge()
        .exchange(request, &actor(), CancellationToken::new())
        .await
        .expect_err("a rejected grant authorizes nothing");
    assert_eq!(err.code(), "grant.rejected");
    assert!(h.tool.call_arguments().is_empty());
    assert!(h.audit.event_types().contains(&"grant.rejected".to_owned()));
}

/// An app can never reach a tool whose scope its owner's session lacks: the
/// bridge passes the session's scopes to `policy::evaluate` and widens nothing.
#[tokio::test]
async fn an_app_cannot_exceed_its_owners_session_scopes() {
    let h = Harness::with(
        vec![Capability::HomeReadState],
        ToolId::home_get_state(),
        policy(RiskLevel::R0, "home:read"),
        &[], // the session holds no scopes at all
        FakeApprovalGate::approving(),
    );
    let token = h.valid_token(Capability::HomeReadState).await;

    let err = h
        .bridge()
        .exchange(
            h.request(Capability::HomeReadState, token),
            &actor(),
            CancellationToken::new(),
        )
        .await
        .expect_err("missing scope");
    assert!(matches!(err, BridgeError::Policy(_)));
    assert!(h.tool.call_arguments().is_empty());
}

// --- shape of the request ---------------------------------------------------

/// An operation that takes a value cannot be invoked without one, and a read
/// cannot smuggle one: the host builds the argument tree from the closed
/// capability, so the app influences a target and (where applicable) a value —
/// and nothing else about the call.
#[tokio::test]
async fn an_operation_missing_its_value_is_refused_before_any_tool_runs() {
    let h = Harness::with(
        vec![Capability::HomeSetLight],
        ToolId::home_set_light(),
        policy(RiskLevel::R1, "home:write"),
        &["home:write"],
        FakeApprovalGate::approving(),
    );
    let token = h.valid_token(Capability::HomeSetLight).await;
    let mut request = h.request(Capability::HomeSetLight, token);
    request.target = "light.kitchen".to_owned();
    request.value = None;

    let err = h
        .bridge()
        .exchange(request, &actor(), CancellationToken::new())
        .await
        .expect_err("no value");
    assert_eq!(err.code(), "app.invalid_target");
    assert!(h.tool.call_arguments().is_empty());
}

/// A target carrying control or bidi characters never reaches a tool. (A
/// *valid* target still confers nothing — the tool re-resolves it through its
/// own allowlist.)
#[tokio::test]
async fn an_unsafe_target_is_refused() {
    let h = Harness::new();
    let token = h.valid_token(Capability::HomeReadState).await;
    let mut request = h.request(Capability::HomeReadState, token);
    request.target = "sensor.kitchen\u{202E}evil".to_owned();

    let err = h
        .bridge()
        .exchange(request, &actor(), CancellationToken::new())
        .await
        .expect_err("unsafe target");
    assert_eq!(err.code(), "app.invalid_target");
    assert!(h.tool.call_arguments().is_empty());
}

/// Only a bundle has capabilities to declare. A markdown note asked to act as
/// an app is refused — the same rule F6.4 enforces on the render path, checked
/// again here because this path grants far more than rendering.
#[tokio::test]
async fn a_non_bundle_artifact_can_never_be_a_bridge_peer() {
    let h = Harness::with(
        vec![Capability::HomeReadState],
        ToolId::home_get_state(),
        policy(RiskLevel::R0, "home:read"),
        &["home:read"],
        FakeApprovalGate::approving(),
    );
    let h = Harness {
        artifacts: FakeArtifacts::with(ArtifactKind::MarkdownHtml, vec![Capability::HomeReadState]),
        ..h
    };

    let err = h
        .bridge()
        .mint_token(
            &APP.parse().unwrap(),
            v1(),
            Capability::HomeReadState,
            &actor(),
        )
        .await
        .expect_err("not an app");
    assert_eq!(err.code(), "app.not_an_app");
}

/// A cancelled operation neither executes nor reports success (invariant 4).
#[tokio::test]
async fn a_cancelled_exchange_executes_nothing() {
    let h = Harness::new();
    let token = h.valid_token(Capability::HomeReadState).await;
    let cancel = CancellationToken::new();
    cancel.cancel();

    let err = h
        .bridge()
        .exchange(
            h.request(Capability::HomeReadState, token),
            &actor(),
            cancel,
        )
        .await
        .expect_err("cancelled");
    assert!(matches!(err, BridgeError::Cancelled));
    assert!(h.tool.call_arguments().is_empty());
}

/// **CF-9 at the bridge.** The human may edit the arguments in an approval; an
/// edit that no longer satisfies the tool's own schema must fail **before** a
/// grant binds it — the same gate the orchestrator applies, applied to the one
/// other place a grant is now minted (found at the M6 gate's audit pass).
#[tokio::test]
async fn an_edited_approval_that_breaks_the_tools_schema_never_mints_a_grant() {
    let tool = FakeTool::requiring_key("ok", "entity_id");
    let mut registry = ToolRegistry::new();
    registry
        .register(ToolDescriptor {
            id: ToolId::home_execute_scene(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(policy(RiskLevel::R2, "home:write")),
            executor: tool.clone(),
        })
        .expect("registers");

    let h = Harness {
        artifacts: FakeArtifacts::with(ArtifactKind::Bundle, vec![Capability::HomeExecuteScene]),
        tokens: FakeTokens::default(),
        registry,
        audit: RecordingAuditSink::default(),
        clock: ManualClock::at_unix(NOW_SECS),
        // Approves, but hands back arguments the tool cannot accept.
        gate: FakeApprovalGate::approving_with(CanonicalValue::obj([(
            "nonsense",
            CanonicalValue::str("x"),
        )])),
        minter: FakeGrantMinter,
        validator: FakeGrantValidator::accepting(),
        digest: FoldingArgumentDigest,
        tool: tool.clone(),
        scopes: PolicyContext {
            user_id: USER.parse().unwrap(),
            device_id: DEVICE.parse().unwrap(),
            granted_scopes: [Scope::new("home:write").unwrap()].into_iter().collect(),
        },
    };

    let token = h.valid_token(Capability::HomeExecuteScene).await;
    let mut request = h.request(Capability::HomeExecuteScene, token);
    request.target = "scene.movie_night".to_owned();
    request.value = Some("Movie night".to_owned());

    let err = h
        .bridge()
        .exchange(request, &actor(), CancellationToken::new())
        .await
        .expect_err("an invalid edit must not reach a grant");
    assert!(matches!(err, BridgeError::Tool(_)), "got {err:?}");
    assert!(h.tool.call_arguments().is_empty(), "nothing executed");
    assert!(
        h.audit
            .event_types()
            .contains(&"approval.invalid_args".to_owned()),
        "the refusal is auditable: {:?}",
        h.audit.event_types()
    );
    assert!(
        !h.audit.event_types().contains(&"grant.minted".to_owned()),
        "no grant may be minted for arguments the tool rejects"
    );
}
