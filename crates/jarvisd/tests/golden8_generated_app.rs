//! **Golden trace 8** (docs/07 §2): *"generated app requests an undeclared
//! capability; bridge rejects."* — M6 exit evidence #2 and #3 (F6.7).
//!
//! Everything above the transport is production code: the real HTTP routes
//! behind the real bearer middleware, the real `AppBridge` use case, the real
//! `policy::evaluate` and `ToolRegistry`, the real `PgAuditSink` writing to live
//! Postgres, and the real CAS. Only the *tool at the end* and the *builder
//! worker* are fakes — per CLAUDE.md's fixture-over-live rule, and because
//! neither is what this trace is about.
//!
//! The three test classes docs/01 §6 names for FR-18, and docs/06 §8 gate 4:
//!
//! * **capability denial** — undeclared, forged, malformed, replayed,
//!   cross-artifact and cross-version tokens are all rejected **and audited**.
//!   *Expiry* is not here: a 60-second wait is not a test, so the deadline is
//!   pinned by the domain table (`jarvis_domain::appbridge`) and the
//!   application table (`appbridge_tests`) against a controlled clock;
//! * **CSP** — the served app document carries the sandbox policy, and the
//!   download route still refuses to render the same bytes;
//! * **escape** — a bundle cannot reach the control origin (no same-origin
//!   relationship, asserted at the header/policy level here and at the frame
//!   level in the web suite), cannot be swapped for another artifact's blob,
//!   and the *builder* cannot be walked out of its template directory.

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use jarvis_application::policy::{
    ApprovalGate, ApprovalOutcome, ApprovalRequest, ToolDescriptor, ToolExecutor, ToolRegistry,
};
use jarvis_application::ports::{ArtifactStore, BlobStore, IdentityStore};
use jarvis_domain::artifact::{
    ArtifactContent, ArtifactKind, ArtifactManifest, ArtifactSource, BuildNetwork, BuildProvenance,
    Capability, MediaType,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::grants::{ExecutionGrant, Sha256};
use jarvis_domain::ids::{ArtifactId, RunId};
use jarvis_domain::location::Sensitivity;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{
    CanonicalValue, ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion,
};
use jarvis_infra::artifact_cas::FileBlobStore;
use jarvis_infra::artifacts::PgArtifactStore;
use jarvis_infra::audit_sink::PgAuditSink;
use jarvisd::runs::SystemClock;
use sqlx::PgPool;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tokio_util::sync::CancellationToken;

const APP: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER_APP: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB9";
const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";

/// The app document a build produced: declares `home.read_state` and nothing
/// else. It is *the manifest* that decides what the bridge allows — the bytes
/// are irrelevant to the decision, which is the point.
const BUNDLE: &str = "<!doctype html><html><body><h1>Kitchen</h1></body></html>";

// --- fakes: the tool at the end, and the approval seam ----------------------

/// Stands in for `home.get_state`. Records every call so a rejected request can
/// be shown to have reached *nothing*.
#[derive(Default)]
struct SpyTool {
    calls: Mutex<Vec<CanonicalValue>>,
}

#[async_trait]
impl ToolExecutor for SpyTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.calls.lock().unwrap().push(invocation.arguments);
        Ok(ToolResult {
            content: "21.5".to_owned(),
            truncated: false,
            compensation: None,
        })
    }
}

/// Never consulted in this trace: every request here is either R0 or refused
/// before the policy engine. Approving by default would hide a regression that
/// routed a refusal into the approval path, so it panics instead.
struct UnusedApprovalGate;

#[async_trait]
impl ApprovalGate for UnusedApprovalGate {
    async fn request(&self, _r: ApprovalRequest, _c: CancellationToken) -> ApprovalOutcome {
        panic!("golden 8 must never reach the approval seam");
    }
}

// --- harness ----------------------------------------------------------------

fn temp_root() -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("jarvis-golden8-{}-{nanos}", std::process::id()))
}

fn home_read_policy() -> ToolPolicy {
    ToolPolicy {
        risk: RiskLevel::R0,
        is_reversible: true,
        requires_user_presence: false,
        timeout: Duration::from_secs(5),
        required_scopes: [Scope::new("home:read").unwrap()].into_iter().collect(),
        egress: DataEgress::Local,
    }
}

