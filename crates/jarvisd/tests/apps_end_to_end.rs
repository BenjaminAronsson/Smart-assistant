//! **M6 exit evidence #1** — *a dashboard app is generated* (F6.6, FR-18).
//!
//! The generation path as a user experiences it, end to end and with nothing
//! faked below the transport: the **real** `tools/app-builder` Node worker runs
//! a **real** Vite build of the locked `dashboard/v1` template, the host stores
//! the result through the **real** ports (content-addressed [`FileBlobStore`] +
//! [`PgArtifactStore`] against live Postgres), and the app is then served the
//! way the shell fetches it — through the F6.4 document route on a **fresh**
//! router, which is the restart analogue.
//!
//! What each assertion is for:
//! * the model emits a **spec**, not source, and an invalid one never reaches a
//!   worker (ADR-029);
//! * the bundle lands as an immutable v1 `Bundle` artifact carrying **real**
//!   build provenance — the first producer in the system to write anything but
//!   `BuildProvenance::none()` — with its `artifact.created` audit event in the
//!   same transaction (invariant 6);
//! * the app **reopens after a restart**, because the manifest is durable and
//!   the blob is content-addressed;
//! * it is served renderable **only** through the app route, under the sandbox
//!   CSP, while the blob route still refuses to render the same bytes;
//! * the app's declared capabilities travel on the manifest, which is what the
//!   F6.5 bridge enforces against.
//!
//! Requires `node` and the template's installed dependencies:
//! `npm --prefix tools/app-builder run install-templates`. The build is skipped
//! with a clear message when they are absent, so a fresh checkout does not fail
//! for a reason that looks like a bug.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::SystemTime;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use jarvis_adapters::app_builder::{AppBuilderHost, ChildAppBuilderTransport, lockfile_hash};
use jarvis_application::ports::{ArtifactStore, BlobStore, IdentityStore};
use jarvis_domain::artifact::{ArtifactKind, BuildNetwork, BuildProvenance, Capability};
use jarvis_domain::ids::ArtifactId;
use jarvis_domain::tools::{CanonicalValue, ToolInvocation, ToolVersion};
use jarvis_infra::artifact_cas::FileBlobStore;
use jarvis_infra::artifacts::PgArtifactStore;
use jarvis_infra::audit::verify_chain;
use jarvisd::apptool::AppGenerateTool;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

/// The document a model would emit for "make me a kitchen dashboard".
const SPEC: &str = r#"{
  "template": "dashboard/v1",
  "title": "Kitchen",
  "capabilities": ["home.read_state"],
  "bindings": [
    {"name": "kitchen_temp", "capability": "home.read_state",
     "target": "sensor.kitchen_temperature"}
  ]
}"#;

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/jarvisd
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root exists")
}

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("jarvis-m6-{tag}-{}-{nanos}", std::process::id()))
}

/// Are the locked template's dependencies installed? Their absence is a
/// configuration state, not a failure of the code under test.
fn template_ready() -> bool {
    repo_root()
        .join("tools/app-builder/templates/dashboard-v1/node_modules/vite")
        .exists()
}

