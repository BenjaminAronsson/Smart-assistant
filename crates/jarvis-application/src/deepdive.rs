//! Deep-dive threads: turn routing, source handoff, and Research Notes
//! promotion (FR-27, ADR-017, docs/12 §2.5).
//!
//! [`jarvis_domain::deepdive`] holds the pure decisions — is this query a
//! continuation, which cited source does "the second one" mean, what does the
//! promoted markdown look like. This module is the *use case* around them: it
//! keeps the live thread's running state, turns the classifier's answer into a
//! canvas instruction the HUD can act on, and writes the promoted document
//! through the F3a.2 artifact ports.
//!
//! Three boundaries are load-bearing:
//!
//! * **The handoff proposes; it never executes** (invariant #1). "Open that"
//!   yields a [`SourceHandoff`] carrying a [`ToolProposal`] for the browser
//!   worker (F3a.5). `policy::evaluate` still classifies it, an approval may
//!   still be required, and only the executor ever runs it. Nothing in this
//!   module holds a [`crate::policy::ToolRegistry`] or a
//!   [`crate::policy::ToolExecutor`] — there is no code path from here to a
//!   side effect.
//! * **The HUD never re-renders page content** (ADR-017 §3). Reading a source
//!   means opening the real page in the browser worker. Accordingly nothing
//!   here carries page body text: a [`SourceHandoff`] is a URL and its
//!   attribution label, and the wire card grammar has no field for a page body
//!   either (`jarvis_contracts::cards`).
//! * **Facts are paraphrases, not scrapes** (ADR-017). The guard lives on
//!   [`ResearchThread::record_fact`], and it is structural, not advisory: a
//!   thread's fields are private, so `record_fact` and its siblings are the
//!   only way content gets in — there is no `thread.facts.push(page_body)` and
//!   no struct literal. A caller holding fetched text therefore cannot file it
//!   as a finding, here or anywhere else in the workspace.
//!
//! Card *construction* is deliberately not here: `jarvis-application` may not
//! depend on `jarvis-contracts` (invariant #3), so this module yields domain
//! values and jarvisd maps them to `HudCardDto`s — the same shape as the
//! display-directive port.

use std::collections::BTreeMap;
use std::sync::Arc;

use jarvis_domain::artifact::{
    ArtifactContent, ArtifactKind, ArtifactManifest, ArtifactSource, ArtifactVersion,
    BuildProvenance, MediaType,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::deepdive::{
    QueryRelation, ResearchThread, classify_query, display_domain, is_explicit_reset,
    is_source_handoff, render_research_notes, select_source, should_offer_promotion,
};
use jarvis_domain::ids::{ArtifactId, RunId};
use jarvis_domain::location::Sensitivity;
use jarvis_domain::tools::{CanonicalValue, ToolId, ToolProposal};
use tokio_util::sync::CancellationToken;

use crate::orchestrator::Clock;
use crate::ports::{ArtifactStore, BlobStore, RepositoryError};

/// What the HUD must do with the materialization canvas for this turn
/// (docs/12 §2.5/§4). Exhaustive and deliberately small: a third answer would
/// change the panel lifecycle, which is a spec decision, not an implementation
/// one.
///
/// Note what is *not* here: anything about approvals. Pending approval cards are
/// exempt from shelving (FR-24, docs/12 §4), and this enum cannot express an
/// instruction that would retract one — the exemption lives in the HUD's panel
/// lifecycle and is not something a router decision can override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasAction {
    /// A continuation: append this turn's cards, leave the prior ones in place.
    Extend,
    /// A genuine topic change: shelve the result cards (restorable, FR-24).
    Shelve,
}

/// "Open that / let me read it" resolved to a concrete page (ADR-017 §3).
///
/// A **proposal**, not a permission. The caller must put `proposal` through
/// `policy::evaluate` like any other tool call; that this struct exists says
/// only that the user asked for a page Jarvis had already cited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceHandoff {
    /// The browser-worker call to submit to the policy engine.
    pub proposal: ToolProposal,
    /// The page to open — one of the thread's own recorded sources, never a URL
    /// parsed out of user or page text.
    pub url: String,
    /// The attribution label for what Jarvis says while opening it.
    pub domain: String,
}

