//! Sandboxed coding worker host (F3a.6, docs/02 §8, docs/06 §5, golden 7,
//! invariant #1).
//!
//! Jarvis delegates a coding task to an out-of-process **coding worker**
//! (`tools/coding-worker`) that runs in a **disposable git worktree**, produces a
//! unified-diff **patch**, and is torn down — mirroring the browser worker's
//! isolation discipline (F3a.5) and the Claude-Code coding profile (ADR-004). The
//! host turns that patch into a **reviewable patch artifact** (F3a.1 `CodeText`
//! kind) persisted through the artifact ports (F3a.2).
//!
//! **PATCH-ONLY (owner decision, M3-features.md; golden 7 "no direct
//! deployment").** There is deliberately **no code path that applies, commits, or
//! deploys** the patch: the worker's output is *data* — a diff stored as an
//! immutable artifact for human review — never a host mutation (invariant #1).
//! Applying a patch is a separate approved action, deferred to a later milestone.
//!
//! Security discipline (docs/06 §5) — the worker and its output are untrusted
//! (Z4):
//! * The worker declares nothing; the host owns the tool's [`ToolPolicy`]
//!   ([`coding_patch_policy`]) — producing a patch is R1 *data output*, not a
//!   mutation, so it needs no grant, but it also cannot itself change the host.
//! * The patch **summary** and any worker error are sanitized
//!   ([`sanitize_result_content`]) before they reach a log, span, or the model.
//!   The raw patch bytes are stored **only** in the content-addressed blob store
//!   for review; they are never folded into a model prompt as authority.
//! * The stored artifact + its audit event are written atomically (invariant #6)
//!   — the durable `artifact.created` evidence. (The replayable WS `artifact.created`
//!   fan-out lands with orchestrator/outbox wiring — D-M3a-3, gate.)
//! * The worker round-trip is bounded and cancellable, and the transport poisons
//!   itself on any interrupted exchange (invariant #4) — same discipline as the
//!   browser worker.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use jarvis_application::ports::{ArtifactStore, BlobStore, RepositoryError};
use jarvis_domain::artifact::{
    ArtifactContent, ArtifactKind, ArtifactManifest, ArtifactSource, BuildProvenance, MediaType,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::{ArtifactId, RunId};
use jarvis_domain::location::Sensitivity;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::sanitize_result_content;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};
use tokio_util::sync::CancellationToken;

/// The media type a patch artifact is stored under (a unified diff).
const PATCH_MEDIA_TYPE: &str = "text/x-diff";

/// Hard cap on the patch a worker may return. A patch is a reviewable diff, not a
/// payload channel; an over-cap patch is rejected (fail closed) rather than stored
/// (docs/06 §5 denial-of-resources). Generous — real review patches are small.
const MAX_PATCH_BYTES: usize = 1024 * 1024;

/// Cap on the human-readable summary folded into logs/results (invariant #5).
const MAX_SUMMARY_BYTES: usize = 2 * 1024;

/// Cap on one line of worker stdout — the untrusted worker cannot OOM the host
/// with a newline-less line (docs/06 §5). Sized above [`MAX_PATCH_BYTES`] because
/// the patch travels as one JSON line.
const MAX_WORKER_LINE_BYTES: usize = MAX_PATCH_BYTES + 64 * 1024;

/// Wall-clock bound on one coding task. Generous — a delegated coding step runs a
/// model editing a worktree — but a wedged worker must not hang a run forever
/// (invariant #4).
const CODING_TIMEOUT: Duration = Duration::from_secs(600);

/// One coding task the host sends to the worker. Only the host constructs this.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CodingRequest {
    pub task_id: u64,
    /// The natural-language coding instruction (host/run-authored).
    pub instruction: String,
    /// The repository the worker copies into a disposable worktree.
    pub repo_path: String,
}

/// A worker's reply to one task. **Untrusted (Z4).** Only these fields are read;
/// serde drops any others, so the worker cannot declare a tool, an action, or a
/// deploy step (invariant #1). `patch` is a unified diff; `summary` is a short
/// description; both are sanitized/capped before use.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct CodingResponse {
    pub ok: bool,
    pub patch: Option<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

