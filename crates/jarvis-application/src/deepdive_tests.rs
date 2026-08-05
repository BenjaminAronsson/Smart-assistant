//! F3b.6 deep-dive turn routing, source handoff, and Research Notes promotion
//! (FR-27, ADR-017, docs/12 §2.5).
//!
//! Three properties carry the feature's risk and are asserted here rather than
//! described in a comment:
//!
//! 1. **A continuation extends the canvas; only a topic change shelves** — and
//!    a pending approval is exempt from either (FR-24, F3b.4 must not regress).
//! 2. **"Open that" produces a *proposal*, never an execution.** The handoff
//!    hands back a [`ToolProposal`] for the browser worker that `policy::evaluate`
//!    still has to authorize (invariant #1), and it carries no page content.
//! 3. **Promotion writes an audited, versioned artifact through the ports** —
//!    and a thread that cannot be audited is not persisted at all.

use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use jarvis_domain::artifact::{ArtifactManifest, ArtifactVersion};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::deepdive::MAX_PARAPHRASE_CHARS;
use jarvis_domain::grants::Sha256;
use jarvis_domain::ids::{ArtifactId, RunId};
use tokio_util::sync::CancellationToken;

use crate::deepdive::{CanvasAction, DeepDiveError, DeepDiveService, ThreadState};
use crate::ports::{ArtifactStore, BlobStore, BlobStoreError, RepositoryError};
use crate::testing::ManualClock;

// --- fake ports -----------------------------------------------------------

#[derive(Default)]
struct FakeBlobs {
    stored: Mutex<Vec<(Sha256, Vec<u8>)>>,
}

#[async_trait::async_trait]
impl BlobStore for FakeBlobs {
    async fn put(&self, bytes: &[u8]) -> Result<Sha256, BlobStoreError> {
        // Deterministic stand-in address for the test (not a real hash), the
        // same shape the coding-worker tests use.
        let mut key = [0u8; 32];
        for (i, b) in bytes.iter().take(31).enumerate() {
            key[i] = *b;
        }
        key[31] = bytes.len() as u8;
        let hash = Sha256::from_bytes(key);
        self.stored.lock().unwrap().push((hash, bytes.to_vec()));
        Ok(hash)
    }
    async fn get(&self, hash: &Sha256) -> Result<Option<Vec<u8>>, BlobStoreError> {
        Ok(self
            .stored
            .lock()
            .unwrap()
            .iter()
            .find(|(h, _)| h == hash)
            .map(|(_, b)| b.clone()))
    }
    async fn contains(&self, hash: &Sha256) -> Result<bool, BlobStoreError> {
        Ok(self.get(hash).await?.is_some())
    }
}

impl FakeBlobs {
    fn last_text(&self) -> String {
        let stored = self.stored.lock().unwrap();
        let (_, bytes) = stored.last().expect("something was stored");
        String::from_utf8(bytes.clone()).expect("markdown is utf-8")
    }
}

#[derive(Default)]
struct FakeArtifacts {
    versions: Mutex<Vec<ArtifactManifest>>,
    audits: Mutex<Vec<AuditEvent>>,
    /// When set, every write fails — an artifact that cannot be audited must
    /// not be persisted (invariant #6).
    refuse: bool,
}

#[async_trait::async_trait]
impl ArtifactStore for FakeArtifacts {
    async fn create_version(
        &self,
        manifest: &ArtifactManifest,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        if self.refuse {
            return Err(RepositoryError::Storage("audit unavailable".to_owned()));
        }
        self.versions.lock().unwrap().push(manifest.clone());
        self.audits.lock().unwrap().push(audit.clone());
        Ok(())
    }
    async fn get(
        &self,
        id: &ArtifactId,
        version: ArtifactVersion,
    ) -> Result<Option<ArtifactManifest>, RepositoryError> {
        Ok(self
            .versions
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.id() == id && m.version() == version)
            .cloned())
    }
    async fn latest(&self, id: &ArtifactId) -> Result<Option<ArtifactManifest>, RepositoryError> {
        Ok(self
            .versions
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.id() == id)
            .max_by_key(|m| m.version())
            .cloned())
    }
    async fn list_versions(
        &self,
        id: &ArtifactId,
    ) -> Result<Vec<ArtifactManifest>, RepositoryError> {
        Ok(self
            .versions
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.id() == id)
            .cloned()
            .collect())
    }
}

/// The instant every promotion audit in this file is pinned to. A wall clock
/// would make the audit row's `occurred_at` unassertable, which is why the
/// service takes the [`Clock`](crate::orchestrator::Clock) port.
const AUDIT_AT_UNIX: u64 = 1_700_000_000;