/// Store a bundle artifact declaring exactly `capabilities`.
async fn seed_app(
    store: &PgArtifactStore,
    blobs: &FileBlobStore,
    id: &str,
    kind: ArtifactKind,
    capabilities: Vec<Capability>,
) {
    let sha = blobs.put(BUNDLE.as_bytes()).await.unwrap();
    let content = ArtifactContent {
        sha256: sha,
        media_type: "text/html".parse::<MediaType>().unwrap(),
        kind,
        sources: vec![ArtifactSource::Run(RUN.parse::<RunId>().unwrap())],
        sensitivity: Sensitivity::Normal,
        build: BuildProvenance {
            worker_image: Some("jarvis-app-builder@sha256:golden8".to_owned()),
            lockfile_hash: Some(Sha256::from_bytes([5; 32])),
            network: BuildNetwork::Disabled,
        },
        capabilities,
    };
    store
        .create_version(
            &ArtifactManifest::initial(
                id.parse::<ArtifactId>().unwrap(),
                RUN.parse::<RunId>().unwrap(),
                content,
            ),
            &AuditEvent {
                occurred_at: SystemTime::now(),
                actor: "system:app-builder".to_owned(),
                event_type: "artifact.created".to_owned(),
                target: format!("artifact:{id}"),
                correlation_id: Some(RUN.to_owned()),
                payload_json: "{}".to_owned(),
            },
        )
        .await
        .expect("seeds");
}

struct Harness {
    router: Router,
    token: String,
    tool: Arc<SpyTool>,
    pool: PgPool,
}

impl Harness {
    async fn new(pool: PgPool) -> Self {
        let blobs = Arc::new(FileBlobStore::new(temp_root()));
        let store = Arc::new(PgArtifactStore::new(pool.clone()));
        seed_app(
            &store,
            &blobs,
            APP,
            ArtifactKind::Bundle,
            vec![Capability::HomeReadState],
        )
        .await;
        // A second app declaring the *same* capability, so a cross-artifact
        // token is refused for being cross-artifact and not for anything else.
        seed_app(
            &store,
            &blobs,
            OTHER_APP,
            ArtifactKind::Bundle,
            vec![Capability::HomeReadState],
        )
        .await;

        let tool = Arc::new(SpyTool::default());
        let mut registry = ToolRegistry::new();
        registry
            .register(ToolDescriptor {
                id: ToolId::home_get_state(),
                version: ToolVersion::new(1, 0, 0),
                policy: Some(home_read_policy()),
                executor: tool.clone(),
            })
            .expect("registers");

        let grants = Arc::new(jarvis_infra::grants::PgGrantStore::new(pool.clone()));
        let identity = Arc::new(jarvis_infra::identity::PgIdentityStore::new(pool.clone()));
        let auth = jarvisd::auth::AuthState::bootstrap(identity as Arc<dyn IdentityStore>).await;
        let code = auth.current_pairing_code().expect("first-run pairing code");

        let router = jarvisd::api::router_with(
            jarvisd::api::AppState::new().with_auth(auth),
            jarvisd::api::Wiring {
                artifacts: Some(jarvisd::artifacts::ArtifactApi::new(
                    store.clone(),
                    blobs.clone(),
                )),
                appbridge: Some(jarvisd::appbridge::AppBridgeApi {
                    artifacts: store,
                    tokens: Arc::new(jarvis_infra::appbridge::InMemoryCapabilityTokens::new()),
                    registry: Arc::new(registry),
                    audit: Arc::new(PgAuditSink::new(pool.clone())),
                    clock: Arc::new(SystemClock),
                    approval_gate: Arc::new(UnusedApprovalGate),
                    grant_minter: grants.clone(),
                    grant_validator: grants,
                    arg_digest: Arc::new(jarvis_infra::appbridge::Sha256ArgumentDigest),
                }),
                ..jarvisd::api::Wiring::default()
            },
        );

        let response = tower::ServiceExt::oneshot(
            router.clone(),
            Request::post("/api/v1/auth/pair")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"pairingCode":"{code}","deviceName":"laptop"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let token = json["deviceToken"].as_str().unwrap().to_owned();

        // No scope fixup here, deliberately. The device this trace uses is
        // paired through the **real** route and holds exactly what pairing
        // grants — which is the point: until the M6 gate (finding B1, owner
        // decision 2026-08-11) pairing handed out `["ui"]` alone, so a real
        // device could execute nothing while every suite that built
        // `PolicyContext` by hand stayed green. If this trace ever needs a
        // scope patch again, that is the bug returning.

        Self {
            router,
            token,
            tool,
            pool,
        }
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = tower::ServiceExt::oneshot(self.router.clone(), request)
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    fn post(&self, path: String, body: serde_json::Value) -> Request<Body> {
        Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn get(&self, path: String) -> Request<Body> {
        Request::get(path)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token))
            .body(Body::empty())
            .unwrap()
    }

    /// Mint a token for `capability` on `APP` v1.
    async fn mint(&self, capability: &str) -> (StatusCode, serde_json::Value) {
        self.send(self.post(
            format!("/api/v1/apps/{APP}/versions/1/capability-tokens"),
            serde_json::json!({ "capability": capability }),
        ))
        .await
    }

    async fn invoke(
        &self,
        app: &str,
        capability: &str,
        target: &str,
        token: &str,
    ) -> (StatusCode, serde_json::Value) {
        self.send(self.post(
            format!("/api/v1/apps/{app}/versions/1/invoke"),
            serde_json::json!({
                "capability": capability,
                "target": target,
                "token": token,
            }),
        ))
        .await
    }

    fn tool_calls(&self) -> usize {
        self.tool.calls.lock().unwrap().len()
    }

    /// Count durable audit rows of one type — the observable half of every
    /// rejection (docs/06 §6). A refusal nobody can see is indistinguishable
    /// from an absent check.
    async fn audit_count(&self, event_type: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM audit.audit_events WHERE event_type = $1",
        )
        .bind(event_type)
        .fetch_one(&self.pool)
        .await
        .expect("audit query")
    }

