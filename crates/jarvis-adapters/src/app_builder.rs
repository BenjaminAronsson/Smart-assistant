//! Sandboxed **app builder** host (F6.2, FR-18, docs/06 §6, ADR-027, ADR-029).
//!
//! The third instance of the out-of-process worker pattern F3a.5 (browser) and
//! F3a.6 (coding) established — deliberately not a new execution primitive. The
//! worker (`tools/app-builder`) renders a **validated** [`AppSpec`] against a
//! locked Vite template with a committed lockfile and the network disabled, and
//! returns **one self-contained document**. This host turns that document into an
//! immutable [`ArtifactKind::Bundle`] artifact with **real** [`BuildProvenance`]
//! — the first producer to populate those fields with anything but
//! [`BuildProvenance::none`].
//!
//! Why one document rather than a directory or an archive: no extraction means no
//! path traversal, no zip-slip, no decompression bomb, and no multi-file origin
//! surface for F6.4's sandbox to police. The CAS stores one blob per version, and
//! that is exactly what a bundle now is.
//!
//! Security discipline (docs/06 §5/§6) — the worker and its output are untrusted
//! (Z4):
//! * **Building is not authorizing** (invariant 1). The host owns the
//!   [`ToolPolicy`] ([`app_build_policy`]); a produced bundle can do nothing on
//!   its own. Whether the app may *act* is answered later and separately by the
//!   F6.5 bridge, through `policy::evaluate` and a real `ExecutionGrant`. The
//!   capabilities recorded in the manifest come from the host-validated spec,
//!   never from anything the worker says.
//! * **Provenance is host/ops-attested, never worker-reported.** A worker that
//!   could declare its own `network: disabled` would launder the exact fact
//!   docs/06 §6 exists to record. [`AppBuildResponse`] has no provenance field to
//!   lie in, and a build whose provenance cannot be recorded produces no artifact.
//! * Every diagnostic is sanitized and length-capped before it reaches a log,
//!   span, audit reason or model prompt (invariant 5). The builder holds no
//!   credential.
//! * The round trip is bounded and cancellable, and the transport poisons itself
//!   on any interrupted exchange (invariant 4).
//! * Artifact and `artifact.created` audit are written in one transaction
//!   (invariant 6).
//!
//! Threat note: `docs/features/F6.2-threat-note.md`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use jarvis_application::ports::{ArtifactStore, BlobStore, RepositoryError};
use jarvis_domain::appspec::{AppSpec, MAX_BUILD_SECONDS, MAX_BUNDLE_BYTES};
use jarvis_domain::artifact::{
    ArtifactContent, ArtifactKind, ArtifactManifest, ArtifactSource, BuildNetwork, BuildProvenance,
    MediaType,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::grants::Sha256;
use jarvis_domain::ids::{ArtifactId, RunId};
use jarvis_domain::location::Sensitivity;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::sanitize_result_content;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};
use tokio_util::sync::CancellationToken;

/// The media type a bundle artifact is stored under: one self-contained HTML
/// document. It is served as an attachment by the ordinary blob route (M3a's
/// anti-execution guard) and *rendered* only through F6.4's separate sandboxed
/// origin — never inline in the control UI's origin.
pub const BUNDLE_MEDIA_TYPE: &str = "text/html";

/// Cap on the human-readable summary folded into logs and audit payloads
/// (invariant 5).
const MAX_SUMMARY_BYTES: usize = 2 * 1024;

/// Cap on one line of worker stdout — an untrusted worker must not be able to OOM
/// the host with a newline-less line (docs/06 §5). Sized above the bundle ceiling
/// because the document travels as one JSON line.
const MAX_WORKER_LINE_BYTES: usize = MAX_BUNDLE_BYTES as usize + 64 * 1024;

/// Wall-clock bound on one build round trip, owned by the **host**. Deliberately
/// larger than the domain's per-build ceiling ([`MAX_BUILD_SECONDS`]) so the
/// worker's own timer is the thing that normally fires and reports a typed
/// failure; this one is the backstop for a worker that has stopped answering at
/// all (invariant 4).
const APP_BUILD_TIMEOUT: Duration = Duration::from_secs(MAX_BUILD_SECONDS as u64 + 60);

/// One build task, host → worker. Only the host constructs this, and it is built
/// from an [`AppSpec`] — a type that cannot exist without having passed domain
/// validation, so the worker cannot be handed an unvalidated template id,
/// capability, name or limit.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AppBuildRequest {
    pub build_id: u64,
    /// The host template id (`TemplateId::as_str`) selecting the locked source
    /// tree and lockfile. Closed vocabulary (ADR-029).
    pub template: String,
    pub title: String,
    /// Declared capabilities, as their dotted domain names. Passed so the
    /// template can render what the app says it uses; they confer nothing.
    pub capabilities: Vec<String>,
    pub bindings: Vec<AppBindingWire>,
    /// Host-decided ceilings for this build. The worker enforces them too — two
    /// independent bounds, neither trusting the other.
    pub max_bundle_bytes: u64,
    pub max_build_seconds: u32,
}

/// One data binding on the wire. `target` is opaque: carried, never interpreted
/// here, and re-resolved through the backing tool's own allowlist at bridge time.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AppBindingWire {
    pub name: String,
    pub capability: String,
    pub target: String,
}