fn service(promote_after: u32) -> (DeepDiveService, Arc<FakeBlobs>, Arc<FakeArtifacts>) {
    let blobs = Arc::new(FakeBlobs::default());
    let artifacts = Arc::new(FakeArtifacts::default());
    let svc = DeepDiveService::new(
        blobs.clone(),
        artifacts.clone(),
        promote_after,
        "user:owner",
        Arc::new(ManualClock::at_unix(AUDIT_AT_UNIX)),
    );
    (svc, blobs, artifacts)
}

fn run_id() -> RunId {
    "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap()
}

fn artifact_id() -> ArtifactId {
    "01BX5ZZKBKACTAV9WEVGEMMVS1".parse().unwrap()
}

fn started() -> ThreadState {
    let mut state = ThreadState::default();
    state.begin_topic("ramen places near Kreuzberg");
    state
        .thread
        .record_source("Kome Ramen", "https://kome.example/menu")
        .unwrap();
    state
        .thread
        .record_source("Berlin Ramen Guide", "https://guide.example/ramen")
        .unwrap();
    state
}

// --- 1. continuation extends, topic change shelves ------------------------

#[test]
fn a_continuation_extends_the_canvas_and_counts_as_a_follow_up() {
    let (svc, _, _) = service(3);
    let mut state = started();

    let outcome = svc.observe_turn(&mut state, "tell me more about the second one");
    assert_eq!(outcome.canvas, CanvasAction::Extend);
    // The live thread is untouched: extending never retires anything.
    assert!(outcome.retired.is_none());
    assert_eq!(state.follow_ups(), 1);
    assert_eq!(state.thread.topic(), "ramen places near Kreuzberg");
}

#[test]
fn a_genuine_topic_change_shelves_and_retires_the_thread() {
    let (svc, _, _) = service(3);
    let mut state = started();

    let outcome = svc.observe_turn(&mut state, "what's the weather tomorrow");
    assert_eq!(outcome.canvas, CanvasAction::Shelve);
    // The retired thread comes back to the caller rather than being dropped —
    // it may still be worth promoting.
    let retired = outcome.retired.expect("the old thread is handed back");
    assert_eq!(retired.topic(), "ramen places near Kreuzberg");
    assert_eq!(retired.sources().len(), 2);
    // A new thread starts on the new topic, with the follow-up count reset.
    assert_eq!(state.thread.topic(), "what's the weather tomorrow");
    assert!(state.thread.sources().is_empty());
    assert_eq!(state.follow_ups(), 0);
}

#[test]
fn an_empty_thread_is_not_retired_as_if_it_were_worth_keeping() {
    let (svc, _, _) = service(3);
    let mut state = ThreadState::default();
    state.begin_topic("ramen places");
    let outcome = svc.observe_turn(&mut state, "what's the weather tomorrow");
    assert_eq!(outcome.canvas, CanvasAction::Shelve);
    assert!(outcome.retired.is_none());
}

#[test]
fn the_canvas_action_never_speaks_to_approvals() {
    // FR-24/docs/12 §4: pending approvals are exempt from shelving. This layer
    // decides *the canvas action for the result cards only*; it emits no
    // instruction that could retract an approval, which is what keeps the F3b.4
    // exemption intact no matter what the router decides. `CanvasAction` is
    // exhaustive and has exactly two arms — extend, or shelve the result set.
    let (svc, _, _) = service(3);
    let mut state = started();
    for (query, expected) in [
        ("tell me more", CanvasAction::Extend),
        ("what's the weather", CanvasAction::Shelve),
    ] {
        let outcome = svc.observe_turn(&mut state, query);
        assert_eq!(outcome.canvas, expected);
        match outcome.canvas {
            CanvasAction::Extend | CanvasAction::Shelve => {}
        }
        state = started();
    }
}

// --- 2. source handoff is a proposal, and carries no page content ----------

#[test]
fn opening_a_cited_source_proposes_a_browser_navigation() {
    let (svc, _, _) = service(3);
    let mut state = started();

    let outcome = svc.observe_turn(&mut state, "open the second one");
    let handoff = outcome.handoff.expect("a handoff was proposed");
    assert_eq!(handoff.proposal.tool_id.as_str(), "browser.navigate");
    assert_eq!(handoff.url, "https://guide.example/ramen");
    assert_eq!(handoff.domain, "guide.example");
    // Reading a source is a follow-up on the same thread: it must not shelve
    // the canvas the reference is sitting on.
    assert_eq!(outcome.canvas, CanvasAction::Extend);
}

