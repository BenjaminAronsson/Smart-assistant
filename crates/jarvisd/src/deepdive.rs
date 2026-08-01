//! Deep-dive surface (F3b.6, FR-27, ADR-017, docs/12 §2.3/§2.5) — the wiring
//! that makes a thread a *running* thing rather than a service nobody calls.
//!
//! Three entry points, in the order a turn actually uses them:
//!
//! 1. **The turn itself is an ordinary message** (docs/12 §2.5: "every
//!    follow-up is a normal run on the Run Spine"). `POST
//!    /api/v1/sessions/{id}/messages` calls [`DeepDiveApi::observe_turn`]
//!    before it spawns the run, so the continuation-vs-new-topic decision rides
//!    the real conversational path and costs the client no extra call. The
//!    decision goes out as the transient `hud.canvas` event: `extend` appends,
//!    `shelve` files the displaced panels under a label (FR-24).
//! 2. **`POST /api/v1/sessions/{id}/deepdive/findings`** files what the turn
//!    consulted — paraphrased facts, pages, images — through the thread's
//!    guarded recorders, and republishes the canvas so the sources and gallery
//!    cards appear.
//! 3. **`POST /api/v1/sessions/{id}/deepdive/promote`** accepts the offer: the
//!    thread becomes a versioned markdown artifact through the F3a.2 ports.
//!
//! ## What this module is not allowed to do
//!
//! * **It executes nothing** (invariant #1). A source handoff ("open the second
//!   one") is published as a *citation* — the url and domain the sources card
//!   already carries — never as a command. The [`ToolProposal`] the application
//!   layer builds for the browser worker stays here, is recorded on the span,
//!   and reaches nothing: opening a page has to go through `policy::evaluate`
//!   like any other tool call, and `browser.navigate` is not even registered in
//!   this binary yet. Nothing in this module holds a registry or an executor.
//! * **It writes no content into a thread directly.** Every fact, source and
//!   image goes through `ResearchThread::record_*`, which is the *only* door:
//!   the struct's fields are private, so the paraphrase cap (ADR-017: facts are
//!   paraphrased, not scraped) and the `is_web_url`/`display_domain`
//!   attribution check cannot be routed around from here. A refused entry is
//!   reported back and simply does not exist in the thread. That is
//!   deliberately where the untrusted input of this feature lands: titles,
//!   URLs and alt text come from fetched pages (Z4).
//! * **It cannot retract an approval.** The canvas instruction has two values,
//!   `extend` and `shelve`, and the client's panel lifecycle exempts pending
//!   approvals from both (docs/12 §4, F3b.4) — there is no value here that
//!   could regress that exemption.
//!
//! ## Live threads are in memory, and bounded
//!
//! A thread is conversation state, not a record: it lives for as long as the
//! conversation does, and the durable artifact is what survives a restart
//! (ADR-017 — "the canvas keeps showing only the current conversation; the
//! artifact is the durable bibliography"). So threads are held in a
//! [`MAX_LIVE_THREADS`]-entry map, most-recently-used last, and the oldest is
//! dropped when a new session arrives. Nothing here grows without a bound
//! (docs/09 §5).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::{Extension, Json};
use jarvis_application::deepdive::{
    CanvasAction, DeepDiveError, DeepDiveService, ThreadState, TurnOutcome,
};
use jarvis_contracts::cards::HudCardDto;
use jarvis_contracts::deepdive::{
    CanvasActionDto, DeepDiveFindingsRequest, DeepDiveFindingsResponse, HudCanvasDto,
    PromoteNotesResponse, SourceHandoffDto,
};
use jarvis_contracts::errors::ErrorCode;
use jarvis_domain::ids::{ArtifactId, RunId, SessionId};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::auth::DeviceContext;
use crate::cards::{CanvasSink, gallery_card, sources_card};
use crate::problem::problem;

/// How many conversations keep a live deep-dive thread at once. A single owner
/// does not hold a dozen threads in their head either; past this the
/// least-recently-touched one is dropped, which costs its (unpromoted) canvas
/// state and nothing durable.
const MAX_LIVE_THREADS: usize = 8;

/// The sources card's title — "show me the references" (docs/12 §2.5).
const SOURCES_TITLE: &str = "References";