/// One routed deep-dive turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// The classifier's answer, carried through for tracing/telemetry.
    pub relation: QueryRelation,
    pub canvas: CanvasAction,
    /// Present when the user asked to read a source (ADR-017 §3).
    pub handoff: Option<SourceHandoff>,
    /// A spoken one-liner offering to keep the thread (docs/12 §2.5: Jarvis's
    /// normal voice, never a dialog box). `None` most turns.
    pub offer: Option<String>,
    /// The thread a topic change retired, when it had anything worth keeping.
    /// Handed back rather than dropped so a caller can still promote it.
    pub retired: Option<ResearchThread>,
}

/// The live deep-dive thread and its turn bookkeeping.
///
/// Plain data with a private counter: the service decides, this holds. Callers
/// keep one per conversation session.
#[derive(Debug, Clone, Default)]
pub struct ThreadState {
    pub thread: ResearchThread,
    follow_ups: u32,
    offered_at: Option<u32>,
    promoted: Option<ArtifactId>,
}

impl ThreadState {
    /// Start a fresh thread on `topic`, returning the one it replaced if that
    /// had accumulated anything. Resets the follow-up count and the
    /// already-offered mark — a new thread has its own promotion threshold.
    pub fn begin_topic(&mut self, topic: impl Into<String>) -> Option<ResearchThread> {
        let previous = std::mem::replace(&mut self.thread, ResearchThread::new(topic));
        self.follow_ups = 0;
        self.offered_at = None;
        self.promoted = None;
        previous.has_content().then_some(previous)
    }

    /// Follow-ups on the live thread so far (what the promotion threshold
    /// counts).
    pub fn follow_ups(&self) -> u32 {
        self.follow_ups
    }

    /// The artifact this thread was promoted into, if it has been. A second
    /// promotion appends a version to it rather than minting a rival document.
    pub fn promoted_artifact(&self) -> Option<&ArtifactId> {
        self.promoted.as_ref()
    }

    /// Whether an offer is still owed at `follow_ups` — false once made, so the
    /// same turn never asks twice.
    pub fn offer_pending_at(&self, follow_ups: u32) -> bool {
        self.offered_at != Some(follow_ups)
    }
}

/// Why a promotion did not happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeepDiveError {
    /// The thread has nothing in it — no artifact is minted for an empty
    /// document.
    #[error("this thread has nothing worth keeping yet")]
    NothingToPromote,
    /// The run was abandoned before the artifact was persisted (invariant #4).
    #[error("the request was cancelled")]
    Cancelled,
    #[error("could not store the notes: {0}")]
    Blob(String),
    /// Includes the case that matters most: a manifest that could not be
    /// written *with* its audit event (invariant #6) is not persisted at all.
    #[error("could not record the notes: {0}")]
    Store(String),
    /// A version chain that has run out of numbers — not a real condition, but
    /// never silently wrapped.
    #[error("this artifact has no next version")]
    VersionOverflow,
}

impl From<RepositoryError> for DeepDiveError {
    fn from(e: RepositoryError) -> Self {
        Self::Store(e.to_string())
    }
}

/// What a successful promotion produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotedNotes {
    pub artifact_id: ArtifactId,
    pub version: ArtifactVersion,
    pub sha256_hex: String,
}

/// Routes deep-dive turns and promotes threads to Research Notes artifacts.
///
/// Holds two ports and a threshold — no model provider, no policy engine, no
/// executor. Everything it can do is: classify text, shape a proposal, and
/// write a document.
pub struct DeepDiveService {
    blobs: Arc<dyn BlobStore>,
    artifacts: Arc<dyn ArtifactStore>,
    /// `[ui] deepdive_promote_after` (docs/09 §1, default 3). Zero disables the
    /// offer rather than making it constantly.
    promote_after: u32,
    /// Who the `artifact.created` audit names.
    actor: String,
    /// The audit timestamp comes from the [`Clock`] port, never from
    /// `SystemTime::now()` — same discipline as [`crate::lists::ListsService`].
    /// An audit row is evidence (invariant #6), and evidence whose time cannot
    /// be pinned in a test is evidence nothing asserts.
    clock: Arc<dyn Clock>,
}

