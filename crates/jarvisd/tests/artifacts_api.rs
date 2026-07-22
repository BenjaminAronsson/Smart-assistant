//! F3a.3: artifact read surface through the production router (docs/05 §1,
//! FR-08) — real content-addressed blob store (tempdir), fake manifest store,
//! full middleware path. Covers: list versions + provenance, blob download with
//! content-addressed ETag + 304, unknown/404, fail-closed 500 on a corrupted
//! blob, auth, and reopen-through-a-fresh-app-instance (the API half of exit
//! evidence #1; the persistence half is jarvis-infra's restart test).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use jarvis_application::ports::{ArtifactStore, BlobStore, IdentityStore, RepositoryError};
use jarvis_domain::artifact::{
    ArtifactContent, ArtifactKind, ArtifactManifest, ArtifactSource, ArtifactVersion,
    BuildProvenance, MediaType,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::identity::Device;
use jarvis_domain::ids::{ArtifactId, RunId};
use jarvis_domain::location::Sensitivity;
use jarvis_infra::artifact_cas::FileBlobStore;
use jarvisd::api::{AppState, router_with};
use jarvisd::artifacts::ArtifactApi;
use jarvisd::auth::AuthState;
use std::time::SystemTime;
use tower::ServiceExt;

const ARTIFACT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";

// --- fakes --------------------------------------------------------------

#[derive(Default)]
struct FakeIdentityStore {
    devices: Mutex<Vec<Device>>,
}

#[async_trait::async_trait]
impl IdentityStore for FakeIdentityStore {
    async fn device_count(&self) -> Result<u64, RepositoryError> {
        Ok(self.devices.lock().unwrap().len() as u64)
    }
    async fn pair_device(
        &self,
        _owner_name: &str,
        device: &Device,
        _audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        self.devices.lock().unwrap().push(device.clone());
        Ok(())
    }
    async fn find_active_device_by_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<Device>, RepositoryError> {
        Ok(self
            .devices
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.token_hash == token_hash && d.is_active())
            .cloned())
    }
}

/// In-memory manifest store mirroring PgArtifactStore's contract (its DB-backed
/// tests live in jarvis-infra). Shared via Arc so a second app instance can read
/// what the first stored — the "reopen after restart" simulation at the API layer.
#[derive(Default)]
struct FakeArtifactStore {
    manifests: Mutex<Vec<ArtifactManifest>>,
}

#[async_trait::async_trait]
impl ArtifactStore for FakeArtifactStore {
    async fn create_version(
        &self,
        manifest: &ArtifactManifest,
        _audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        let mut m = self.manifests.lock().unwrap();
        if m.iter()
            .any(|e| e.id() == manifest.id() && e.version() == manifest.version())
        {
            return Err(RepositoryError::Conflict("version exists".into()));
        }
        m.push(manifest.clone());
        Ok(())
    }
    async fn get(
        &self,
        id: &ArtifactId,
        version: ArtifactVersion,
    ) -> Result<Option<ArtifactManifest>, RepositoryError> {
        Ok(self
            .manifests
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id() == id && e.version() == version)
            .cloned())
    }
    async fn latest(&self, id: &ArtifactId) -> Result<Option<ArtifactManifest>, RepositoryError> {
        Ok(self
            .manifests
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.id() == id)
            .max_by_key(|e| e.version().get())
            .cloned())
    }
    async fn list_versions(
        &self,
        id: &ArtifactId,
    ) -> Result<Vec<ArtifactManifest>, RepositoryError> {
        let mut v: Vec<ArtifactManifest> = self
            .manifests
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.id() == id)
            .cloned()
            .collect();
        v.sort_by_key(|e| e.version().get());
        Ok(v)
    }
}

// --- harness ------------------------------------------------------------

fn temp_root() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("jarvis-artapi-{}-{}", std::process::id(), nanos))
}