/// A worker's reply to one build. **Untrusted (Z4).** Only these fields are read;
/// serde drops any others, so the worker can declare no tool, no capability and —
/// pointedly — no provenance (invariant 1, docs/06 §6).
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct AppBuildResponse {
    pub ok: bool,
    /// The built, self-contained document.
    pub bundle: Option<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

/// Why a build could not produce a bundle artifact. Carries no worker-supplied
/// content beyond a short sanitized diagnostic (invariant 5).
#[derive(Debug, thiserror::Error)]
pub enum AppBuildError {
    #[error("failed to spawn app builder: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("app builder protocol error: {0}")]
    Protocol(String),
    #[error("app build round-trip timed out")]
    Timeout,
    #[error("app build was cancelled")]
    Cancelled,
    #[error("app builder reported a failure: {0}")]
    WorkerFailed(String),
    #[error("built bundle is {len} bytes, over the {max}-byte limit")]
    BundleTooLarge { len: u64, max: u64 },
    #[error("built bundle references an external subresource ({0})")]
    ExternalSubresource(String),
    #[error("build provenance is not recordable: {0}")]
    Provenance(&'static str),
    #[error("could not persist the bundle artifact: {0}")]
    Store(String),
}

/// The transport carrying one build exchange to the worker. A trait so the host
/// logic is testable against a fake worker with no Node, no Vite and no
/// filesystem, while production uses [`ChildAppBuilderTransport`] over stdio.
///
/// Contract: an implementation **owns the round-trip deadline and honours
/// `cancel`** (invariant 4), and must never pair a build with the wrong reply — an
/// exchange interrupted after the request is sent must fail closed rather than
/// desync.
#[async_trait]
pub trait AppBuilderTransport: Send + Sync {
    async fn run(
        &self,
        request: &AppBuildRequest,
        cancel: &CancellationToken,
    ) -> Result<AppBuildResponse, AppBuildError>;
}

/// The host-owned policy for building a generated app (docs/06 §5/§6).
///
/// **R1 data output**, local egress, no grant: a build reads a host-owned
/// template and returns bytes: nothing here mutates the host or leaves the
/// machine. That the resulting app *declares* capabilities — possibly R2 ones —
/// does not raise this tier, because declaring is not authorizing: every actual
/// operation is re-evaluated at bridge time against the live registry, and R2+
/// still mints an `ExecutionGrant` (ADR-029 §4, invariant 1). What the user sees
/// before approving a generation is `AppSpec::max_declared_risk`, a *preview*
/// (F6.6) — not this policy.
pub fn app_build_policy() -> ToolPolicy {
    ToolPolicy {
        risk: RiskLevel::R1,
        is_reversible: false,
        requires_user_presence: false,
        timeout: APP_BUILD_TIMEOUT,
        required_scopes: [Scope::new("app:build").expect("valid scope")]
            .into_iter()
            .collect(),
        egress: DataEgress::Local,
    }
}

/// What a successful build produced: the immutable bundle artifact's id and
/// version, its content address, its byte length, and a sanitized human summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleOutcome {
    pub artifact_id: ArtifactId,
    pub version: u32,
    pub sha256_hex: String,
    pub bytes: u64,
    pub summary: String,
}

/// The app builder host: drives the worker and turns its document into a bundle
/// artifact through the F3a.2 ports.
pub struct AppBuilderHost {
    transport: Arc<dyn AppBuilderTransport>,
    blobs: Arc<dyn BlobStore>,
    artifacts: Arc<dyn ArtifactStore>,
    /// Host/ops-attested provenance for the launch profile this host drives
    /// (docs/06 §6): the worker image, the lockfile the locked template resolved
    /// against, and the **true** network posture — container profile =
    /// `Disabled`, ADR-027's dev/CI process fallback = `Enabled`, because that is
    /// what is true there. Never self-reported by the untrusted worker.
    provenance: BuildProvenance,
    /// Who the builder acts as in the `artifact.created` audit — a dedicated
    /// system identity for an unattended worker (docs/06 §5).
    actor: String,
    builds: AtomicU64,
}

impl AppBuilderHost {
    pub fn new(
        transport: Arc<dyn AppBuilderTransport>,
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
            builds: AtomicU64::new(1),
        }
    }

    /// Build one app from a **validated** spec and persist it as an immutable
    /// `Bundle` artifact (v1). The host mints `artifact_id` and knows the
    /// producing `run_id`.
    pub async fn build_app_artifact(
        &self,
        artifact_id: ArtifactId,
        run_id: RunId,
        spec: &AppSpec,
        cancel: &CancellationToken,
    ) -> Result<BundleOutcome, AppBuildError> {
        // Before anything is spawned: a build whose provenance cannot be recorded
        // does not happen at all (F6.2, docs/06 §6). Checking here rather than at
        // the persist phase means a misconfigured host burns no build.
        check_provenance(&self.provenance)?;

        let build_id = self.builds.fetch_add(1, Ordering::Relaxed);
        let request = request_for(build_id, spec);

        let response = self.transport.run(&request, cancel).await?;
        if !response.ok {
            let text = sanitize_result_content(
                response.error.as_deref().unwrap_or_default(),
                MAX_SUMMARY_BYTES,
            )
            .text;
            return Err(AppBuildError::WorkerFailed(if text.is_empty() {
                "no detail".to_owned()
            } else {
                text
            }));
        }

        let bundle = response.bundle.as_deref().ok_or_else(|| {
            AppBuildError::Protocol("worker reported ok with no bundle".to_owned())
        })?;

        // The cap is the host's, and it is the *tighter* of the two: a spec may
        // ask for less than the ceiling, never more (ADR-029 §6). Refused whole,
        // never truncated — a prefix of a bundle is not a bundle.
        let cap = spec.limits().max_bundle_bytes().min(MAX_BUNDLE_BYTES);
        let len = bundle.len() as u64;
        if len > cap {
            return Err(AppBuildError::BundleTooLarge { len, max: cap });
        }
        // Defence in depth (threat note #2): a self-contained document has no
        // legitimate external subresource. The control that actually enforces
        // this at render time is F6.4's CSP; this one fails the *build*, early
        // and loudly, so a template regression cannot ship quietly.
        if let Some(reference) = external_subresource(bundle) {
            return Err(AppBuildError::ExternalSubresource(reference));
        }

        let summary = sanitize_result_content(
            response.summary.as_deref().unwrap_or("app built"),
            MAX_SUMMARY_BYTES,
        )
        .text;

        // The ports below take no token; don't mint an artifact for a run the
        // user already abandoned (invariant 4).
        if cancel.is_cancelled() {
            return Err(AppBuildError::Cancelled);
        }

        let sha256 = self
            .blobs
            .put(bundle.as_bytes())
            .await
            .map_err(|e| AppBuildError::Store(e.to_string()))?;
        let sha256_hex = sha256.to_string();

        let content = ArtifactContent {
            sha256,
            media_type: BUNDLE_MEDIA_TYPE
                .parse::<MediaType>()
                .expect("text/html is a valid media type"),
            kind: ArtifactKind::Bundle,
            sources: vec![ArtifactSource::Run(run_id.clone())],
            sensitivity: Sensitivity::Normal,
            build: self.provenance.clone(),
            // From the **validated spec**, never from the worker: this list is
            // what the F6.5 bridge will enforce against, so its provenance has to
            // be the host's own validation (invariant 1).
            capabilities: spec.capabilities().to_vec(),
        };
        let manifest = ArtifactManifest::initial(artifact_id.clone(), run_id.clone(), content);

        let audit = AuditEvent {
            occurred_at: SystemTime::now(),
            actor: self.actor.clone(),
            event_type: "artifact.created".to_owned(),
            target: format!("artifact:{artifact_id}"),
            correlation_id: Some(run_id.to_string()),
            payload_json: serde_json::json!({
                "kind": "bundle",
                "media_type": BUNDLE_MEDIA_TYPE,
                "sha256": sha256_hex,
                "bytes": len,
                "template": spec.template().as_str(),
                "capabilities": spec
                    .capabilities()
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>(),
                "build_network": match self.provenance.network {
                    BuildNetwork::Disabled => "disabled",
                    BuildNetwork::Enabled => "enabled",
                },
                "worker_image": self.provenance.worker_image,
                "lockfile_sha256": self.provenance.lockfile_hash.map(|h| h.to_string()),
                "summary": summary,
            })
            .to_string(),
        };

        self.artifacts
            .create_version(&manifest, &audit)
            .await
            .map_err(|e: RepositoryError| AppBuildError::Store(e.to_string()))?;

        Ok(BundleOutcome {
            artifact_id,
            version: 1,
            sha256_hex,
            bytes: len,
            summary,
        })
    }
}