/// Why a coding task could not produce a patch artifact. Carries no worker-supplied
/// content beyond a short sanitized diagnostic (invariant #5).
#[derive(Debug, thiserror::Error)]
pub enum CodingError {
    #[error("failed to spawn coding worker: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("coding worker protocol error: {0}")]
    Protocol(String),
    #[error("coding worker round-trip timed out")]
    Timeout,
    #[error("coding task was cancelled")]
    Cancelled,
    #[error("coding worker reported a failure: {0}")]
    WorkerFailed(String),
    #[error("coding worker returned a patch larger than the cap")]
    PatchTooLarge,
    #[error("could not persist the patch artifact: {0}")]
    Store(String),
}

/// The transport that carries one task/patch exchange to the worker. A trait so
/// the producer logic is testable against a fake worker with no git/model, while
/// production uses [`ChildCodingTransport`] over the child's stdio.
///
/// Contract: an implementation **owns the round-trip deadline and honours
/// `cancel`** (invariant #4), and must never leave itself able to pair a task with
/// the wrong reply — if an exchange is interrupted after the task is sent,
/// subsequent calls must fail rather than desync.
#[async_trait]
pub trait CodingTransport: Send + Sync {
    async fn run(
        &self,
        request: &CodingRequest,
        cancel: &CancellationToken,
    ) -> Result<CodingResponse, CodingError>;
}

/// The host-owned policy for producing a patch (docs/06 §5). Producing a patch is
/// **R1 data output** — the worker reads a repo copy and returns a diff; nothing
/// here mutates the host, so no grant is required, and there is no apply path a
/// grant could authorize. Egress is `Local`: the patch is stored locally as an
/// artifact.
pub fn coding_patch_policy() -> ToolPolicy {
    ToolPolicy {
        risk: RiskLevel::R1,
        is_reversible: false,
        requires_user_presence: false,
        timeout: CODING_TIMEOUT,
        required_scopes: [Scope::new("coding:patch").expect("valid scope")]
            .into_iter()
            .collect(),
        egress: DataEgress::Local,
    }
}

/// What a successful coding task produced: the immutable patch artifact's id and
/// version, its content address, and a sanitized human summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchOutcome {
    pub artifact_id: ArtifactId,
    pub version: u32,
    pub sha256_hex: String,
    pub summary: String,
}

/// The coding worker host: drives the worker and turns its patch into a reviewable
/// artifact through the F3a.2 ports. Holds no per-task state beyond a monotonic
/// task counter (for correlation).
pub struct CodingWorkerHost {
    transport: Arc<dyn CodingTransport>,
    blobs: Arc<dyn BlobStore>,
    artifacts: Arc<dyn ArtifactStore>,
    /// Host/ops-attested build provenance for the launch profile this host drives
    /// (docs/06 §6): the worker image and the **true** network posture (container
    /// = `Disabled`; the dev/CI process fallback = `Enabled`). Attested here, never
    /// self-reported by the untrusted worker (docs/06 §5).
    provenance: BuildProvenance,
    /// Who the worker acts as in the `artifact.created` audit (a dedicated system
    /// identity for an unattended worker, docs/06 §5). Orchestrator wiring threads
    /// the run's actor here later (deferred, D-M3a-3). The audit itself is written
    /// atomically by [`ArtifactStore::create_version`] (invariant #6) — the host
    /// needs no separate audit sink.
    actor: String,
    tasks: AtomicU64,
}