#[test]
fn the_handoff_proposes_only_the_url_and_no_page_content() {
    use jarvis_domain::tools::CanonicalValue;

    let (svc, _, _) = service(3);
    let mut state = started();
    let handoff = svc
        .observe_turn(&mut state, "open that")
        .handoff
        .expect("a handoff was proposed");

    // The whole argument tree is one url — there is no channel here through
    // which page text could travel back onto the HUD (ADR-017 §3).
    let CanonicalValue::Object(args) = &handoff.proposal.arguments else {
        panic!("browser.navigate takes an object");
    };
    assert_eq!(args.len(), 1);
    assert!(matches!(args.get("url"), Some(CanonicalValue::Str(_))));
}

#[test]
fn a_handoff_is_never_offered_for_a_source_that_cannot_be_navigated() {
    let (svc, _, _) = service(3);
    // Nothing consulted yet: nothing to open, and no proposal invented.
    let mut empty = ThreadState::default();
    empty.begin_topic("ramen places");
    assert!(svc.observe_turn(&mut empty, "open that").handoff.is_none());

    // Out of range: opening a page the user did not ask for would be worse than
    // opening none.
    let mut state = started();
    assert!(
        svc.observe_turn(&mut state, "open the fifth one")
            .handoff
            .is_none()
    );
}

#[test]
fn an_explicit_reset_beats_a_handoff_phrase() {
    let (svc, _, _) = service(3);
    let mut state = started();
    let outcome = svc.observe_turn(&mut state, "new topic, open the tax return");
    assert_eq!(outcome.canvas, CanvasAction::Shelve);
    assert!(outcome.handoff.is_none());
}

// --- 3. promotion offer and the Research Notes artifact -------------------

#[test]
fn promotion_is_offered_once_past_the_threshold() {
    let (svc, _, _) = service(3);
    let mut state = started();

    assert!(svc.observe_turn(&mut state, "tell me more").offer.is_none());
    assert!(svc.observe_turn(&mut state, "what else").offer.is_none());
    let offer = svc
        .observe_turn(&mut state, "and what about ramen prices")
        .offer
        .expect("the third follow-up offers to keep the thread");
    // Spoken in Jarvis's own voice, never a dialog box (docs/12 §2.5).
    assert!(offer.contains("Research Notes"), "{offer}");
    assert!(
        !offer.contains('\n'),
        "the offer is one spoken line: {offer}"
    );
    // It does not nag: the same turn does not re-offer.
    assert!(!state.offer_pending_at(state.follow_ups()));
}

#[test]
fn a_zero_threshold_never_offers() {
    let (svc, _, _) = service(0);
    let mut state = started();
    for _ in 0..5 {
        assert!(svc.observe_turn(&mut state, "tell me more").offer.is_none());
    }
}

#[tokio::test]
async fn promoting_writes_a_versioned_audited_markdown_artifact() {
    let (svc, blobs, artifacts) = service(3);
    let mut state = started();
    state
        .thread
        .record_fact("Kome opens at noon and is rated 4.7.")
        .unwrap();
    state
        .thread
        .record_image(
            "a bowl of shoyu ramen",
            "https://cdn.example/one.jpg",
            "https://kome.example/menu",
        )
        .unwrap();

    let notes = svc
        .promote(
            &mut state,
            run_id(),
            artifact_id(),
            &CancellationToken::new(),
        )
        .await
        .expect("promotion succeeds");

    assert_eq!(notes.version.get(), 1);
    let md = blobs.last_text();
    assert!(md.starts_with("# Research Notes: ramen places near Kreuzberg"));
    assert!(md.contains("- Kome opens at noon and is rated 4.7."));
    // Every source consulted, and the image's OWN provenance (ADR-017).
    assert!(md.contains("https://kome.example/menu"));
    assert!(md.contains("https://guide.example/ramen"));

    // The manifest is a markdown artifact whose provenance names the run and
    // every page it was built from.
    let manifest = artifacts.latest(&artifact_id()).await.unwrap().unwrap();
    assert_eq!(manifest.media_type().as_str(), "text/markdown");
    assert_eq!(manifest.renderer_id(), "markdown-html/v1");
    let cited: Vec<String> = manifest
        .sources()
        .iter()
        .filter_map(|s| match s {
            jarvis_domain::artifact::ArtifactSource::Web { url } => Some(url.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        cited,
        ["https://kome.example/menu", "https://guide.example/ramen"]
    );

    // Append-only evidence, written with the manifest (invariant #6).
    let audits = artifacts.audits.lock().unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].event_type, "artifact.created");
    // The timestamp comes from the injected clock, not the wall clock — an
    // audit row is evidence, and evidence with an unassertable time is evidence
    // no test can check (rust-reviewer R6).
    assert_eq!(
        audits[0].occurred_at,
        UNIX_EPOCH + Duration::from_secs(AUDIT_AT_UNIX)
    );
    assert_eq!(audits[0].target, format!("artifact:{}", artifact_id()));
    // The payload is built from structural values only — counts, a constant
    // media type, a hex hash. The thread's *topic* and its facts are Z2/Z4 free
    // text and are deliberately absent: this row is assembled by string
    // formatting, and untrusted text there could produce a malformed audit
    // record. The document itself is the place that content lives.
    assert_eq!(
        audits[0].payload_json,
        format!(
            r#"{{"kind":"markdown_html","mediaType":"text/markdown","sha256":"{}","facts":1,"sources":2,"images":1}}"#,
            notes.sha256_hex
        )
    );
}