fn spawn_worker() -> (
    tokio::process::Child,
    ChildAppBuilderTransport<tokio::process::ChildStdin, tokio::process::ChildStdout>,
) {
    let mut child = tokio::process::Command::new("node")
        .arg(repo_root().join("tools/app-builder/src/index.mjs"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Never forwarded into the host: a build child's stderr can carry a
        // credential (invariant 5).
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("node must be on PATH");
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    (child, ChildAppBuilderTransport::new(stdin, stdout))
}

/// The host attests the launch profile's true posture; the worker never
/// self-reports it (docs/06 §5/§6). This is ADR-027's dev/CI **process**
/// fallback, so `network: Enabled` — recorded honestly (**D-M6-1**).
async fn attested_provenance() -> BuildProvenance {
    BuildProvenance {
        worker_image: None,
        lockfile_hash: Some(
            lockfile_hash(
                repo_root().join("tools/app-builder/templates/dashboard-v1/package-lock.json"),
            )
            .await
            .expect("the committed lockfile is part of the repo"),
        ),
        network: BuildNetwork::Enabled,
    }
}

fn invocation(spec: &str) -> ToolInvocation {
    ToolInvocation {
        tool_id: AppGenerateTool::id(),
        tool_version: ToolVersion::new(1, 0, 0),
        arguments: CanonicalValue::obj([("spec", CanonicalValue::str(spec))]),
    }
}

/// A router serving the artifact/app routes over the given stores, plus a paired
/// device token — the shell's view.
async fn app_router(
    store: Arc<PgArtifactStore>,
    blobs: Arc<FileBlobStore>,
    pool: PgPool,
) -> (axum::Router, String) {
    let identity = Arc::new(jarvis_infra::identity::PgIdentityStore::new(pool));
    let auth =
        jarvisd::auth::AuthState::bootstrap(identity.clone() as Arc<dyn IdentityStore>).await;
    let code = auth.current_pairing_code().expect("first-run pairing code");
    let router = jarvisd::api::router_with(
        jarvisd::api::AppState::new().with_auth(auth),
        jarvisd::api::Wiring {
            artifacts: Some(jarvisd::artifacts::ArtifactApi::new(store, blobs)),
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
    (router, token)
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_dashboard_app_is_generated_stored_and_reopens_sandboxed(pool: PgPool) {
    if !template_ready() {
        eprintln!(
            "SKIP: the dashboard/v1 template's dependencies are not installed — run \
             `npm --prefix tools/app-builder run install-templates`"
        );
        return;
    }

    let blob_root = temp_root("apps");
    let blobs = Arc::new(FileBlobStore::new(&blob_root));
    let store = Arc::new(PgArtifactStore::new(pool.clone()));
    let (_child, transport) = spawn_worker();

    let builder = Arc::new(AppBuilderHost::new(
        Arc::new(transport),
        blobs.clone(),
        store.clone(),
        attested_provenance().await,
        "system:app-builder",
    ));
    let tool = AppGenerateTool::descriptor(builder);

    // --- 1. an invalid spec never reaches the worker ------------------------
    let bad = r#"{"template":"evil/v1","title":"x","capabilities":[],"bindings":[]}"#;
    assert!(
        tool.executor
            .validate_args(&invocation(bad).arguments)
            .is_err(),
        "an unknown template is a domain rejection, not a build failure"
    );

    // --- 2. the real build --------------------------------------------------
    let result = tool
        .executor
        .execute(invocation(SPEC), None, CancellationToken::new())
        .await
        .expect("the locked template builds");
    let summary: serde_json::Value = serde_json::from_str(&result.content).unwrap();
    let artifact_id: ArtifactId = summary["artifactId"].as_str().unwrap().parse().unwrap();
    assert_eq!(summary["version"], 1);
    assert_eq!(summary["template"], "dashboard/v1");
    assert!(
        !result.content.contains("<!doctype"),
        "bundle bytes must never be folded back into the model's context"
    );

    // --- 3. the artifact, with REAL provenance ------------------------------
    // A **fresh** store instance: the restart analogue. Nothing in memory
    // carries over; the manifest is durable and the blob content-addressed.
    let reopened = PgArtifactStore::new(pool.clone());
    let manifest = reopened
        .get(
            &artifact_id,
            jarvis_domain::artifact::ArtifactVersion::new(1).unwrap(),
        )
        .await
        .expect("store reads")
        .expect("the app reopens after a restart");
    assert_eq!(manifest.kind(), ArtifactKind::Bundle);
    assert_eq!(manifest.renderer_id(), "sandboxed-webapp/v1");
    assert_eq!(
        manifest.capabilities(),
        &[Capability::HomeReadState],
        "the manifest carries what the bridge will enforce against"
    );
    let build = manifest.build();
    assert!(
        build.lockfile_hash.is_some(),
        "the first producer to record real build provenance must record it"
    );
    assert_eq!(
        build.network,
        BuildNetwork::Enabled,
        "the process fallback attests what is true (D-M6-1), never what is flattering"
    );

    // The bytes are really in the CAS at the address the manifest names.
    let bytes = blobs
        .get(manifest.sha256())
        .await
        .expect("blob reads")
        .expect("the bundle is in the CAS");
    let document = String::from_utf8(bytes).expect("a self-contained HTML document");
    assert!(document.starts_with("<!doctype html>"));
    assert!(
        document.contains("Kitchen"),
        "the spec's title reached the rendered app"
    );

    // The audit chain still verifies with the artifact.created row in it
    // (invariant 6).
    let mut conn = pool.acquire().await.expect("a connection");
    verify_chain(&mut conn).await.expect("audit chain verifies");
    drop(conn);

    // --- 4. it opens sandboxed, and only through the app route --------------
    let (router, token) = app_router(store, blobs, pool).await;
    let get = |path: String| {
        Request::get(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };

    let response = tower::ServiceExt::oneshot(
        router.clone(),
        get(format!("/api/v1/apps/{artifact_id}/versions/1/document")),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let csp = response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|v| v.to_str().ok())
        .expect("the app document is served under a CSP")
        .to_owned();
    assert!(csp.contains("sandbox allow-scripts"), "{csp}");
    assert!(!csp.contains("allow-same-origin"), "{csp}");
    assert!(csp.contains("connect-src 'none'"), "{csp}");
    assert!(
        response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .is_none()
    );
    let served = response.into_body().collect().await.unwrap().to_bytes();
    let served = String::from_utf8(served.to_vec()).unwrap();
    assert!(
        served.starts_with("<meta http-equiv=\"Content-Security-Policy\""),
        "the host's policy is the first thing parsed"
    );
    assert!(served.ends_with(&document), "…then the bundle, unmodified");

    // The same artifact through the blob route is still a download, never a
    // render (M3a security-auditor B1 survives M6).
    let response = tower::ServiceExt::oneshot(
        router,
        get(format!("/api/v1/artifacts/{artifact_id}/versions/1/blob")),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok()),
        Some("attachment")
    );

    let _ = std::fs::remove_dir_all(&blob_root);
}