/// Build the worker request from a validated spec. Free function so a test can
/// assert the shape the **real** producer sends, rather than a shape a fixture
/// invented — the failure mode M5 hit three times.
fn request_for(build_id: u64, spec: &AppSpec) -> AppBuildRequest {
    AppBuildRequest {
        build_id,
        template: spec.template().as_str().to_owned(),
        title: spec.title().to_owned(),
        capabilities: spec
            .capabilities()
            .iter()
            .map(|c| c.as_str().to_owned())
            .collect(),
        bindings: spec
            .bindings()
            .iter()
            .map(|b| AppBindingWire {
                name: b.name().as_str().to_owned(),
                capability: b.capability().as_str().to_owned(),
                target: b.target().as_str().to_owned(),
            })
            .collect(),
        max_bundle_bytes: spec.limits().max_bundle_bytes().min(MAX_BUNDLE_BYTES),
        max_build_seconds: spec.limits().max_build_seconds().min(MAX_BUILD_SECONDS),
    }
}

/// Hash the committed lockfile a template resolved against, for
/// [`BuildProvenance::lockfile_hash`].
///
/// The **host** reads and hashes the file: provenance the worker could report is
/// provenance the worker could forge (docs/06 §5/§6). Wiring calls this once at
/// startup — the lockfile is a committed file that does not change under a
/// running daemon.
pub async fn lockfile_hash(path: impl AsRef<std::path::Path>) -> Result<Sha256, AppBuildError> {
    let bytes = tokio::fs::read(path.as_ref())
        .await
        .map_err(AppBuildError::Spawn)?;
    let digest: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(&bytes).into();
    Ok(Sha256::from_bytes(digest))
}

/// A bundle's provenance must be *recordable* before a build starts (docs/06 §6).
///
/// Two rules, both decidable:
/// * a lockfile hash is required — it is what makes "validated template" a fact
///   about a specific dependency tree rather than a claim;
/// * `network: Disabled` requires a worker image, because a host with no launch
///   profile has nothing that could have isolated the network, and an
///   unsubstantiated `Disabled` is worse than an honest `Enabled`.
fn check_provenance(p: &BuildProvenance) -> Result<(), AppBuildError> {
    if p.lockfile_hash.is_none() {
        return Err(AppBuildError::Provenance(
            "a bundle build must record the lockfile it resolved against",
        ));
    }
    if p.network == BuildNetwork::Disabled && p.worker_image.is_none() {
        return Err(AppBuildError::Provenance(
            "network: disabled cannot be attested without a worker image",
        ));
    }
    Ok(())
}