/// The gallery card's title.
const GALLERY_TITLE: &str = "Images";

/// State for the deep-dive routes. Cloneable so it can be axum route state, and
/// so `submit_message` can hold a handle to route the turn.
#[derive(Clone)]
pub struct DeepDiveApi {
    inner: Arc<Inner>,
}

struct Inner {
    service: Arc<DeepDiveService>,
    /// Live threads, least-recently-touched first. A `Vec` rather than a map:
    /// it is capped at [`MAX_LIVE_THREADS`], so a linear scan is cheaper than a
    /// hash, and it carries the recency order the eviction needs for free.
    threads: Mutex<Vec<(SessionId, ThreadState)>>,
    canvas: Arc<dyn CanvasSink>,
}

impl DeepDiveApi {
    pub fn new(service: Arc<DeepDiveService>, canvas: Arc<dyn CanvasSink>) -> Self {
        Self {
            inner: Arc::new(Inner {
                service,
                threads: Mutex::new(Vec::new()),
                canvas,
            }),
        }
    }

    /// Route one turn against the session's live thread and publish the canvas
    /// instruction (FR-27, ADR-017).
    ///
    /// Called by `POST /api/v1/sessions/{id}/messages` before the run is
    /// spawned — the deep-dive signal belongs to the turn, not to a second
    /// request the client has to remember to make. Returns the outcome for the
    /// caller's span; the visible effect is the published event.
    ///
    /// Never fails and never blocks a message: a classification is a pure
    /// function over text, and a canvas instruction nobody is subscribed to is
    /// simply not delivered.
    #[tracing::instrument(skip_all, fields(
        session.id = %session_id,
        deepdive.relation = tracing::field::Empty,
        deepdive.follow_ups = tracing::field::Empty,
        deepdive.handoff_tool = tracing::field::Empty,
    ))]
    pub async fn observe_turn(&self, session_id: &SessionId, utterance: &str) -> TurnOutcome {
        let mut threads = self.inner.threads.lock().await;
        let index = self.inner.slot_for(&mut threads, session_id);
        let (_, state) = &mut threads[index];

        // The label names the panels being *displaced*, so it is the topic as it
        // stood before the router touched it.
        let displaced_label = state.thread.topic().to_owned();
        let outcome = self.inner.service.observe_turn(state, utterance);
        let cards = thread_cards(session_id, state);

        let span = tracing::Span::current();
        span.record("deepdive.relation", tracing::field::debug(outcome.relation));
        span.record("deepdive.follow_ups", state.follow_ups());
        if let Some(handoff) = &outcome.handoff {
            // The proposal is observable and goes no further: it is not
            // executed here, and there is no path from this module to one.
            // `domain` is the parsed host, never the raw URL — a span field is
            // a log line, and an unparsed URL there is a forging primitive.
            span.record(
                "deepdive.handoff_tool",
                tracing::field::display(handoff.proposal.tool_id.as_str()),
            );
            tracing::info!(
                source.domain = %handoff.domain,
                "deep-dive source handoff proposed (not executed)"
            );
        }
        if let Some(retired) = &outcome.retired {
            // Handed back by the service rather than dropped. It is deliberately
            // NOT auto-promoted: promotion mints a durable artifact, and doing
            // that on every topic change would write documents nobody asked for
            // (ADR-017 — the offer is an offer). The panels stay restorable on
            // the shelf, which is the reversible half of the same decision.
            tracing::info!(
                retired.sources = retired.sources().len(),
                retired.facts = retired.facts().len(),
                "deep-dive thread retired by a topic change"
            );
        }

        // Publish outside the lock: the thread map serialises every turn on this
        // process, so nothing that is not strictly thread state belongs inside it.
        drop(threads);
        self.inner.canvas.publish(HudCanvasDto {
            session_id: Some(session_id.clone()),
            action: canvas_action(outcome.canvas),
            label: displaced_label,
            cards,
            offer: outcome.offer.clone(),
            handoff: outcome.handoff.as_ref().map(|h| SourceHandoffDto {
                url: h.url.clone(),
                domain: h.domain.clone(),
            }),
        });
        outcome
    }
}