#[tokio::test]
async fn promoting_the_same_thread_again_appends_a_version() {
    let (svc, _, artifacts) = service(3);
    let mut state = started();
    state.thread.record_fact("Kome opens at noon.").unwrap();
    let token = CancellationToken::new();

    let first = svc
        .promote(&mut state, run_id(), artifact_id(), &token)
        .await
        .unwrap();
    state
        .thread
        .record_fact("Guide lists eleven more places.")
        .unwrap();
    // The second promotion re-uses the artifact this thread already has; the id
    // passed in is only the one to mint if there is none yet, so handing over a
    // fresh one must not fork the thread into a second artifact.
    let unused: ArtifactId = "01BX5ZZKBKACTAV9WEVGEMMVS2".parse().unwrap();
    let second = svc
        .promote(&mut state, run_id(), unused.clone(), &token)
        .await
        .unwrap();

    assert_eq!(first.artifact_id, second.artifact_id);
    assert_eq!(second.version.get(), 2);
    assert_eq!(
        artifacts.list_versions(&artifact_id()).await.unwrap().len(),
        2
    );
    assert!(artifacts.list_versions(&unused).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_artifact_that_cannot_be_audited_is_not_persisted() {
    let blobs = Arc::new(FakeBlobs::default());
    let artifacts = Arc::new(FakeArtifacts {
        refuse: true,
        ..FakeArtifacts::default()
    });
    let svc = DeepDiveService::new(
        blobs,
        artifacts.clone(),
        3,
        "user:owner",
        Arc::new(ManualClock::at_unix(AUDIT_AT_UNIX)),
    );
    let mut state = started();
    state.thread.record_fact("Kome opens at noon.").unwrap();

    let err = svc
        .promote(
            &mut state,
            run_id(),
            artifact_id(),
            &CancellationToken::new(),
        )
        .await
        .expect_err("a write that cannot be audited fails closed");
    assert!(matches!(err, DeepDiveError::Store(_)));
    assert!(artifacts.latest(&artifact_id()).await.unwrap().is_none());
    // The thread does not believe it was promoted, so the offer can be retried.
    assert!(state.promoted_artifact().is_none());
}

#[tokio::test]
async fn an_empty_thread_is_not_promoted() {
    let (svc, _, artifacts) = service(3);
    let mut state = ThreadState::default();
    state.begin_topic("ramen places");
    let err = svc
        .promote(
            &mut state,
            run_id(),
            artifact_id(),
            &CancellationToken::new(),
        )
        .await
        .expect_err("there is nothing to keep");
    assert!(matches!(err, DeepDiveError::NothingToPromote));
    assert!(artifacts.latest(&artifact_id()).await.unwrap().is_none());
}

#[tokio::test]
async fn a_cancelled_promotion_mints_nothing() {
    let (svc, _, artifacts) = service(3);
    let mut state = started();
    state.thread.record_fact("Kome opens at noon.").unwrap();
    let token = CancellationToken::new();
    token.cancel();

    let err = svc
        .promote(&mut state, run_id(), artifact_id(), &token)
        .await
        .expect_err("an abandoned run does not mint an artifact");
    assert!(matches!(err, DeepDiveError::Cancelled));
    assert!(artifacts.latest(&artifact_id()).await.unwrap().is_none());
}

#[tokio::test]
async fn page_text_cannot_reach_the_promoted_document_through_a_fact() {
    // The paraphrase guard is the enforcement point for "paraphrased, not
    // scraped" (ADR-017): a caller holding a fetched page cannot file it.
    let (svc, _, _) = service(3);
    let mut state = started();
    let page = "Lorem ipsum dolor sit amet. ".repeat(40);
    assert!(page.chars().count() > MAX_PARAPHRASE_CHARS);
    assert!(state.thread.record_fact(page.clone()).is_err());

    // And with nothing else recorded, there is no document to promote either.
    let err = svc
        .promote(
            &mut ThreadState::default(),
            run_id(),
            artifact_id(),
            &CancellationToken::new(),
        )
        .await
        .expect_err("nothing to keep");
    assert!(matches!(err, DeepDiveError::NothingToPromote));
}