impl CodingWorkerHost {
    pub fn new(
        transport: Arc<dyn CodingTransport>,
        blobs: Arc<dyn BlobStore>,
        artifacts: Arc<dyn ArtifactStore>,
        provenance: BuildProvenance,
        actor: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            blobs,
            artifacts,
            provenance,
            actor: actor.into(),
            tasks: AtomicU64::new(1),
        }
    }

    /// Delegate one coding task and persist its patch as an immutable `CodeText`
    /// artifact (v1). The host mints `artifact_id` (host owns randomness) and knows
    /// the producing `run_id`. **No apply path**: this stores the diff for review
    /// and returns — it never applies, commits, or deploys it (golden 7, invariant
    /// #1).
    pub async fn produce_patch_artifact(
        &self,
        artifact_id: ArtifactId,
        run_id: RunId,
        instruction: impl Into<String>,
        repo_path: impl Into<String>,
        cancel: &CancellationToken,
    ) -> Result<PatchOutcome, CodingError> {
        let task_id = self.tasks.fetch_add(1, Ordering::Relaxed);
        let request = CodingRequest {
            task_id,
            instruction: instruction.into(),
            repo_path: repo_path.into(),
        };

        let response = self.transport.run(&request, cancel).await?;
        if !response.ok {
            let raw = response.error.as_deref().unwrap_or_default();
            let text = sanitize_result_content(raw, MAX_SUMMARY_BYTES).text;
            return Err(CodingError::WorkerFailed(if text.is_empty() {
                "no detail".to_owned()
            } else {
                text
            }));
        }

        // The patch is the only load-bearing output. Stored raw (content-addressed,
        // reviewed via the artifact renderer) — never folded into a prompt.
        let patch = response
            .patch
            .as_deref()
            .ok_or_else(|| CodingError::Protocol("worker reported ok with no patch".to_owned()))?;
        if patch.len() > MAX_PATCH_BYTES {
            return Err(CodingError::PatchTooLarge);
        }
        let summary = sanitize_result_content(
            response.summary.as_deref().unwrap_or("patch produced"),
            MAX_SUMMARY_BYTES,
        )
        .text;

        // The ports below don't take a token, but don't mint an artifact for a run
        // the user already abandoned (invariant #4): check before the persist phase.
        if cancel.is_cancelled() {
            return Err(CodingError::Cancelled);
        }
        let sha256 = self
            .blobs
            .put(patch.as_bytes())
            .await
            .map_err(|e| CodingError::Store(e.to_string()))?;
        // The domain hash's Display is canonical lowercase hex — one source of truth.
        let sha256_hex = sha256.to_string();

        let content = ArtifactContent {
            sha256,
            media_type: PATCH_MEDIA_TYPE
                .parse::<MediaType>()
                .expect("text/x-diff is a valid media type"),
            kind: ArtifactKind::CodeText,
            sources: vec![ArtifactSource::Run(run_id.clone())],
            sensitivity: Sensitivity::Normal,
            // Build provenance is **host/ops-attested**, not worker-reported: the
            // untrusted worker must not get to declare its own isolation posture
            // (docs/06 §5/§6). The host is constructed with the true posture of the
            // launch profile it used (container = network disabled; the dev/CI
            // process fallback = whatever the host set).
            build: self.provenance.clone(),
            capabilities: Vec::new(),
        };
        let manifest = ArtifactManifest::initial(artifact_id.clone(), run_id.clone(), content);

        // artifact.created durable evidence: the manifest and this audit event are
        // written in one transaction (invariant #6). If the store cannot persist
        // it, the whole task fails — no un-audited artifact. Build the payload with
        // serde_json so an untrusted `summary` (which may legitimately carry `\n`/
        // `\t`, preserved by the sanitizer) cannot produce a malformed audit row.
        let audit = AuditEvent {
            occurred_at: SystemTime::now(),
            actor: self.actor.clone(),
            event_type: "artifact.created".to_owned(),
            target: format!("artifact:{artifact_id}"),
            correlation_id: Some(run_id.to_string()),
            payload_json: serde_json::json!({
                "kind": "code_text",
                "media_type": PATCH_MEDIA_TYPE,
                "sha256": sha256_hex,
                "summary": summary,
            })
            .to_string(),
        };
        self.artifacts
            .create_version(&manifest, &audit)
            .await
            .map_err(|e: RepositoryError| CodingError::Store(e.to_string()))?;

        Ok(PatchOutcome {
            artifact_id,
            version: 1,
            sha256_hex,
            summary,
        })
    }
}

/// Strip control bytes from an internal diagnostic before it becomes a
/// [`CodingError`] (invariant #5).
fn sanitize_diag(raw: String) -> String {
    sanitize_result_content(&raw, MAX_SUMMARY_BYTES).text
}

/// Production transport: line-delimited JSON over a spawned worker's stdio, with
/// the same self-poisoning discipline as the browser worker (F3a.5): the protocol
/// carries no echoed id, so any exchange interrupted after the task is sent would
/// desync the next call — the transport poisons itself and fails closed instead
/// (invariants #4/#6).
pub struct ChildCodingTransport<W, R> {
    writer: Mutex<FramedWrite<W, LinesCodec>>,
    reader: Mutex<FramedRead<R, LinesCodec>>,
    poisoned: AtomicBool,
}

