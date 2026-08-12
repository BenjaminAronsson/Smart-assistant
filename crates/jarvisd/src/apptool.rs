//! `app.generate` — the tool a run proposes to build a generated app (F6.6,
//! FR-18, exit evidence #1).
//!
//! The end of the chain M6 built: a model emits a **spec** (never source), the
//! domain validates it (F6.1), the builder renders it against a locked template
//! (F6.2), the result lands in the CAS as a `Bundle` with real provenance, and
//! the shell opens it in an opaque origin (F6.4) where it may ask for exactly
//! the capabilities it declared (F6.5).
//!
//! The tool lives in `jarvisd` rather than in `jarvis-adapters` for one reason:
//! it mints an [`ArtifactId`], and the host owns randomness. Everything that
//! could be done without randomness — the transport, the caps, the provenance,
//! the artifact write — is in the adapter.

use std::sync::Arc;

use async_trait::async_trait;
use jarvis_adapters::app_builder::{AppBuildError, AppBuilderHost, app_build_policy};
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_contracts::appspec::parse_and_validate;
use jarvis_domain::ids::{ArtifactId, RunId};
use jarvis_domain::policy::ToolPolicy;
use jarvis_domain::tools::{
    CanonicalValue, ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion,
    sanitize_result_content,
};
use tokio_util::sync::CancellationToken;

use crate::auth::fresh_id;

/// Cap on the diagnostic a failed build folds back into the model's context
/// (invariant 5).
const MAX_DIAGNOSTIC_BYTES: usize = 512;

/// Builds an app from a model-authored spec.
pub struct AppGenerateTool {
    builder: Arc<AppBuilderHost>,
}

impl AppGenerateTool {
    pub fn id() -> ToolId {
        "app.generate".parse().expect("static tool id is valid")
    }

    /// The host-owned policy — [`app_build_policy`] unchanged. Registered here
    /// rather than restated, so the tier the registry enforces and the tier the
    /// adapter documents cannot drift.
    pub fn policy() -> ToolPolicy {
        app_build_policy()
    }

    pub fn descriptor(builder: Arc<AppBuilderHost>) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: Arc::new(Self { builder }),
        }
    }

    /// The one argument: the spec document, verbatim. A *string* rather than a
    /// structured argument tree because the spec's own size limit is defined in
    /// bytes of the document the model emitted (`MAX_APP_SPEC_BYTES`), and
    /// re-serializing an argument tree to measure it would measure something
    /// else.
    fn spec_document(arguments: &CanonicalValue) -> Result<&str, ToolError> {
        match arguments {
            CanonicalValue::Object(map) => match map.get("spec") {
                Some(CanonicalValue::Str(s)) => Ok(s),
                _ => Err(ToolError::SchemaInvalid(
                    "app.generate arguments must be exactly {spec: <json document>}".to_owned(),
                )),
            },
            _ => Err(ToolError::SchemaInvalid(
                "app.generate arguments must be an object".to_owned(),
            )),
        }
    }
}

#[async_trait]
impl ToolExecutor for AppGenerateTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<jarvis_domain::grants::ExecutionGrant>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let document = Self::spec_document(&invocation.arguments)?;
        // Validation is total, pure and happens **before** a worker is driven:
        // an invalid spec fails here with a typed reason a replan can act on,
        // not inside Node as a timeout (ADR-029 §1).
        let spec = parse_and_validate(document)
            .map_err(|e| ToolError::SchemaInvalid(sanitize_diag(&e.to_string())))?;

        // The host mints the artifact id (randomness is the host's) and a
        // correlation run id: `ToolInvocation` carries no run id, so the artifact's
        // `created_by_run` is this correlation id rather than the conversational
        // run — the same gap D-M3a-3 recorded for the coding worker, tracked as
        // **D-M6-2**. It is provenance, not authority.
        let artifact_id = fresh_id::<ArtifactId>();
        let run_id = fresh_id::<RunId>();

        let outcome = self
            .builder
            .build_app_artifact(artifact_id, run_id, &spec, &cancel)
            .await
            .map_err(build_error)?;

        // The content the model sees names the artifact and nothing else — no
        // bundle bytes are ever folded into a prompt (invariant 1: the app is
        // data the *user* opens, not context the model reads back).
        Ok(ToolResult {
            content: serde_json::json!({
                "artifactId": outcome.artifact_id.to_string(),
                "version": outcome.version,
                "title": spec.title(),
                "template": spec.template().as_str(),
                "capabilities": spec
                    .capabilities()
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>(),
                "bytes": outcome.bytes,
            })
            .to_string(),
            truncated: false,
            // Nothing to undo: a build mutates nothing. The artifact is
            // immutable and inert until a human opens it.
            compensation: None,
        })
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        // Pre-grant validation (CF-9): the same total validation `execute` runs,
        // so an approved-but-edited spec is caught before anything binds.
        let document = Self::spec_document(arguments)?;
        parse_and_validate(document)
            .map(|_| ())
            .map_err(|e| ToolError::SchemaInvalid(sanitize_diag(&e.to_string())))
    }
}