    async fn denial_reasons(&self) -> Vec<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT payload::text FROM audit.audit_events \
             WHERE event_type = 'app.capability_denied' ORDER BY seq",
        )
        .fetch_all(&self.pool)
        .await
        .expect("audit query")
    }
}

// --- the trace --------------------------------------------------------------

/// **Golden 8.** The whole scenario in one test, in the order a hostile app
/// would try things.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn golden8_a_generated_app_cannot_reach_an_undeclared_capability(pool: PgPool) {
    let h = Harness::new(pool).await;

    // --- 0. the declared capability works ----------------------------------
    // Establishes that a refusal below is a refusal, not a broken harness.
    let (status, minted) = h.mint("home.read_state").await;
    assert_eq!(status, StatusCode::OK, "{minted}");
    let good_token = minted["token"].as_str().unwrap().to_owned();

    let (status, body) = h
        .invoke(
            APP,
            "home.read_state",
            "sensor.kitchen_temperature",
            &good_token,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["content"], "21.5");
    assert_eq!(h.tool_calls(), 1);
    assert_eq!(h.audit_count("tool.executed").await, 1);

    // --- 1. THE TRACE: an undeclared capability ----------------------------
    // The app declares only `home.read_state`. It asks to switch a light.
    let (status, body) = h.mint("home.set_light").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an undeclared capability must not even mint a token: {body}"
    );
    assert_eq!(body["code"], "app.undeclared_capability");
    assert_eq!(h.tool_calls(), 1, "nothing ran");

    // …and with a *legitimate* token for its declared capability, naming the
    // undeclared one at exchange time is refused too (the token gate catches
    // this one; either refusal is a refusal, and both are audited).
    let (status, minted) = h.mint("home.read_state").await;
    assert_eq!(status, StatusCode::OK, "{minted}");
    let token = minted["token"].as_str().unwrap();
    let (status, body) = h
        .invoke(APP, "home.set_light", "light.kitchen", token)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(h.tool_calls(), 1, "nothing ran");

    let denials = h.denial_reasons().await;
    assert!(
        denials
            .iter()
            .any(|d| d.contains("app.undeclared_capability")),
        "the undeclared-capability refusal must be durably auditable: {denials:?}"
    );

    // --- 2. the token matrix -----------------------------------------------
    // Forged.
    let forged = "f".repeat(64);
    let (status, body) = h
        .invoke(
            APP,
            "home.read_state",
            "sensor.kitchen_temperature",
            &forged,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "app.token_rejected");

    // Malformed — refused with the *same* code, so the shape of the refusal
    // teaches an attacker nothing about the token space.
    let (status, body) = h
        .invoke(APP, "home.read_state", "sensor.kitchen_temperature", "nope")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "app.token_rejected");

    // Replayed: a token spent successfully is gone.
    let (status, body) = h
        .invoke(
            APP,
            "home.read_state",
            "sensor.kitchen_temperature",
            &good_token,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a replayed token must find nothing: {body}"
    );
    assert_eq!(body["code"], "app.token_rejected");

    // Cross-artifact: a token minted for THIS app, presented on the other one.
    let (status, minted) = h.mint("home.read_state").await;
    assert_eq!(status, StatusCode::OK, "{minted}");
    let token = minted["token"].as_str().unwrap();
    let (status, body) = h
        .invoke(
            OTHER_APP,
            "home.read_state",
            "sensor.kitchen_temperature",
            token,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "two apps open at once must not share tokens: {body}"
    );

    // Cross-version: v1's token on v2 (which does not exist, and would declare
    // its own capabilities if it did).
    let (status, minted) = h.mint("home.read_state").await;
    assert_eq!(status, StatusCode::OK);
    let token = minted["token"].as_str().unwrap().to_owned();
    let (status, _body) = h
        .send(h.post(
            format!("/api/v1/apps/{APP}/versions/2/invoke"),
            serde_json::json!({
                "capability": "home.read_state",
                "target": "sensor.kitchen_temperature",
                "token": token,
            }),
        ))
        .await;
    assert_ne!(status, StatusCode::OK, "a v1 token may not act on v2");

    assert_eq!(
        h.tool_calls(),
        1,
        "across the entire refusal matrix, exactly one tool call ever happened — \
         the legitimate one at step 0"
    );
    assert!(
        h.audit_count("app.capability_denied").await >= 5,
        "every refusal is a durable audit row"
    );
    assert_eq!(
        h.audit_count("tool.executed").await,
        1,
        "no refusal produced an execution row"
    );
}