/// CSS positions whose value follows the marker directly.
const CSS_FETCH_MARKERS: [&str; 2] = ["url(", "@import"];

/// HTML attributes a browser fetches. Matched as attribute *names* — whitespace
/// is legal on both sides of the `=`, and `<script SRC = "…">` is a fetch a
/// naive `"src="` scan misses entirely (this was a real miss, caught by the
/// table below).
const HTML_FETCH_ATTRIBUTES: [&str; 2] = ["src", "href"];

/// Off-document schemes. Scheme-relative `//host/x` is included because it
/// inherits the page's scheme and is a fetch all the same.
const OFF_DOCUMENT_SCHEMES: [&str; 3] = ["http://", "https://", "//"];

/// HTML's own whitespace set (§13.2.2) — not Unicode whitespace. A browser does
/// not treat NBSP as an attribute separator, so neither does this scanner.
const HTML_SPACE: [char; 5] = [' ', '\t', '\r', '\n', '\u{000C}'];

/// Find the first external subresource reference in a built document, if any.
///
/// Deliberately narrow: it looks only at *fetch* positions, so an SVG namespace
/// declaration (`xmlns="http://www.w3.org/2000/svg"`) — which no browser fetches
/// — does not trip it, while `<script src="https://…">` does. An over-broad
/// check is not the safe direction here: one that rejected the dashboard
/// template would simply be turned off.
///
/// Returns a clamped, control-stripped snippet safe to put in an error, a log
/// and an audit reason (invariant 5).
fn external_subresource(document: &str) -> Option<String> {
    let hay = document.to_ascii_lowercase();

    for marker in CSS_FETCH_MARKERS {
        let mut from = 0usize;
        while let Some(offset) = hay[from..].find(marker) {
            let value_at = from + offset + marker.len();
            let value = hay[value_at..].trim_start_matches(|c| HTML_SPACE.contains(&c));
            let value = value.trim_start_matches(['"', '\'']);
            if let Some(hit) = off_document(value) {
                return Some(hit);
            }
            from = value_at;
        }
    }

    for name in HTML_FETCH_ATTRIBUTES {
        let mut from = 0usize;
        while let Some(offset) = hay[from..].find(name) {
            let name_at = from + offset;
            let after = name_at + name.len();
            // Only at an attribute-name boundary: `<img src`, `xlink:href`, but
            // not the tail of some other word. A multibyte char before the name
            // is not an HTML separator, so it is not a boundary either.
            let at_boundary = name_at == 0
                || hay[..name_at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| HTML_SPACE.contains(&c) || c == '<' || c == ':');
            if at_boundary {
                let rest = hay[after..].trim_start_matches(|c| HTML_SPACE.contains(&c));
                if let Some(rest) = rest.strip_prefix('=') {
                    let value = rest.trim_start_matches(|c| HTML_SPACE.contains(&c));
                    let value = value.trim_start_matches(['"', '\'']);
                    if let Some(hit) = off_document(value) {
                        return Some(hit);
                    }
                }
            }
            from = after;
        }
    }
    None
}

/// If `value` starts with an off-document scheme, a short safe snippet of it.
fn off_document(value: &str) -> Option<String> {
    let scheme = OFF_DOCUMENT_SCHEMES
        .iter()
        .find(|s| value.starts_with(**s))?;
    let snippet: String = value.chars().take(80).collect();
    let safe = sanitize_result_content(&snippet, 96).text.trim().to_owned();
    Some(if safe.is_empty() {
        (*scheme).to_owned()
    } else {
        safe
    })
}

/// Strip control bytes from an internal diagnostic before it becomes an
/// [`AppBuildError`] (invariant 5).
fn sanitize_diag(raw: String) -> String {
    sanitize_result_content(&raw, MAX_SUMMARY_BYTES).text
}

/// Production transport: line-delimited JSON over a spawned worker's stdio, with
/// the self-poisoning discipline F3a.5/F3a.6 established — the protocol carries no
/// echoed id, so any exchange interrupted after the request is sent would desync
/// the next call (invariants 4/6).
pub struct ChildAppBuilderTransport<W, R> {
    writer: Mutex<FramedWrite<W, LinesCodec>>,
    reader: Mutex<FramedRead<R, LinesCodec>>,
    poisoned: AtomicBool,
}

enum ReadOutcome {
    Cancelled,
    Line(Option<Result<String, tokio_util::codec::LinesCodecError>>),
}