enum ReadOutcome {
    Cancelled,
    Line(Option<Result<String, tokio_util::codec::LinesCodecError>>),
}

impl<W, R> ChildCodingTransport<W, R>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    /// Wrap a worker's stdin (write) and stdout (read). jarvisd/ops builds the
    /// launch `Command` (container / disposable worktree, credentials in env —
    /// never argv) and hands the child's pipes here.
    pub fn new(stdin: W, stdout: R) -> Self {
        Self {
            writer: Mutex::new(FramedWrite::new(stdin, LinesCodec::new())),
            reader: Mutex::new(FramedRead::new(
                stdout,
                LinesCodec::new_with_max_length(MAX_WORKER_LINE_BYTES),
            )),
            poisoned: AtomicBool::new(false),
        }
    }

    fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);
    }
}

#[async_trait]
impl<W, R> CodingTransport for ChildCodingTransport<W, R>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    async fn run(
        &self,
        request: &CodingRequest,
        cancel: &CancellationToken,
    ) -> Result<CodingResponse, CodingError> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(CodingError::Protocol(
                "coding worker transport poisoned after an interrupted exchange".to_owned(),
            ));
        }
        let line = serde_json::to_string(request)
            .map_err(|e| CodingError::Protocol(sanitize_diag(e.to_string())))?;

        let mut writer = self.writer.lock().await;
        let mut reader = self.reader.lock().await;

        match tokio::select! {
            biased;
            () = cancel.cancelled() => { self.poison(); return Err(CodingError::Cancelled); }
            send = writer.send(line) => send,
        } {
            Ok(()) => {}
            Err(e) => {
                self.poison();
                return Err(CodingError::Protocol(sanitize_diag(e.to_string())));
            }
        }

        let read = async {
            tokio::select! {
                biased;
                () = cancel.cancelled() => ReadOutcome::Cancelled,
                next = reader.next() => ReadOutcome::Line(next),
            }
        };
        match tokio::time::timeout(CODING_TIMEOUT, read).await {
            Err(_elapsed) => {
                self.poison();
                Err(CodingError::Timeout)
            }
            Ok(ReadOutcome::Cancelled) => {
                self.poison();
                Err(CodingError::Cancelled)
            }
            Ok(ReadOutcome::Line(Some(Ok(text)))) => {
                match serde_json::from_str::<CodingResponse>(&text) {
                    Ok(response) => Ok(response),
                    Err(e) => {
                        self.poison();
                        Err(CodingError::Protocol(sanitize_diag(e.to_string())))
                    }
                }
            }
            Ok(ReadOutcome::Line(Some(Err(e)))) => {
                self.poison();
                Err(CodingError::Protocol(sanitize_diag(e.to_string())))
            }
            Ok(ReadOutcome::Line(None)) => {
                self.poison();
                Err(CodingError::Protocol(
                    "coding worker closed its stdout".to_owned(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_application::ports::BlobStoreError;
    use jarvis_domain::grants::Sha256;
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    fn a_run() -> RunId {
        "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap()
    }
    fn an_artifact() -> ArtifactId {
        "01ARZ3NDEKTSV4RRFFQ69G5FB0".parse().unwrap()
    }

    struct FakeWorker {
        response: CodingResponse,
    }
    #[async_trait]
    impl CodingTransport for FakeWorker {
        async fn run(
            &self,
            _request: &CodingRequest,
            _cancel: &CancellationToken,
        ) -> Result<CodingResponse, CodingError> {
            Ok(self.response.clone())
        }
    }

    #[derive(Default)]
    struct FakeBlobs {
        stored: StdMutex<BTreeMap<[u8; 32], Vec<u8>>>,
    }
    #[async_trait]
    impl BlobStore for FakeBlobs {
        async fn put(&self, bytes: &[u8]) -> Result<Sha256, BlobStoreError> {
            // Deterministic content address for the test (not a real hash).
            let mut key = [0u8; 32];
            for (i, b) in bytes.iter().take(32).enumerate() {
                key[i] = *b;
            }
            key[31] = bytes.len() as u8;
            self.stored.lock().unwrap().insert(key, bytes.to_vec());
            Ok(Sha256::from_bytes(key))
        }
        async fn get(&self, hash: &Sha256) -> Result<Option<Vec<u8>>, BlobStoreError> {
            Ok(self.stored.lock().unwrap().get(hash.as_bytes()).cloned())
        }
        async fn contains(&self, hash: &Sha256) -> Result<bool, BlobStoreError> {
            Ok(self.stored.lock().unwrap().contains_key(hash.as_bytes()))
        }
    }

    #[derive(Default)]
    struct FakeArtifacts {
        manifests: StdMutex<Vec<ArtifactManifest>>,
        audits: StdMutex<Vec<AuditEvent>>,
        fail: bool,
    }
    #[async_trait]
    impl ArtifactStore for FakeArtifacts {
        async fn create_version(
            &self,
            manifest: &ArtifactManifest,
            audit: &AuditEvent,
        ) -> Result<(), RepositoryError> {
            if self.fail {
                return Err(RepositoryError::Storage("store down".to_owned()));
            }
            // Mirror the real store: the payload is parsed as JSON before it is
            // hashed/stored (jarvis-infra audit::append). A malformed payload must
            // fail here too, so tests exercise the real constraint (not just clone).
            serde_json::from_str::<serde_json::Value>(&audit.payload_json)
                .map_err(|e| RepositoryError::Storage(format!("bad audit payload: {e}")))?;
            self.manifests.lock().unwrap().push(manifest.clone());
            self.audits.lock().unwrap().push(audit.clone());
            Ok(())
        }
        async fn get(
            &self,
            _id: &ArtifactId,
            _version: jarvis_domain::artifact::ArtifactVersion,
        ) -> Result<Option<ArtifactManifest>, RepositoryError> {
            Ok(None)
        }
        async fn latest(
            &self,
            _id: &ArtifactId,
        ) -> Result<Option<ArtifactManifest>, RepositoryError> {
            Ok(None)
        }
        async fn list_versions(
            &self,
            _id: &ArtifactId,
        ) -> Result<Vec<ArtifactManifest>, RepositoryError> {
            Ok(self.manifests.lock().unwrap().clone())
        }
    }

    fn host(
        response: CodingResponse,
        artifacts: Arc<FakeArtifacts>,
        blobs: Arc<FakeBlobs>,
    ) -> CodingWorkerHost {
        CodingWorkerHost::new(
            Arc::new(FakeWorker { response }),
            blobs,
            artifacts,
            // Host-attested provenance for the (test) launch profile.
            BuildProvenance::none(),
            "system",
        )
    }

    fn ok_response(patch: &str) -> CodingResponse {
        CodingResponse {
            ok: true,
            patch: Some(patch.to_owned()),
            summary: Some("added a function".to_owned()),
            error: None,
        }
    }

    // ---- the happy path: a coding task creates a CodeText patch artifact ----

    #[tokio::test]
    async fn a_coding_task_creates_a_code_text_patch_artifact() {
        let artifacts = Arc::new(FakeArtifacts::default());
        let blobs = Arc::new(FakeBlobs::default());
        let patch = "--- a/x.rs\n+++ b/x.rs\n@@\n-old\n+new\n";
        let host = host(ok_response(patch), artifacts.clone(), blobs.clone());

        let outcome = host
            .produce_patch_artifact(
                an_artifact(),
                a_run(),
                "add a function",
                "/repo",
                &CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(outcome.version, 1);
        assert_eq!(outcome.summary, "added a function");

        // The manifest is a CodeText artifact whose blob holds the exact patch.
        // Clone the fields out and drop the guard before the async blob read.
        let (kind, media, run, sha) = {
            let manifests = artifacts.manifests.lock().unwrap();
            assert_eq!(manifests.len(), 1);
            let m = &manifests[0];
            (
                m.kind(),
                m.media_type().as_str().to_owned(),
                m.created_by_run().clone(),
                *m.sha256(),
            )
        };
        assert_eq!(kind, ArtifactKind::CodeText);
        assert_eq!(media, "text/x-diff");
        assert_eq!(run, a_run());
        let stored = blobs.get(&sha).await.unwrap().unwrap();
        assert_eq!(
            stored,
            patch.as_bytes(),
            "the raw patch is preserved for review"
        );

        // The artifact.created audit is written atomically with the manifest.
        let audits = artifacts.audits.lock().unwrap();
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].event_type, "artifact.created");
        assert!(audits[0].target.starts_with("artifact:"));
    }

    // ---- invariant #1: the patch is data — there is NO apply path ----

    // Golden-7 / invariant-#1 property (documented): the host's only production
    // entry point is `produce_patch_artifact`, which stores the diff as a reviewable
    // artifact and returns — there is deliberately no `apply`, `commit`, or `deploy`
    // method anywhere in this module, and `CodingResponse` has no field a worker
    // could use to request one (see `unknown_worker_fields_are_ignored`). If an
    // apply path is ever added, docs/07 §2 (7) must be revisited.

    #[tokio::test]
    async fn a_worker_failure_produces_no_artifact() {
        let artifacts = Arc::new(FakeArtifacts::default());
        let blobs = Arc::new(FakeBlobs::default());
        let response = CodingResponse {
            ok: false,
            patch: None,
            summary: None,
            error: Some("compile error\u{0007}boom".to_owned()),
        };
        let host = host(response, artifacts.clone(), blobs);
        let err = host
            .produce_patch_artifact(
                an_artifact(),
                a_run(),
                "x",
                "/repo",
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CodingError::WorkerFailed(m) if m == "compile errorboom"));
        assert!(
            artifacts.manifests.lock().unwrap().is_empty(),
            "no artifact on failure"
        );
    }

    #[tokio::test]
    async fn an_ok_response_without_a_patch_is_a_protocol_error() {
        let artifacts = Arc::new(FakeArtifacts::default());
        let response = CodingResponse {
            ok: true,
            patch: None,
            summary: Some("done".to_owned()),
            error: None,
        };
        let host = host(response, artifacts.clone(), Arc::new(FakeBlobs::default()));
        let err = host
            .produce_patch_artifact(
                an_artifact(),
                a_run(),
                "x",
                "/repo",
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CodingError::Protocol(_)));
        assert!(artifacts.manifests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_oversized_patch_is_rejected_and_not_stored() {
        let artifacts = Arc::new(FakeArtifacts::default());
        let blobs = Arc::new(FakeBlobs::default());
        let big = "x".repeat(MAX_PATCH_BYTES + 1);
        let host = host(ok_response(&big), artifacts.clone(), blobs.clone());
        let err = host
            .produce_patch_artifact(
                an_artifact(),
                a_run(),
                "x",
                "/repo",
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CodingError::PatchTooLarge));
        assert!(artifacts.manifests.lock().unwrap().is_empty());
        assert!(blobs.stored.lock().unwrap().is_empty(), "nothing stored");
    }

    #[tokio::test]
    async fn a_multiline_summary_still_yields_valid_audit_json() {
        // Regression: `sanitize_result_content` preserves `\n`/`\t`, so a hand-rolled
        // JSON encoder that only escaped `"`/`\` produced an invalid `payload_json`
        // that the real store rejects. `FakeArtifacts` now parses the payload, so
        // this fails unless the payload is proper JSON.
        let artifacts = Arc::new(FakeArtifacts::default());
        let blobs = Arc::new(FakeBlobs::default());
        let mut response = ok_response("--- a\n+++ b\n");
        response.summary = Some("line one\nline two\twith tab".to_owned());
        let host = host(response, artifacts.clone(), blobs);
        host.produce_patch_artifact(
            an_artifact(),
            a_run(),
            "x",
            "/repo",
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let audits = artifacts.audits.lock().unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&audits[0].payload_json).expect("audit payload is valid JSON");
        assert_eq!(parsed["summary"], "line one\nline two\twith tab");
    }

    #[tokio::test]
    async fn a_cancelled_task_mints_no_artifact() {
        let artifacts = Arc::new(FakeArtifacts::default());
        let blobs = Arc::new(FakeBlobs::default());
        let host = host(ok_response("--- a\n+++ b\n"), artifacts.clone(), blobs);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = host
            .produce_patch_artifact(an_artifact(), a_run(), "x", "/repo", &cancel)
            .await
            .unwrap_err();
        assert!(matches!(err, CodingError::Cancelled));
        assert!(artifacts.manifests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_summary_with_control_bytes_is_sanitized_in_the_audit() {
        let artifacts = Arc::new(FakeArtifacts::default());
        let blobs = Arc::new(FakeBlobs::default());
        let mut response = ok_response("--- a\n+++ b\n");
        response.summary = Some("did\u{0007}\u{202E}stuff".to_owned());
        let host = host(response, artifacts.clone(), blobs);
        let outcome = host
            .produce_patch_artifact(
                an_artifact(),
                a_run(),
                "x",
                "/repo",
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!outcome.summary.contains('\u{0007}'));
        assert!(!outcome.summary.contains('\u{202E}'));
    }

    #[tokio::test]
    async fn a_store_failure_surfaces_and_no_partial_artifact_is_reported() {
        let artifacts = Arc::new(FakeArtifacts {
            manifests: StdMutex::new(Vec::new()),
            audits: StdMutex::new(Vec::new()),
            fail: true,
        });
        let host = host(
            ok_response("--- a\n+++ b\n"),
            artifacts.clone(),
            Arc::new(FakeBlobs::default()),
        );
        let err = host
            .produce_patch_artifact(
                an_artifact(),
                a_run(),
                "x",
                "/repo",
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CodingError::Store(_)));
    }

    #[test]
    fn coding_patch_policy_is_r1_data_output() {
        let policy = coding_patch_policy();
        assert_eq!(policy.risk, RiskLevel::R1);
        assert_eq!(policy.egress, DataEgress::Local);
        assert!(
            !policy.risk.requires_approval(),
            "producing a patch needs no grant"
        );
    }

    // ---- transport poisoning (mirrors the browser worker) ----

    fn duplex_transport<F, Fut>(
        worker: F,
    ) -> ChildCodingTransport<tokio::io::DuplexStream, tokio::io::DuplexStream>
    where
        F: FnOnce(tokio::io::DuplexStream, tokio::io::DuplexStream) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (host_stdin, worker_stdin) = tokio::io::duplex(1024 * 1024);
        let (worker_stdout, host_stdout) = tokio::io::duplex(1024 * 1024);
        tokio::spawn(worker(worker_stdin, worker_stdout));
        ChildCodingTransport::new(host_stdin, host_stdout)
    }

    fn a_request() -> CodingRequest {
        CodingRequest {
            task_id: 1,
            instruction: "x".to_owned(),
            repo_path: "/repo".to_owned(),
        }
    }

    #[tokio::test]
    async fn child_transport_round_trips_a_patch() {
        let transport = duplex_transport(|win, wout| async move {
            let mut r = FramedRead::new(win, LinesCodec::new());
            let mut w = FramedWrite::new(wout, LinesCodec::new());
            if r.next().await.is_some() {
                w.send(r#"{"ok":true,"patch":"--- a\n+++ b\n","summary":"s"}"#.to_owned())
                    .await
                    .unwrap();
            }
        });
        let resp = transport
            .run(&a_request(), &CancellationToken::new())
            .await
            .unwrap();
        assert!(resp.ok);
        assert!(resp.patch.unwrap().contains("+++"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_task_poisons_the_transport() {
        let transport = duplex_transport(|win, wout| async move {
            let mut r = FramedRead::new(win, LinesCodec::new());
            let _ = r.next().await;
            let _keep = wout;
            futures_util::future::pending::<()>().await;
        });
        let first = transport.run(&a_request(), &CancellationToken::new()).await;
        assert!(matches!(first, Err(CodingError::Timeout)), "{first:?}");
        let second = transport.run(&a_request(), &CancellationToken::new()).await;
        assert!(
            matches!(second, Err(CodingError::Protocol(_))),
            "{second:?}"
        );
    }

    #[tokio::test]
    async fn unknown_worker_fields_are_ignored() {
        let raw = r#"{"ok":true,"patch":"d","deploy":{"target":"prod"},"apply":true}"#;
        let parsed: CodingResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.patch.as_deref(), Some("d"));
        // No `deploy`/`apply` field exists on the type — the worker cannot ask the
        // host to deploy (invariant #1).
    }
}