/// **CSP + escape.** The served app document carries the sandbox policy and the
/// download path is unchanged; a non-bundle can never be requested as an app.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn golden8_the_app_document_is_sandboxed_and_the_blob_route_still_downloads(pool: PgPool) {
    let h = Harness::new(pool).await;

    let response = tower::ServiceExt::oneshot(
        h.router.clone(),
        h.get(format!("/api/v1/apps/{APP}/versions/1/document")),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let csp = response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|v| v.to_str().ok())
        .expect("a CSP")
        .to_owned();
    // No same-origin relationship with the control UI (docs/06 §6), no network,
    // no form-action egress, no base-uri rewrite.
    assert!(csp.contains("sandbox allow-scripts"), "{csp}");
    assert!(!csp.contains("allow-same-origin"), "{csp}");
    assert!(csp.contains("default-src 'none'"), "{csp}");
    assert!(csp.contains("connect-src 'none'"), "{csp}");
    assert!(csp.contains("form-action 'none'"), "{csp}");
    assert!(csp.contains("base-uri 'none'"), "{csp}");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let document = String::from_utf8(body.to_vec()).unwrap();
    assert!(document.starts_with("<meta http-equiv=\"Content-Security-Policy\""));

    // The same bytes through the blob route are still a download.
    let response = tower::ServiceExt::oneshot(
        h.router.clone(),
        h.get(format!("/api/v1/artifacts/{APP}/versions/1/blob")),
    )
    .await
    .unwrap();
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok()),
        Some("attachment")
    );

    // An app id that names no artifact is a 404, not an empty document.
    let response = tower::ServiceExt::oneshot(
        h.router.clone(),
        h.get("/api/v1/apps/01ARZ3NDEKTSV4RRFFQ69G5FCC/versions/1/document".to_owned()),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// **Builder escape** (docs/06 §8 gate 4). The worker maps a *closed* set of
/// template ids to directories, so an id-shaped string can never become a path:
/// traversal, absolute paths and unknown ids are all simply "unknown template".
/// Requires `node`; nothing else.
#[tokio::test]
async fn golden8_the_builder_cannot_be_walked_out_of_its_template_directory() {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let worker = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/app-builder/src/index.mjs")
        .canonicalize()
        .expect("the worker source is part of the repo");

    let mut child = tokio::process::Command::new("node")
        .arg(&worker)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("node must be on PATH");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();

    for hostile in [
        "../../../etc",
        "/etc/passwd",
        "dashboard/v1/../../../..",
        "..%2f..%2fetc",
        "dashboard/v2",
    ] {
        let request = serde_json::json!({
            "build_id": 1,
            "template": hostile,
            "title": "x",
            "capabilities": [],
            "bindings": [],
            "max_bundle_bytes": 1024,
            "max_build_seconds": 5,
        });
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();

        let line = stdout
            .next_line()
            .await
            .unwrap()
            .expect("the worker answers every request");
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["ok"], false, "`{hostile}` must not build: {reply}");
        assert_eq!(
            reply["error"], "unknown template",
            "`{hostile}` must be refused as an unknown id — never resolved as a path: {reply}"
        );
        assert!(
            reply["bundle"].is_null(),
            "a refused build returns no bytes"
        );
    }
}