/// Map a build failure onto the tool error taxonomy. Nothing worker-authored
/// crosses over beyond the adapter's already-sanitized diagnostic.
fn build_error(error: AppBuildError) -> ToolError {
    match error {
        // The adapter's own bound, reported as the taxonomy's timeout so the
        // orchestrator treats a wedged builder like any other wedged tool.
        AppBuildError::Timeout => ToolError::Timeout(APP_BUILD_TIMEOUT),
        AppBuildError::Cancelled => ToolError::Cancelled,
        other => ToolError::ExecutionFailed(sanitize_diag(&other.to_string())),
    }
}

/// Mirrors the adapter's round-trip bound for the timeout the taxonomy reports.
const APP_BUILD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

fn sanitize_diag(raw: &str) -> String {
    sanitize_result_content(raw, MAX_DIAGNOSTIC_BYTES).text
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_adapters::app_builder::{AppBuildRequest, AppBuildResponse, AppBuilderTransport};
    use jarvis_application::ports::{
        ArtifactStore, BlobRead, BlobStore, BlobStoreError, RepositoryError,
    };
    use jarvis_domain::artifact::{
        ArtifactManifest, ArtifactVersion, BuildNetwork, BuildProvenance,
    };
    use jarvis_domain::audit::AuditEvent;
    use jarvis_domain::grants::Sha256;
    use jarvis_domain::policy::{DataEgress, RiskLevel};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    const SPEC: &str = r#"{
        "template": "dashboard/v1",
        "title": "Kitchen",
        "capabilities": ["home.read_state"],
        "bindings": [
            {"name": "kitchen_temp", "capability": "home.read_state",
             "target": "sensor.kitchen_temperature"}
        ]
    }"#;

    struct FakeWorker;
    #[async_trait]
    impl AppBuilderTransport for FakeWorker {
        async fn run(
            &self,
            _request: &AppBuildRequest,
            _cancel: &CancellationToken,
        ) -> Result<AppBuildResponse, AppBuildError> {
            Ok(AppBuildResponse {
                ok: true,
                bundle: Some("<!doctype html><html><body>kitchen</body></html>".to_owned()),
                summary: Some("built".to_owned()),
                error: None,
            })
        }
    }

    #[derive(Default)]
    struct Blobs(Mutex<BTreeMap<[u8; 32], Vec<u8>>>);
    #[async_trait]
    impl BlobStore for Blobs {
        async fn put(&self, bytes: &[u8]) -> Result<Sha256, BlobStoreError> {
            let mut key = [0u8; 32];
            for (i, b) in bytes.iter().take(31).enumerate() {
                key[i] = *b;
            }
            key[31] = bytes.len() as u8;
            self.0.lock().unwrap().insert(key, bytes.to_vec());
            Ok(Sha256::from_bytes(key))
        }
        async fn get(&self, hash: &Sha256) -> Result<Option<Vec<u8>>, BlobStoreError> {
            Ok(self.0.lock().unwrap().get(hash.as_bytes()).cloned())
        }
        async fn contains(&self, hash: &Sha256) -> Result<bool, BlobStoreError> {
            Ok(self.0.lock().unwrap().contains_key(hash.as_bytes()))
        }
        async fn open(&self, hash: &Sha256, _max: u64) -> Result<Option<BlobRead>, BlobStoreError> {
            Ok(self.get(hash).await?.map(BlobRead::from_bytes))
        }
    }

    #[derive(Default)]
    struct Artifacts(Mutex<Vec<ArtifactManifest>>);
    #[async_trait]
    impl ArtifactStore for Artifacts {
        async fn create_version(
            &self,
            manifest: &ArtifactManifest,
            _audit: &AuditEvent,
        ) -> Result<(), RepositoryError> {
            self.0.lock().unwrap().push(manifest.clone());
            Ok(())
        }
        async fn get(
            &self,
            _id: &ArtifactId,
            _v: ArtifactVersion,
        ) -> Result<Option<ArtifactManifest>, RepositoryError> {
            Ok(None)
        }
        async fn latest(
            &self,
            _id: &ArtifactId,
        ) -> Result<Option<ArtifactManifest>, RepositoryError> {
            Ok(self.0.lock().unwrap().last().cloned())
        }
        async fn list_versions(
            &self,
            _id: &ArtifactId,
        ) -> Result<Vec<ArtifactManifest>, RepositoryError> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    fn tool(artifacts: Arc<Artifacts>, blobs: Arc<Blobs>) -> AppGenerateTool {
        AppGenerateTool {
            builder: Arc::new(AppBuilderHost::new(
                Arc::new(FakeWorker),
                blobs,
                artifacts,
                BuildProvenance {
                    worker_image: Some("jarvis-app-builder@sha256:test".to_owned()),
                    lockfile_hash: Some(Sha256::from_bytes([9; 32])),
                    network: BuildNetwork::Disabled,
                },
                "system:app-builder",
            )),
        }
    }

    fn args(spec: &str) -> CanonicalValue {
        CanonicalValue::obj([("spec", CanonicalValue::str(spec))])
    }

    fn invocation(spec: &str) -> ToolInvocation {
        ToolInvocation {
            tool_id: AppGenerateTool::id(),
            tool_version: ToolVersion::new(1, 0, 0),
            arguments: args(spec),
        }
    }

    /// **Exit evidence #1, at the tool boundary.** A model-shaped spec produces
    /// a `Bundle` artifact, and the result the model reads back names the
    /// artifact — never the bundle's bytes.
    #[tokio::test]
    async fn a_valid_spec_produces_a_bundle_artifact_and_names_it() {
        let artifacts = Arc::new(Artifacts::default());
        let blobs = Arc::new(Blobs::default());
        let result = tool(artifacts.clone(), blobs)
            .execute(invocation(SPEC), None, CancellationToken::new())
            .await
            .expect("builds");

        let value: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["template"], "dashboard/v1");
        assert_eq!(value["title"], "Kitchen");
        assert_eq!(value["capabilities"][0], "home.read_state");
        assert!(
            !result.content.contains("<!doctype"),
            "bundle bytes must never be folded back into the model's context"
        );
        assert!(result.compensation.is_none(), "a build has nothing to undo");

        let manifests = artifacts.0.lock().unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(
            manifests[0].kind(),
            jarvis_domain::artifact::ArtifactKind::Bundle
        );
        assert_eq!(
            manifests[0].capabilities(),
            &[jarvis_domain::artifact::Capability::HomeReadState]
        );
        assert_eq!(manifests[0].build().network, BuildNetwork::Disabled);
    }

    /// An invalid spec fails in the domain, before a worker is driven — and
    /// `validate_args` catches it *before* a grant could bind (CF-9).
    #[tokio::test]
    async fn an_invalid_spec_is_refused_by_validation_not_by_the_builder() {
        let artifacts = Arc::new(Artifacts::default());
        let t = tool(artifacts.clone(), Arc::new(Blobs::default()));
        let bad = r#"{"template":"evil/v1","title":"x","capabilities":[],"bindings":[]}"#;

        assert!(matches!(
            t.validate_args(&args(bad)),
            Err(ToolError::SchemaInvalid(_))
        ));
        assert!(matches!(
            t.execute(invocation(bad), None, CancellationToken::new())
                .await,
            Err(ToolError::SchemaInvalid(_))
        ));
        assert!(
            artifacts.0.lock().unwrap().is_empty(),
            "no artifact for a spec the domain rejected"
        );
    }

    /// An undeclared *capability* is a spec-validation failure too — the model
    /// cannot invent authority by naming it (ADR-029 §3).
    #[tokio::test]
    async fn a_spec_naming_an_unknown_capability_never_reaches_the_builder() {
        let artifacts = Arc::new(Artifacts::default());
        let t = tool(artifacts.clone(), Arc::new(Blobs::default()));
        let bad = r#"{"template":"dashboard/v1","title":"x",
                      "capabilities":["shell.exec"],"bindings":[]}"#;
        assert!(matches!(
            t.execute(invocation(bad), None, CancellationToken::new())
                .await,
            Err(ToolError::SchemaInvalid(_))
        ));
        assert!(artifacts.0.lock().unwrap().is_empty());
    }

    #[test]
    fn arguments_must_be_exactly_a_spec_document() {
        let artifacts = Arc::new(Artifacts::default());
        let t = tool(artifacts, Arc::new(Blobs::default()));
        assert!(
            t.validate_args(&CanonicalValue::str("not an object"))
                .is_err()
        );
        assert!(
            t.validate_args(&CanonicalValue::obj([(
                "template",
                CanonicalValue::str("x")
            )]))
            .is_err()
        );
    }

    /// The registered tier is the adapter's, not a restatement: R1 local data
    /// output. Building an app is not authorizing what the app may later ask
    /// for — that is the bridge's question, re-answered per operation (F6.5).
    #[test]
    fn the_registered_policy_is_the_adapters_own() {
        let policy = AppGenerateTool::policy();
        assert_eq!(policy.risk, RiskLevel::R1);
        assert_eq!(policy.egress, DataEgress::Local);
    }
}