impl DeepDiveService {
    pub fn new(
        blobs: Arc<dyn BlobStore>,
        artifacts: Arc<dyn ArtifactStore>,
        promote_after: u32,
        actor: impl Into<String>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            blobs,
            artifacts,
            promote_after,
            actor: actor.into(),
            clock,
        }
    }

    /// `[ui] deepdive_promote_after` as configured — exposed so the surface that
    /// wires this service can report the threshold it is running with.
    pub fn promote_after(&self) -> u32 {
        self.promote_after
    }

    /// Route one turn against the live thread.
    ///
    /// The order matters and is the feature's spec:
    ///
    /// 1. An **explicit reset** wins over everything — "new topic" is an
    ///    instruction, not evidence to weigh (docs/12 §2.5).
    /// 2. Otherwise a recognized **source handoff against a thread that has
    ///    cited sources** is a continuation by construction: "open the second
    ///    one" can only mean something that is already on the canvas, so it must
    ///    not shelve the very references it is pointing at. (The bare
    ///    classifier cannot see the sources, which is why this lives here.)
    /// 3. Otherwise [`classify_query`] decides.
    ///
    /// A continuation counts a follow-up and may produce the promotion offer; a
    /// topic change retires the thread and starts a new one.
    pub fn observe_turn(&self, state: &mut ThreadState, query: &str) -> TurnOutcome {
        let reset = is_explicit_reset(query);
        let wants_source = !reset && is_source_handoff(query) && !state.thread.sources().is_empty();

        let relation = if wants_source {
            QueryRelation::Continuation
        } else {
            classify_query(state.thread.topic(), query)
        };

        match relation {
            QueryRelation::Continuation => {
                state.follow_ups = state.follow_ups.saturating_add(1);
                let offer =
                    should_offer_promotion(state.follow_ups, self.promote_after, state.offered_at)
                        .then(|| {
                            state.offered_at = Some(state.follow_ups);
                            promotion_offer(state.thread.topic())
                        });
                TurnOutcome {
                    relation,
                    canvas: CanvasAction::Extend,
                    handoff: wants_source.then(|| self.handoff(state, query)).flatten(),
                    offer,
                    retired: None,
                }
            }
            QueryRelation::NewTopic => TurnOutcome {
                relation,
                canvas: CanvasAction::Shelve,
                // A new topic has no cited sources to open, and a handoff phrase
                // that reached here was overridden by an explicit reset.
                handoff: None,
                offer: None,
                retired: state.begin_topic(query),
            },
        }
    }

    /// Resolve "open that / the second one" to a proposal for the browser
    /// worker. `None` when nothing matches — an out-of-range ordinal opens
    /// nothing rather than a page the user did not ask for, and a source whose
    /// URL has no honest attribution is never navigated to (it could not have
    /// been recorded in the first place, so this is defence in depth).
    fn handoff(&self, state: &ThreadState, query: &str) -> Option<SourceHandoff> {
        let index = select_source(query, state.thread.sources().len())?;
        let source = state.thread.sources().get(index)?;
        let domain = display_domain(source.url())?;
        let mut arguments = BTreeMap::new();
        arguments.insert(
            "url".to_owned(),
            CanonicalValue::str(source.url().to_owned()),
        );
        Some(SourceHandoff {
            proposal: ToolProposal {
                tool_id: ToolId::browser_navigate(),
                arguments: CanonicalValue::Object(arguments),
            },
            url: source.url().to_owned(),
            domain,
        })
    }

    /// Promote the live thread into a versioned Research Notes artifact
    /// (FR-08/FR-27).
    ///
    /// `artifact_id` is the id to mint if this thread has never been promoted;
    /// once it has, the thread keeps its own artifact and this appends a
    /// version to that chain instead (a thread is one document that grows, not
    /// a new document per save).
    ///
    /// The manifest and its `artifact.created` audit event are written in one
    /// transaction by the store (invariant #6): a document that cannot be
    /// audited is not persisted, and this returns [`DeepDiveError::Store`]
    /// without marking the thread promoted, so the offer can be made again.
    pub async fn promote(
        &self,
        state: &mut ThreadState,
        run_id: RunId,
        artifact_id: ArtifactId,
        cancel: &CancellationToken,
    ) -> Result<PromotedNotes, DeepDiveError> {
        if !state.thread.has_content() {
            return Err(DeepDiveError::NothingToPromote);
        }
        if cancel.is_cancelled() {
            return Err(DeepDiveError::Cancelled);
        }

        let markdown = render_research_notes(&state.thread);
        let sha256 = self
            .blobs
            .put(markdown.as_bytes())
            .await
            .map_err(|e| DeepDiveError::Blob(e.to_string()))?;
        let sha256_hex = sha256.to_string();

        // The ports below take no token; don't mint an artifact for a run the
        // user has since abandoned (invariant #4).
        if cancel.is_cancelled() {
            return Err(DeepDiveError::Cancelled);
        }

        // Provenance names the run *and every page the notes were built from*,
        // so a promoted fact never loses which source it came from (docs/04 §4).
        let mut sources = Vec::with_capacity(state.thread.sources().len() + 1);
        sources.push(ArtifactSource::Run(run_id.clone()));
        sources.extend(state.thread.sources().iter().map(|s| ArtifactSource::Web {
            url: s.url().to_owned(),
        }));

        let media_type = MediaType::markdown();
        let content = ArtifactContent {
            sha256,
            media_type: media_type.clone(),
            kind: ArtifactKind::MarkdownHtml,
            sources,
            sensitivity: Sensitivity::Normal,
            // No isolated builder: a Research Notes document is rendered from
            // values already in memory.
            build: BuildProvenance::none(),
            capabilities: Vec::new(),
        };

        let existing = match state.promoted.clone() {
            Some(id) => self.artifacts.latest(&id).await?,
            None => None,
        };
        let manifest = match existing {
            Some(previous) => previous
                .next_version(run_id.clone(), content)
                .ok_or(DeepDiveError::VersionOverflow)?,
            None => ArtifactManifest::initial(artifact_id, run_id.clone(), content),
        };

        let audit = AuditEvent {
            occurred_at: self.clock.now(),
            actor: self.actor.clone(),
            event_type: "artifact.created".to_owned(),
            target: format!("artifact:{}", manifest.id()),
            correlation_id: Some(run_id.to_string()),
            // Structural values only — a constant kind and media type, a hex
            // hash, three counts. The topic and the facts are free text (Z2/Z4)
            // and this row is assembled by formatting, so they stay out of it;
            // the document is where that content belongs.
            payload_json: format!(
                r#"{{"kind":"markdown_html","mediaType":"{}","sha256":"{sha256_hex}","facts":{},"sources":{},"images":{}}}"#,
                media_type.as_str(),
                state.thread.facts().len(),
                state.thread.sources().len(),
                state.thread.images().len()
            ),
        };

        self.artifacts.create_version(&manifest, &audit).await?;

        let promoted = PromotedNotes {
            artifact_id: manifest.id().clone(),
            version: manifest.version(),
            sha256_hex,
        };
        state.promoted = Some(promoted.artifact_id.clone());
        Ok(promoted)
    }
}

/// The spoken offer (docs/12 §2.5: Jarvis's normal voice, one line, never a
/// dialog). Single-line by construction so a caller cannot render it as a form.
fn promotion_offer(topic: &str) -> String {
    let topic = topic.trim().replace(['\n', '\r'], " ");
    if topic.is_empty() {
        "We've covered a fair bit — want me to keep this as a Research Notes document?".to_owned()
    } else {
        format!(
            "We've covered a fair bit on {topic} — want me to keep this as a Research Notes document?"
        )
    }
}