impl Inner {
    /// Index of this session's thread, creating it (and evicting the
    /// least-recently-touched entry when full) and moving it to the
    /// most-recent end.
    fn slot_for(&self, threads: &mut Vec<(SessionId, ThreadState)>, session: &SessionId) -> usize {
        if let Some(position) = threads.iter().position(|(id, _)| id == session) {
            let entry = threads.remove(position);
            threads.push(entry);
        } else {
            if threads.len() >= MAX_LIVE_THREADS {
                let (evicted, _) = threads.remove(0);
                tracing::info!(session.id = %evicted, "deep-dive thread evicted (bound reached)");
            }
            threads.push((session.clone(), ThreadState::default()));
        }
        threads.len() - 1
    }
}

/// Project the live thread onto the canvas's card set.
///
/// The **whole** current set, not a delta: a client applying it upsert-by-id
/// converges on the thread's real state however many events it missed, which is
/// what makes the transient classification of `hud.canvas` honest. The ids are
/// therefore stable per session — the same bibliography is the same card.
fn thread_cards(session_id: &SessionId, state: &ThreadState) -> Vec<HudCardDto> {
    [
        sources_card(
            format!("deepdive-sources-{session_id}"),
            SOURCES_TITLE,
            &state.thread,
        ),
        gallery_card(
            format!("deepdive-gallery-{session_id}"),
            GALLERY_TITLE,
            &state.thread,
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Map the application decision onto the wire. Exhaustive on purpose: a third
/// canvas action would force a wire decision here rather than defaulting.
fn canvas_action(action: CanvasAction) -> CanvasActionDto {
    match action {
        CanvasAction::Extend => CanvasActionDto::Extend,
        CanvasAction::Shelve => CanvasActionDto::Shelve,
    }
}

/// `POST /api/v1/sessions/{id}/deepdive/findings` — file what this turn
/// consulted (FR-27, ADR-017).
///
/// Everything in the body is untrusted (Z4: it originates in fetched pages) and
/// every item goes through the thread's guarded recorders, which are the only
/// way content enters a thread at all. A refused item — a "fact" too long to be
/// a paraphrase, a URL with no honest attribution — is reported in `refused`
/// and is simply not in the thread; one bad entry never costs the good ones.
#[tracing::instrument(skip_all, fields(session.id = tracing::field::Empty))]
pub async fn record_findings(
    State(api): State<DeepDiveApi>,
    Path(session_id): Path<String>,
    Extension(_device): Extension<DeviceContext>,
    Json(request): Json<DeepDiveFindingsRequest>,
) -> Result<Json<DeepDiveFindingsResponse>, Response> {
    let session_id = parse_session_id(&session_id).ok_or_else(not_a_session_id)?;
    tracing::Span::current().record("session.id", tracing::field::display(&session_id));

    let mut threads = api.inner.threads.lock().await;
    let index = api.inner.slot_for(&mut threads, &session_id);
    let (_, state) = &mut threads[index];

    let mut response = DeepDiveFindingsResponse {
        facts: 0,
        sources: 0,
        images: 0,
        refused: Vec::new(),
    };
    for fact in &request.facts {
        match state.thread.record_fact(fact.clone()) {
            Ok(()) => response.facts += 1,
            Err(e) => response.refused.push(e.to_string()),
        }
    }
    for source in &request.sources {
        match state
            .thread
            .record_source(source.title.clone(), source.url.clone())
        {
            Ok(()) => response.sources += 1,
            Err(e) => response.refused.push(e.to_string()),
        }
    }
    for image in &request.images {
        match state.thread.record_image(
            image.alt.clone(),
            image.url.clone(),
            image.source_url.clone(),
        ) {
            Ok(()) => response.images += 1,
            Err(e) => response.refused.push(e.to_string()),
        }
    }
    if !response.refused.is_empty() {
        // Counts only: the reasons quote the caller's own input, which is
        // untrusted text and does not belong in the log stream.
        tracing::info!(
            refused = response.refused.len(),
            "deep-dive findings refused"
        );
    }

    // Republish the canvas so the references and gallery appear. `extend`:
    // filing what a turn consulted is never a topic change, so it must not
    // shelve the canvas those cards belong to.
    let cards = thread_cards(&session_id, state);
    let label = state.thread.topic().to_owned();
    drop(threads);
    api.inner.canvas.publish(HudCanvasDto {
        session_id: Some(session_id),
        action: CanvasActionDto::Extend,
        label,
        cards,
        offer: None,
        handoff: None,
    });

    Ok(Json(response))
}

/// `POST /api/v1/sessions/{id}/deepdive/promote` — the human accepted the offer
/// (FR-08/FR-27).
///
/// The document is written through the same artifact ports as every other
/// artifact (F3a.2): the manifest and its `artifact.created` audit event land in
/// one transaction, so a document that cannot be audited is not persisted at all
/// (invariant #6). Re-promoting the same thread appends a version rather than
/// minting a rival document.
#[tracing::instrument(skip_all, fields(session.id = tracing::field::Empty))]
pub async fn promote(
    State(api): State<DeepDiveApi>,
    Path(session_id): Path<String>,
    Extension(_device): Extension<DeviceContext>,
) -> Result<Json<PromoteNotesResponse>, Response> {
    let session_id = parse_session_id(&session_id).ok_or_else(not_a_session_id)?;
    tracing::Span::current().record("session.id", tracing::field::display(&session_id));

    // The id to mint if this thread has never been promoted; a thread that has
    // keeps its own document and this is unused (the host owns randomness).
    let artifact_id: ArtifactId = crate::auth::fresh_id();
    // A promotion the owner asked for is its own occasion; the run id correlates
    // the artifact's provenance to this request, same as the list promotion.
    let run_id: RunId = crate::auth::fresh_id();
    let cancel = CancellationToken::new();

    // The thread lock is held across the write, so a promotion serialises with
    // the turns of every session. That is deliberate and cheap: `promote` needs
    // `&mut ThreadState` to mark the thread promoted (a second promotion must
    // version the SAME document, never mint a rival one), and taking the state
    // out and putting it back would lose any turn that landed in between. The
    // stall it can cause is bounded by the artifact store — which
    // `submit_message` already awaits twice, for the message row and the run
    // row, so a wedged database blocks a turn with or without this.
    let mut threads = api.inner.threads.lock().await;
    let index = api.inner.slot_for(&mut threads, &session_id);
    let (_, state) = &mut threads[index];
    let promoted = api
        .inner
        .service
        .promote(state, run_id, artifact_id, &cancel)
        .await
        .map_err(promotion_problem)?;

    Ok(Json(PromoteNotesResponse {
        artifact_id: promoted.artifact_id,
        version: promoted.version.get(),
        sha256: promoted.sha256_hex,
        first_promotion: promoted.version.get() == 1,
    }))
}

/// Parse the session path segment, or `None`.
///
/// `Option` rather than `Result<_, Response>` for the same reason as
/// `TimerFault`/`IdFault` elsewhere in jarvisd: an axum `Response` is large, and
/// returning one in a helper's `Err` makes that result enormous (clippy
/// `result_large_err`).
///
/// The **raw** segment is deliberately never logged: axum percent-decodes path
/// parameters, so an id containing a newline would otherwise forge a log line.
/// A parsed [`SessionId`] is 26 characters of Crockford base32 and cannot.
fn parse_session_id(raw: &str) -> Option<SessionId> {
    raw.parse().ok()
}

fn not_a_session_id() -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        ErrorCode::ValidationFailed,
        "session id is not a ULID",
        None,
    )
}

fn promotion_problem(error: DeepDiveError) -> Response {
    match error {
        // Well-formed, and retrying it unchanged will not help: the thread has
        // to consult something before there is a document to keep.
        DeepDiveError::NothingToPromote => problem(
            StatusCode::CONFLICT,
            ErrorCode::DeepDiveNothingToPromote,
            "this thread has nothing worth keeping yet",
            None,
        ),
        DeepDiveError::VersionOverflow => problem(
            StatusCode::CONFLICT,
            ErrorCode::ResourceVersionConflict,
            "this document has no next version",
            None,
        ),
        DeepDiveError::Cancelled => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::ProviderUnavailable,
            "the request was cancelled",
            None,
        ),
        DeepDiveError::Blob(e) | DeepDiveError::Store(e) => {
            tracing::error!(error = %e, "Research Notes promotion failed");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "storage unavailable",
                None,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_application::orchestrator::Clock;
    use jarvis_application::ports::{ArtifactStore, BlobStore, BlobStoreError, RepositoryError};
    use jarvis_domain::artifact::{ArtifactManifest, ArtifactVersion};
    use jarvis_domain::audit::AuditEvent;
    use jarvis_domain::grants::Sha256;
    use std::sync::Mutex as StdMutex;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    // --- fakes ------------------------------------------------------------

    #[derive(Default)]
    struct FakeBlobs {
        stored: StdMutex<Vec<(Sha256, Vec<u8>)>>,
    }

    #[async_trait::async_trait]
    impl BlobStore for FakeBlobs {
        async fn put(&self, bytes: &[u8]) -> Result<Sha256, BlobStoreError> {
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
        versions: StdMutex<Vec<ArtifactManifest>>,
        audits: StdMutex<Vec<AuditEvent>>,
    }

    #[async_trait::async_trait]
    impl ArtifactStore for FakeArtifacts {
        async fn create_version(
            &self,
            manifest: &ArtifactManifest,
            audit: &AuditEvent,
        ) -> Result<(), RepositoryError> {
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
        async fn latest(
            &self,
            id: &ArtifactId,
        ) -> Result<Option<ArtifactManifest>, RepositoryError> {
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

    struct FixedClock;
    impl Clock for FixedClock {
        fn now(&self) -> SystemTime {
            UNIX_EPOCH + Duration::from_secs(1_700_000_000)
        }
    }

    #[derive(Default)]
    struct RecordingCanvas {
        published: StdMutex<Vec<HudCanvasDto>>,
    }

    impl CanvasSink for RecordingCanvas {
        fn publish(&self, canvas: HudCanvasDto) {
            self.published.lock().unwrap().push(canvas);
        }
    }

    impl RecordingCanvas {
        fn last(&self) -> HudCanvasDto {
            self.published
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("a canvas instruction was published")
        }
        fn count(&self) -> usize {
            self.published.lock().unwrap().len()
        }
    }

    fn api(
        promote_after: u32,
    ) -> (
        DeepDiveApi,
        Arc<RecordingCanvas>,
        Arc<FakeBlobs>,
        Arc<FakeArtifacts>,
    ) {
        let blobs = Arc::new(FakeBlobs::default());
        let artifacts = Arc::new(FakeArtifacts::default());
        let canvas = Arc::new(RecordingCanvas::default());
        let service = Arc::new(DeepDiveService::new(
            blobs.clone(),
            artifacts.clone(),
            promote_after,
            "user:owner",
            Arc::new(FixedClock),
        ));
        (
            DeepDiveApi::new(service, canvas.clone()),
            canvas,
            blobs,
            artifacts,
        )
    }

    fn session(n: u8) -> SessionId {
        format!("01J8Z0000000000000000000{n:02}").parse().unwrap()
    }

    async fn file(api: &DeepDiveApi, session: &SessionId, request: DeepDiveFindingsRequest) {
        let mut threads = api.inner.threads.lock().await;
        let index = api.inner.slot_for(&mut threads, session);
        let (_, state) = &mut threads[index];
        for fact in request.facts {
            let _ = state.thread.record_fact(fact);
        }
        for source in request.sources {
            let _ = state.thread.record_source(source.title, source.url);
        }
        for image in request.images {
            let _ = state
                .thread
                .record_image(image.alt, image.url, image.source_url);
        }
    }

    fn a_source(title: &str, url: &str) -> jarvis_contracts::deepdive::SourceFindingDto {
        jarvis_contracts::deepdive::SourceFindingDto {
            title: title.to_owned(),
            url: url.to_owned(),
        }
    }

    // --- the wiring actually runs -----------------------------------------

    #[tokio::test]
    async fn a_turn_publishes_a_canvas_instruction_and_a_follow_up_extends_it() {
        let (api, canvas, _, _) = api(3);
        let session = session(1);

        api.observe_turn(&session, "ramen places near Kreuzberg")
            .await;
        assert_eq!(canvas.last().action, CanvasActionDto::Shelve);

        file(
            &api,
            &session,
            DeepDiveFindingsRequest {
                sources: vec![a_source(
                    "Berlin Ramen Guide",
                    "https://guide.example/ramen",
                )],
                ..DeepDiveFindingsRequest::default()
            },
        )
        .await;

        api.observe_turn(&session, "tell me more about that").await;
        let published = canvas.last();
        // The whole point of FR-27: a follow-up extends, it does not shelve.
        assert_eq!(published.action, CanvasActionDto::Extend);
        assert_eq!(published.session_id.as_ref(), Some(&session));
    }

    #[tokio::test]
    async fn the_sources_card_reaches_the_wire_for_a_real_turn() {
        let (api, canvas, _, _) = api(3);
        let session = session(2);
        api.observe_turn(&session, "ramen places near Kreuzberg")
            .await;
        file(
            &api,
            &session,
            DeepDiveFindingsRequest {
                sources: vec![a_source(
                    "Ramen — Wikipedia",
                    "https://en.wikipedia.org/wiki/Ramen",
                )],
                images: vec![jarvis_contracts::deepdive::ImageFindingDto {
                    alt: "a bowl of shoyu ramen".to_owned(),
                    url: "https://cdn.example/one.jpg".to_owned(),
                    source_url: "https://kome.example/menu".to_owned(),
                }],
                ..DeepDiveFindingsRequest::default()
            },
        )
        .await;

        api.observe_turn(&session, "show me the references").await;
        let cards = canvas.last().cards;
        let types: Vec<&str> = cards.iter().map(HudCardDto::card_type).collect();
        assert_eq!(types, ["card.sources", "card.gallery"]);
        let HudCardDto::Sources { items, .. } = &cards[0] else {
            panic!("expected a sources card");
        };
        // The chip label is computed host-side from the parsed host (docs/12 §2.3).
        assert_eq!(items[0].domain, "en.wikipedia.org");
        // And it serializes — this is what actually reaches the client.
        let json = serde_json::to_value(&cards[0]).unwrap();
        assert_eq!(json["type"], "card.sources");
    }

    #[tokio::test]
    async fn a_topic_change_shelves_under_the_label_of_what_it_displaced() {
        let (api, canvas, _, _) = api(3);
        let session = session(3);
        api.observe_turn(&session, "ramen places near Kreuzberg")
            .await;
        file(
            &api,
            &session,
            DeepDiveFindingsRequest {
                sources: vec![a_source("Guide", "https://guide.example/ramen")],
                ..DeepDiveFindingsRequest::default()
            },
        )
        .await;

        api.observe_turn(&session, "what's the weather tomorrow")
            .await;
        let published = canvas.last();
        assert_eq!(published.action, CanvasActionDto::Shelve);
        assert_eq!(published.label, "ramen places near Kreuzberg");
        // The new thread has consulted nothing, so the canvas starts empty —
        // the old cards are on the shelf, not duplicated onto it.
        assert!(published.cards.is_empty());
    }

    #[tokio::test]
    async fn the_promotion_offer_is_actually_made_past_the_threshold() {
        let (api, canvas, _, _) = api(2);
        let session = session(4);
        api.observe_turn(&session, "ramen places near Kreuzberg")
            .await;
        file(
            &api,
            &session,
            DeepDiveFindingsRequest {
                facts: vec!["Kome opens at noon.".to_owned()],
                sources: vec![a_source("Guide", "https://guide.example/ramen")],
                ..DeepDiveFindingsRequest::default()
            },
        )
        .await;

        api.observe_turn(&session, "tell me more").await;
        assert!(canvas.last().offer.is_none());
        api.observe_turn(&session, "what else").await;
        let offer = canvas.last().offer.expect("the offer is made on the wire");
        assert!(offer.contains("Research Notes"), "{offer}");
        assert!(!offer.contains('\n'), "one spoken line: {offer}");
    }

    #[tokio::test]
    async fn accepting_the_offer_writes_the_versioned_audited_document() {
        let (api, _, blobs, artifacts) = api(2);
        let session = session(5);
        api.observe_turn(&session, "ramen places near Kreuzberg")
            .await;
        file(
            &api,
            &session,
            DeepDiveFindingsRequest {
                facts: vec!["Kome opens at noon and is rated 4.7.".to_owned()],
                sources: vec![a_source("Guide", "https://guide.example/ramen")],
                ..DeepDiveFindingsRequest::default()
            },
        )
        .await;

        let promoted = {
            let mut threads = api.inner.threads.lock().await;
            let index = api.inner.slot_for(&mut threads, &session);
            let (_, state) = &mut threads[index];
            api.inner
                .service
                .promote(
                    state,
                    crate::auth::fresh_id(),
                    crate::auth::fresh_id(),
                    &CancellationToken::new(),
                )
                .await
                .expect("promotion succeeds")
        };

        assert_eq!(promoted.version.get(), 1);
        let md = blobs.last_text();
        assert!(md.starts_with("# Research Notes: ramen places near Kreuzberg"));
        assert!(md.contains("- Kome opens at noon and is rated 4.7."));
        assert!(md.contains("https://guide.example/ramen"));
        // Written with its audit event (invariant #6).
        assert_eq!(artifacts.audits.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_unattributable_source_never_reaches_a_card() {
        // B1: the untrusted URL is the whole risk of this wiring. A `javascript:`
        // URL is refused by the recorder, so it cannot become a link target, a
        // chip label, or a line in the promoted document.
        let (api, canvas, _, _) = api(3);
        let session = session(6);
        api.observe_turn(&session, "ramen places").await;
        file(
            &api,
            &session,
            DeepDiveFindingsRequest {
                sources: vec![
                    a_source("Totally safe", "javascript:alert(1)"),
                    a_source("Spoof", "https://wikipedia.org@evil.example/x"),
                ],
                ..DeepDiveFindingsRequest::default()
            },
        )
        .await;

        api.observe_turn(&session, "show me the references").await;
        let cards = canvas.last().cards;
        let HudCardDto::Sources { items, .. } = &cards[0] else {
            panic!("expected a sources card");
        };
        // Only the http(s) one survived, and it is labelled by its REAL host.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].domain, "evil.example");
    }

    #[tokio::test]
    async fn a_handoff_is_published_as_a_citation_and_nothing_executable() {
        let (api, canvas, _, _) = api(3);
        let session = session(7);
        api.observe_turn(&session, "ramen places").await;
        file(
            &api,
            &session,
            DeepDiveFindingsRequest {
                sources: vec![a_source("Guide", "https://guide.example/ramen")],
                ..DeepDiveFindingsRequest::default()
            },
        )
        .await;

        let outcome = api.observe_turn(&session, "open that").await;
        // The application layer still produced a proposal for the browser
        // worker — and it stayed here (invariant #1): what goes on the wire is
        // the citation, with no tool id and no arguments.
        assert_eq!(
            outcome.handoff.unwrap().proposal.tool_id.as_str(),
            "browser.navigate"
        );
        let handoff = canvas.last().handoff.expect("the citation is published");
        assert_eq!(handoff.url, "https://guide.example/ramen");
        assert_eq!(handoff.domain, "guide.example");
        // Reading a source is a follow-up: it must not shelve the very
        // references it points at.
        assert_eq!(canvas.last().action, CanvasActionDto::Extend);
    }

    #[tokio::test]
    async fn live_threads_are_bounded() {
        let (api, canvas, _, _) = api(3);
        for n in 0..(MAX_LIVE_THREADS as u8 + 3) {
            api.observe_turn(&session(n), "a fresh topic").await;
        }
        assert_eq!(api.inner.threads.lock().await.len(), MAX_LIVE_THREADS);
        // Every turn still published — eviction costs canvas state, not events.
        assert_eq!(canvas.count(), MAX_LIVE_THREADS + 3);
    }

    #[test]
    fn every_canvas_action_has_a_wire_mapping() {
        // Exhaustive by construction: a new variant fails to compile here
        // before it can ship as a silently-defaulted wire value.
        assert_eq!(canvas_action(CanvasAction::Extend), CanvasActionDto::Extend);
        assert_eq!(canvas_action(CanvasAction::Shelve), CanvasActionDto::Shelve);
    }
}