async fn app(store: Arc<FakeArtifactStore>, blobs: Arc<FileBlobStore>) -> (Router, String) {
    let identity = Arc::new(FakeIdentityStore::default());
    let auth = AuthState::bootstrap(identity).await;
    let code = auth.current_pairing_code().unwrap();
    let app = router_with(
        AppState::new().with_auth(auth),
        None,
        None,
        Some(ArtifactApi::new(store, blobs)),
        None,
        None,
    );
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/pair")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"pairingCode":"{code}","deviceName":"laptop"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let token = body["deviceToken"].as_str().unwrap().to_owned();
    (app, token)
}

/// Store a blob and its manifest, returning the markdown bytes used.
async fn seed(store: &FakeArtifactStore, blobs: &FileBlobStore) -> Vec<u8> {
    let bytes = b"# Research Notes\n\nmitochondria are the powerhouse of the cell".to_vec();
    let sha = blobs.put(&bytes).await.unwrap();
    let content = ArtifactContent {
        sha256: sha,
        media_type: "text/markdown".parse::<MediaType>().unwrap(),
        kind: ArtifactKind::MarkdownHtml,
        sources: vec![ArtifactSource::Run(RUN.parse::<RunId>().unwrap())],
        sensitivity: Sensitivity::Sensitive,
        build: BuildProvenance::none(),
        capabilities: vec!["artifact.read-own-data".parse().unwrap()],
    };
    let manifest =
        ArtifactManifest::initial(ARTIFACT.parse().unwrap(), RUN.parse().unwrap(), content);
    store.create_version(&manifest, &audit()).await.unwrap();
    bytes
}