impl<W, R> ChildAppBuilderTransport<W, R>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    /// Wrap a worker's stdin (write) and stdout (read). jarvisd/ops builds the
    /// launch `Command` (container profile or ADR-027's process fallback) and
    /// hands the child's pipes here.
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
impl<W, R> AppBuilderTransport for ChildAppBuilderTransport<W, R>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    async fn run(
        &self,
        request: &AppBuildRequest,
        cancel: &CancellationToken,
    ) -> Result<AppBuildResponse, AppBuildError> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(AppBuildError::Protocol(
                "app builder transport poisoned after an interrupted exchange".to_owned(),
            ));
        }
        let line = serde_json::to_string(request)
            .map_err(|e| AppBuildError::Protocol(sanitize_diag(e.to_string())))?;

        let mut writer = self.writer.lock().await;
        let mut reader = self.reader.lock().await;

        match tokio::select! {
            biased;
            () = cancel.cancelled() => { self.poison(); return Err(AppBuildError::Cancelled); }
            send = writer.send(line) => send,
        } {
            Ok(()) => {}
            Err(e) => {
                self.poison();
                return Err(AppBuildError::Protocol(sanitize_diag(e.to_string())));
            }
        }

        let read = async {
            tokio::select! {
                biased;
                () = cancel.cancelled() => ReadOutcome::Cancelled,
                next = reader.next() => ReadOutcome::Line(next),
            }
        };
        match tokio::time::timeout(APP_BUILD_TIMEOUT, read).await {
            Err(_elapsed) => {
                self.poison();
                Err(AppBuildError::Timeout)
            }
            Ok(ReadOutcome::Cancelled) => {
                self.poison();
                Err(AppBuildError::Cancelled)
            }
            Ok(ReadOutcome::Line(Some(Ok(text)))) => {
                match serde_json::from_str::<AppBuildResponse>(&text) {
                    Ok(response) => Ok(response),
                    Err(e) => {
                        self.poison();
                        Err(AppBuildError::Protocol(sanitize_diag(e.to_string())))
                    }
                }
            }
            Ok(ReadOutcome::Line(Some(Err(e)))) => {
                self.poison();
                Err(AppBuildError::Protocol(sanitize_diag(e.to_string())))
            }
            Ok(ReadOutcome::Line(None)) => {
                self.poison();
                Err(AppBuildError::Protocol(
                    "app builder closed its stdout".to_owned(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_application::ports::{BlobRead, BlobStoreError};
    use jarvis_domain::appspec::{AppLimitsDraft, AppSpecDraft, DataBindingDraft};
    use jarvis_domain::artifact::Capability;
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    fn a_run() -> RunId {
        "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap()
    }
    fn an_artifact() -> ArtifactId {
        "01ARZ3NDEKTSV4RRFFQ69G5FB0".parse().unwrap()
    }

    /// A spec built the way the **real** producer builds one: through
    /// `AppSpec::validate`, never by poking private fields. Fixture-vs-caller
    /// (the bug class M5 hit three times): a fixture that constructs its input
    /// its own way can hide a total mismatch with the real caller.
    fn a_spec() -> AppSpec {
        let draft = AppSpecDraft {
            template: "dashboard/v1".to_owned(),
            title: "Kitchen".to_owned(),
            capabilities: vec!["home.read_state".to_owned()],
            bindings: vec![DataBindingDraft {
                name: "kitchen_temp".to_owned(),
                capability: "home.read_state".to_owned(),
                target: "sensor.kitchen_temperature".to_owned(),
            }],
            limits: None,
        };
        AppSpec::validate(draft, 256).expect("a valid spec")
    }

    /// Provenance a real container profile would attest.
    fn attested() -> BuildProvenance {
        BuildProvenance {
            worker_image: Some("jarvis-app-builder@sha256:beef".to_owned()),
            lockfile_hash: Some(Sha256::from_bytes([0x11; 32])),
            network: BuildNetwork::Disabled,
        }
    }

    struct FakeWorker {
        response: AppBuildResponse,
    }
    #[async_trait]
    impl AppBuilderTransport for FakeWorker {
        async fn run(
            &self,
            _request: &AppBuildRequest,
            _cancel: &CancellationToken,
        ) -> Result<AppBuildResponse, AppBuildError> {
            Ok(self.response.clone())
        }
    }

    /// Captures the request the host actually sent.
    #[derive(Default)]
    struct RecordingWorker {
        seen: StdMutex<Vec<AppBuildRequest>>,
    }
    #[async_trait]
    impl AppBuilderTransport for RecordingWorker {
        async fn run(
            &self,
            request: &AppBuildRequest,
            _cancel: &CancellationToken,
        ) -> Result<AppBuildResponse, AppBuildError> {
            self.seen.lock().unwrap().push(request.clone());
            Ok(ok_response("<!doctype html><html><body>hi</body></html>"))
        }
    }

    #[derive(Default)]
    struct FakeBlobs {
        stored: StdMutex<BTreeMap<[u8; 32], Vec<u8>>>,
    }
    #[async_trait]
    impl BlobStore for FakeBlobs {
        async fn put(&self, bytes: &[u8]) -> Result<Sha256, BlobStoreError> {
            let mut key = [0u8; 32];
            for (i, b) in bytes.iter().take(31).enumerate() {
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
        async fn open(
            &self,
            hash: &Sha256,
            max_bytes: u64,
        ) -> Result<Option<BlobRead>, BlobStoreError> {
            match self.get(hash).await? {
                Some(bytes) if bytes.len() as u64 > max_bytes => Err(BlobStoreError::TooLarge {
                    len: bytes.len() as u64,
                    max: max_bytes,
                }),
                Some(bytes) => Ok(Some(BlobRead::from_bytes(bytes))),
                None => Ok(None),
            }
        }
    }

    #[derive(Default)]
    struct FakeArtifacts {
        manifests: StdMutex<Vec<ArtifactManifest>>,
        audits: StdMutex<Vec<AuditEvent>>,
    }
    #[async_trait]
    impl ArtifactStore for FakeArtifacts {
        async fn create_version(
            &self,
            manifest: &ArtifactManifest,
            audit: &AuditEvent,
        ) -> Result<(), RepositoryError> {
            // Mirror the real store: the payload is parsed as JSON before it is
            // stored (jarvis-infra audit::append), so a malformed payload fails
            // here too rather than only in production.
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

    fn ok_response(bundle: &str) -> AppBuildResponse {
        AppBuildResponse {
            ok: true,
            bundle: Some(bundle.to_owned()),
            summary: Some("built".to_owned()),
            error: None,
        }
    }

    fn host_with(
        transport: Arc<dyn AppBuilderTransport>,
        provenance: BuildProvenance,
        artifacts: Arc<FakeArtifacts>,
        blobs: Arc<FakeBlobs>,
    ) -> AppBuilderHost {
        AppBuilderHost::new(
            transport,
            blobs,
            artifacts,
            provenance,
            "system:app-builder",
        )
    }

    fn host(response: AppBuildResponse) -> (AppBuilderHost, Arc<FakeArtifacts>, Arc<FakeBlobs>) {
        let artifacts = Arc::new(FakeArtifacts::default());
        let blobs = Arc::new(FakeBlobs::default());
        let h = host_with(
            Arc::new(FakeWorker { response }),
            attested(),
            artifacts.clone(),
            blobs.clone(),
        );
        (h, artifacts, blobs)
    }

    // --- the happy path is also the provenance path -------------------------

    /// A build stores the document as a `Bundle` artifact carrying the **real**
    /// build provenance — the first producer in the system to do so — and the
    /// capabilities from the validated spec, plus an atomic `artifact.created`
    /// audit (invariant 6).
    #[tokio::test]
    async fn a_built_app_becomes_a_bundle_artifact_with_real_provenance() {
        let document = "<!doctype html><html><body>kitchen</body></html>";
        let (host, artifacts, blobs) = host(ok_response(document));

        let outcome = host
            .build_app_artifact(an_artifact(), a_run(), &a_spec(), &CancellationToken::new())
            .await
            .expect("the build succeeds");

        assert_eq!(outcome.version, 1);
        assert_eq!(outcome.bytes, document.len() as u64);

        let manifest = artifacts
            .manifests
            .lock()
            .unwrap()
            .first()
            .cloned()
            .expect("one manifest");
        assert_eq!(manifest.kind(), ArtifactKind::Bundle);
        assert_eq!(manifest.renderer_id(), "sandboxed-webapp/v1");
        assert_eq!(manifest.media_type().as_str(), "text/html");
        assert_eq!(
            manifest.build(),
            &attested(),
            "provenance is the host's attestation, recorded verbatim"
        );
        assert_eq!(
            manifest.capabilities(),
            &[Capability::HomeReadState],
            "capabilities come from the validated spec, never from the worker"
        );

        // The bytes actually landed in the CAS at the address the outcome names.
        let stored = blobs
            .get(manifest.sha256())
            .await
            .unwrap()
            .expect("blob stored");
        assert_eq!(stored, document.as_bytes());

        let audits = artifacts.audits.lock().unwrap();
        let audit = audits.first().expect("one audit event");
        assert_eq!(audit.event_type, "artifact.created");
        let payload: serde_json::Value = serde_json::from_str(&audit.payload_json).unwrap();
        assert_eq!(payload["kind"], "bundle");
        assert_eq!(payload["template"], "dashboard/v1");
        assert_eq!(payload["build_network"], "disabled");
        assert_eq!(payload["worker_image"], "jarvis-app-builder@sha256:beef");
        assert_eq!(payload["capabilities"][0], "home.read_state");
    }

    /// Fixture-vs-caller: the request the host really sends is derived from the
    /// validated `AppSpec` — closed template id, dotted capability names, the
    /// host's own ceilings — so a worker can never be handed a value the domain
    /// rejected.
    #[tokio::test]
    async fn the_request_is_built_from_the_validated_spec_not_from_free_text() {
        let worker = Arc::new(RecordingWorker::default());
        let host = host_with(
            worker.clone(),
            attested(),
            Arc::new(FakeArtifacts::default()),
            Arc::new(FakeBlobs::default()),
        );

        host.build_app_artifact(an_artifact(), a_run(), &a_spec(), &CancellationToken::new())
            .await
            .expect("builds");

        let seen = worker.seen.lock().unwrap();
        let request = seen.first().expect("one request");
        assert_eq!(request.template, "dashboard/v1");
        assert_eq!(request.title, "Kitchen");
        assert_eq!(request.capabilities, vec!["home.read_state".to_owned()]);
        assert_eq!(request.bindings[0].name, "kitchen_temp");
        assert_eq!(request.bindings[0].target, "sensor.kitchen_temperature");
        assert_eq!(request.max_bundle_bytes, MAX_BUNDLE_BYTES);
        assert_eq!(request.max_build_seconds, MAX_BUILD_SECONDS);
    }

    /// A spec may tighten its own limits; the host forwards the *tighter* value
    /// and enforces it itself rather than trusting the worker to.
    #[tokio::test]
    async fn a_tightened_spec_limit_is_the_one_enforced() {
        let draft = AppSpecDraft {
            template: "dashboard/v1".to_owned(),
            title: "Tiny".to_owned(),
            capabilities: vec![],
            bindings: vec![],
            limits: Some(AppLimitsDraft {
                max_bundle_bytes: Some(64),
                max_build_seconds: Some(10),
            }),
        };
        let spec = AppSpec::validate(draft, 128).expect("valid");

        let oversized = "x".repeat(65);
        let (host, artifacts, _blobs) = host(ok_response(&oversized));
        let err = host
            .build_app_artifact(an_artifact(), a_run(), &spec, &CancellationToken::new())
            .await
            .expect_err("over the spec's own cap");
        assert!(
            matches!(err, AppBuildError::BundleTooLarge { len: 65, max: 64 }),
            "got {err:?}"
        );
        assert!(
            artifacts.manifests.lock().unwrap().is_empty(),
            "a refused bundle stores nothing"
        );
    }

    // --- provenance is a precondition, not a decoration ---------------------

    /// docs/06 §6: a build whose provenance cannot be recorded produces no
    /// artifact — and fails *before* a worker is driven, so a misconfigured host
    /// burns no build.
    #[tokio::test]
    async fn a_build_without_a_lockfile_hash_produces_no_artifact() {
        let worker = Arc::new(RecordingWorker::default());
        let artifacts = Arc::new(FakeArtifacts::default());
        let host = host_with(
            worker.clone(),
            BuildProvenance {
                worker_image: Some("img".to_owned()),
                lockfile_hash: None,
                network: BuildNetwork::Disabled,
            },
            artifacts.clone(),
            Arc::new(FakeBlobs::default()),
        );

        let err = host
            .build_app_artifact(an_artifact(), a_run(), &a_spec(), &CancellationToken::new())
            .await
            .expect_err("unrecordable provenance");
        assert!(matches!(err, AppBuildError::Provenance(_)), "got {err:?}");
        assert!(
            worker.seen.lock().unwrap().is_empty(),
            "no worker is driven when the provenance is unrecordable"
        );
        assert!(artifacts.manifests.lock().unwrap().is_empty());
    }

    /// An unsubstantiated `network: disabled` is worse than an honest `enabled`:
    /// a host with no launch profile has nothing that could have isolated the
    /// network, so it may not attest that it did (threat note #1).
    #[tokio::test]
    async fn network_disabled_cannot_be_attested_without_a_worker_image() {
        let host = host_with(
            Arc::new(RecordingWorker::default()),
            BuildProvenance {
                worker_image: None,
                lockfile_hash: Some(Sha256::from_bytes([7; 32])),
                network: BuildNetwork::Disabled,
            },
            Arc::new(FakeArtifacts::default()),
            Arc::new(FakeBlobs::default()),
        );
        let err = host
            .build_app_artifact(an_artifact(), a_run(), &a_spec(), &CancellationToken::new())
            .await
            .expect_err("cannot attest isolation it did not have");
        assert!(matches!(err, AppBuildError::Provenance(_)), "got {err:?}");
    }

    /// ADR-027's dev/CI process fallback is honest rather than blocked: no image,
    /// network `Enabled`, and a build that succeeds while recording exactly that
    /// (D-M6-1).
    #[tokio::test]
    async fn the_process_fallback_records_an_honest_enabled_network() {
        let artifacts = Arc::new(FakeArtifacts::default());
        let host = host_with(
            Arc::new(FakeWorker {
                response: ok_response("<!doctype html><html></html>"),
            }),
            BuildProvenance {
                worker_image: None,
                lockfile_hash: Some(Sha256::from_bytes([3; 32])),
                network: BuildNetwork::Enabled,
            },
            artifacts.clone(),
            Arc::new(FakeBlobs::default()),
        );

        host.build_app_artifact(an_artifact(), a_run(), &a_spec(), &CancellationToken::new())
            .await
            .expect("the fallback builds");

        let manifests = artifacts.manifests.lock().unwrap();
        assert_eq!(manifests[0].build().network, BuildNetwork::Enabled);
        let audits = artifacts.audits.lock().unwrap();
        let payload: serde_json::Value = serde_json::from_str(&audits[0].payload_json).unwrap();
        assert_eq!(payload["build_network"], "enabled");
    }

    // --- the worker is untrusted --------------------------------------------

    /// A worker cannot smuggle provenance, capabilities or any other authority
    /// into the manifest: serde drops every field the host does not read
    /// (invariant 1).
    #[test]
    fn a_worker_reply_cannot_declare_provenance_or_capabilities() {
        let reply: AppBuildResponse = serde_json::from_str(
            r#"{"ok":true,"bundle":"<html></html>",
                "build":{"network":"disabled","worker_image":"trust-me"},
                "capabilities":["home.set_light"],"grant":"yes"}"#,
        )
        .expect("unknown fields are dropped, not errors");
        assert!(reply.ok);
        assert_eq!(reply.bundle.as_deref(), Some("<html></html>"));
        // Nothing else survived: the struct has nowhere to put it.
        assert_eq!(reply.summary, None);
        assert_eq!(reply.error, None);
    }

    /// A failing worker's diagnostic is sanitized and capped before it can reach
    /// a log, a span or a model prompt (invariant 5).
    #[tokio::test]
    async fn a_worker_failure_is_reported_sanitized_and_never_stored() {
        let (host, artifacts, _blobs) = host(AppBuildResponse {
            ok: false,
            bundle: None,
            summary: None,
            error: Some("boom\u{202E}gnip\u{0007}".to_owned()),
        });
        let err = host
            .build_app_artifact(an_artifact(), a_run(), &a_spec(), &CancellationToken::new())
            .await
            .expect_err("worker failed");
        match err {
            AppBuildError::WorkerFailed(text) => {
                assert!(
                    !text.contains('\u{202E}'),
                    "bidi override survived: {text:?}"
                );
                assert!(
                    !text.contains('\u{0007}'),
                    "control byte survived: {text:?}"
                );
            }
            other => panic!("expected WorkerFailed, got {other:?}"),
        }
        assert!(artifacts.manifests.lock().unwrap().is_empty());
    }

    /// `ok: true` with no bundle is a protocol error, not an empty artifact.
    #[tokio::test]
    async fn ok_with_no_bundle_is_a_protocol_error() {
        let (host, artifacts, _blobs) = host(AppBuildResponse {
            ok: true,
            bundle: None,
            summary: None,
            error: None,
        });
        let err = host
            .build_app_artifact(an_artifact(), a_run(), &a_spec(), &CancellationToken::new())
            .await
            .expect_err("no bundle");
        assert!(matches!(err, AppBuildError::Protocol(_)), "got {err:?}");
        assert!(artifacts.manifests.lock().unwrap().is_empty());
    }

    /// Cancellation between the worker's reply and the persist phase mints
    /// nothing (invariant 4).
    #[tokio::test]
    async fn a_cancelled_build_mints_no_artifact() {
        let (host, artifacts, _blobs) = host(ok_response("<html></html>"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = host
            .build_app_artifact(an_artifact(), a_run(), &a_spec(), &cancel)
            .await
            .expect_err("cancelled");
        assert!(matches!(err, AppBuildError::Cancelled), "got {err:?}");
        assert!(artifacts.manifests.lock().unwrap().is_empty());
    }

    // --- the self-contained rule --------------------------------------------

    /// A document that would make the browser fetch something off-origin fails
    /// the build (threat note #2). The enforcement that matters is F6.4's CSP;
    /// this catches a template regression at build time.
    #[tokio::test]
    async fn an_external_subresource_fails_the_build() {
        for document in [
            r#"<html><script src="https://evil.example/x.js"></script></html>"#,
            r#"<html><link rel="stylesheet" href="http://evil.example/x.css"></html>"#,
            r#"<html><script SRC = "//evil.example/x.js"></script></html>"#,
            r#"<html><style>@import "https://evil.example/x.css";</style></html>"#,
            r#"<html><style>body{background:url(https://evil.example/x.png)}</style></html>"#,
        ] {
            let (host, artifacts, _blobs) = host(ok_response(document));
            let err = host
                .build_app_artifact(an_artifact(), a_run(), &a_spec(), &CancellationToken::new())
                .await
                .expect_err("an external subresource must fail the build");
            assert!(
                matches!(err, AppBuildError::ExternalSubresource(_)),
                "{document} → {err:?}"
            );
            assert!(
                artifacts.manifests.lock().unwrap().is_empty(),
                "{document} must not have produced an artifact"
            );
        }
    }

    /// …and the check is narrow enough not to reject legitimate output: an SVG
    /// namespace declaration is a `http://` URL no browser ever fetches. A check
    /// that broke the dashboard template would be turned off, which is the real
    /// failure mode of over-broad static checks.
    #[tokio::test]
    async fn an_svg_namespace_url_is_not_an_external_subresource() {
        let document = r#"<html><body><svg xmlns="http://www.w3.org/2000/svg"><circle r="1"/></svg>
            <!-- see https://example.invalid/docs --></body></html>"#;
        let (host, artifacts, _blobs) = host(ok_response(document));
        host.build_app_artifact(an_artifact(), a_run(), &a_spec(), &CancellationToken::new())
            .await
            .expect("an xmlns URL is not a subresource");
        assert_eq!(artifacts.manifests.lock().unwrap().len(), 1);
    }

    /// **Fixture-vs-caller** (the bug class M5 hit three times): the document a
    /// *real* build of the *real* locked template produces must be ACCEPTED. A
    /// static check tuned only against hand-written hostile strings is the kind
    /// that turns out to reject every legitimate build — and then gets deleted.
    ///
    /// The fixture is verbatim output of
    /// `node tools/app-builder/src/index.mjs` on `dashboard/v1`; regenerate it
    /// the same way if the template changes. Note what it contains: minified
    /// Vite preload code in which the bare word `href` appears — a naive
    /// substring scan would have failed here, and did.
    #[tokio::test]
    async fn a_document_from_a_real_build_of_the_locked_template_is_accepted() {
        let document = include_str!("../tests/fixtures/dashboard-v1-built.html");
        let (host, artifacts, _blobs) = host(ok_response(document));

        host.build_app_artifact(an_artifact(), a_run(), &a_spec(), &CancellationToken::new())
            .await
            .expect("a real build of the locked template must pass the host's checks");

        let manifests = artifacts.manifests.lock().unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].kind(), ArtifactKind::Bundle);
    }

    /// Relative subresources — what a self-contained document legitimately has
    /// none of, but a template may still emit as `data:` — are fine.
    #[test]
    fn inline_and_data_subresources_pass() {
        assert_eq!(
            external_subresource(r#"<img src="data:image/png;base64,AAAA">"#),
            None
        );
        assert_eq!(external_subresource(r##"<a href="#section">x</a>"##), None);
        assert_eq!(external_subresource("<style>body{color:red}</style>"), None);
    }

    // --- policy --------------------------------------------------------------

    /// The build's tier is host-owned and R1: producing bytes is data output.
    /// The declared capabilities do **not** raise it, because declaring is not
    /// authorizing — the bridge re-evaluates every operation (ADR-029 §4).
    #[test]
    fn building_is_r1_data_output_with_no_egress() {
        let policy = app_build_policy();
        assert_eq!(policy.risk, RiskLevel::R1);
        assert_eq!(policy.egress, DataEgress::Local);
        assert!(!policy.requires_user_presence);
        assert!(policy.timeout >= Duration::from_secs(MAX_BUILD_SECONDS as u64));
    }
}