fn audit() -> AuditEvent {
    AuditEvent {
        occurred_at: SystemTime::now(),
        actor: format!("run:{RUN}"),
        event_type: "artifact.created".into(),
        target: format!("artifact:{ARTIFACT}"),
        correlation_id: Some(RUN.to_owned()),
        payload_json: "{}".into(),
    }
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMapSnapshot, Vec<u8>) {
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let headers = HeaderMapSnapshot {
        etag: response
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        content_type: header_str(&response, header::CONTENT_TYPE),
        content_disposition: header_str(&response, header::CONTENT_DISPOSITION),
        nosniff: header_str(&response, header::X_CONTENT_TYPE_OPTIONS),
    };
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

fn header_str(response: &axum::response::Response, name: header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

struct HeaderMapSnapshot {
    etag: Option<String>,
    content_type: Option<String>,
    content_disposition: Option<String>,
    nosniff: Option<String>,
}

fn get(path: &str, token: &str) -> Request<Body> {
    Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

// --- tests --------------------------------------------------------------

#[tokio::test]
async fn list_versions_returns_provenance() {
    let store = Arc::new(FakeArtifactStore::default());
    let blobs = Arc::new(FileBlobStore::new(temp_root()));
    seed(&store, &blobs).await;
    let (app, token) = app(store, blobs).await;

    let (status, _h, body) = send(
        &app,
        get(&format!("/api/v1/artifacts/{ARTIFACT}/versions"), &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["artifactId"], ARTIFACT);
    let versions = v["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0]["version"], 1);
    assert_eq!(versions[0]["kind"], "markdown_html");
    assert_eq!(versions[0]["renderer"], "markdown-html/v1");
    assert_eq!(versions[0]["sensitivity"], "sensitive");
    assert_eq!(versions[0]["mediaType"], "text/markdown");
    assert_eq!(versions[0]["sources"][0]["kind"], "run");
    assert_eq!(versions[0]["capabilities"][0], "artifact.read-own-data");
}

#[tokio::test]
async fn blob_download_carries_media_type_and_content_addressed_etag() {
    let store = Arc::new(FakeArtifactStore::default());
    let blobs = Arc::new(FileBlobStore::new(temp_root()));
    let bytes = seed(&store, &blobs).await;
    let (app, token) = app(store, blobs).await;

    let (status, h, body) = send(
        &app,
        get(
            &format!("/api/v1/artifacts/{ARTIFACT}/versions/1/blob"),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, bytes, "blob bytes round-trip exactly");
    assert_eq!(h.content_type.as_deref(), Some("text/markdown"));
    // Anti-execution guard (security-auditor B1): served, never rendered inline.
    assert_eq!(h.nosniff.as_deref(), Some("nosniff"));
    assert_eq!(h.content_disposition.as_deref(), Some("attachment"));
    let etag = h.etag.expect("blob carries an ETag");
    assert!(etag.starts_with('"') && etag.ends_with('"'));

    // A matching If-None-Match short-circuits to 304 with no body.
    let (status, _h, body) = send(
        &app,
        Request::get(format!("/api/v1/artifacts/{ARTIFACT}/versions/1/blob"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::IF_NONE_MATCH, &etag)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
}

#[tokio::test]
async fn unknown_artifact_and_version_are_404() {
    let store = Arc::new(FakeArtifactStore::default());
    let blobs = Arc::new(FileBlobStore::new(temp_root()));
    seed(&store, &blobs).await;
    let (app, token) = app(store, blobs).await;

    let unknown = "01ARZ3NDEKTSV4RRFFQ69G5FZZ";
    let (status, _h, _b) = send(
        &app,
        get(&format!("/api/v1/artifacts/{unknown}/versions"), &token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Existing artifact, non-existent version 2.
    let (status, _h, _b) = send(
        &app,
        get(
            &format!("/api/v1/artifacts/{ARTIFACT}/versions/2/blob"),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A malformed version is our problem+json 404, not axum's plain-text 400.
    let (status, _h, body) = send(
        &app,
        get(
            &format!("/api/v1/artifacts/{ARTIFACT}/versions/notanum/blob"),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["code"], "resource.not_found");
}

#[tokio::test]
async fn corrupted_blob_is_a_fail_closed_500() {
    let store = Arc::new(FakeArtifactStore::default());
    let root = temp_root();
    let blobs = Arc::new(FileBlobStore::new(&root));
    seed(&store, &blobs).await;
    let (app, token) = app(store.clone(), blobs).await;

    // Tamper the on-disk blob so it no longer hashes to its address. The CAS
    // path is <root>/<aa>/<bb>/<hex>; the hex is the manifest's sha256.
    let sha = store
        .get(&ARTIFACT.parse().unwrap(), ArtifactVersion::FIRST)
        .await
        .unwrap()
        .unwrap()
        .sha256()
        .to_string();
    let path = root.join(&sha[0..2]).join(&sha[2..4]).join(&sha);
    tokio::fs::write(&path, b"tampered").await.unwrap();

    let (status, _h, body) = send(
        &app,
        get(
            &format!("/api/v1/artifacts/{ARTIFACT}/versions/1/blob"),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["code"], "artifact.integrity_failed");
}

#[tokio::test]
async fn artifact_endpoints_require_a_token() {
    let store = Arc::new(FakeArtifactStore::default());
    let blobs = Arc::new(FileBlobStore::new(temp_root()));
    let (app, _token) = app(store, blobs).await;
    let response = app
        .oneshot(
            Request::get(format!("/api/v1/artifacts/{ARTIFACT}/versions"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn artifact_reopens_through_a_fresh_app_instance() {
    // Shared backing (the Arc store + the same blob-store root) survives across
    // two independent router builds — the API-layer analogue of a restart.
    let store = Arc::new(FakeArtifactStore::default());
    let root = temp_root();
    let seed_blobs = FileBlobStore::new(&root);
    let bytes = seed(&store, &seed_blobs).await;

    // "Restart": a brand-new app instance, new ArtifactApi, same backing.
    let (app, token) = app(store, Arc::new(FileBlobStore::new(&root))).await;
    let (status, _h, body) = send(
        &app,
        get(
            &format!("/api/v1/artifacts/{ARTIFACT}/versions/1/blob"),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body, bytes,
        "the artifact reopens intact after a fresh start"
    );
}
