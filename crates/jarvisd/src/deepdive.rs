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
//!
//! That bound is *global*, and eviction drops a whole session's entry — which
//! is why a slot is allocated only for a session the register actually knows
//! ([`DeepDiveApi::live_session`]). Otherwise a handful of invented ULIDs would
//! be enough to evict every real conversation's canvas state, and each request
//! is bounded in what it may carry ([`MAX_FINDINGS_PER_REQUEST`]) because the
//! loop that consumes it holds a process-global lock.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::{Extension, Json};
use jarvis_application::deepdive::{
    CanvasAction, DeepDiveError, DeepDiveService, ThreadState, TurnOutcome,
};
use jarvis_application::ports::{RepositoryError, SessionStore};
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

/// Most entries one `findings` request may carry **per array**.
///
/// A turn files what it just consulted: a handful of paraphrases, the pages
/// behind them, the images it showed. Sixty-four of each is far past anything a
/// real turn produces and still well under the thread's own totals
/// ([`jarvis_domain::deepdive::MAX_THREAD_FACTS`] and friends), so nothing
/// legitimate is refused by it.
///
/// The reason it exists is the loop it bounds. `record_findings` holds a
/// **process-global** mutex while it walks these arrays — the same mutex every
/// session's `submit_message` needs — so their length is, directly, how long
/// every other conversation waits. A 2 MB body of four-byte facts is half a
/// million iterations under that lock. Checked *before* the lock is taken, which
/// is the half that matters.
const MAX_FINDINGS_PER_REQUEST: usize = 64;

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
    /// The session register, consulted before a request is allowed to allocate
    /// a thread slot. See [`DeepDiveApi::live_session`].
    sessions: Arc<dyn SessionStore>,
    /// Live threads, least-recently-touched first. A `Vec` rather than a map:
    /// it is capped at [`MAX_LIVE_THREADS`], so a linear scan is cheaper than a
    /// hash, and it carries the recency order the eviction needs for free.
    threads: Mutex<Vec<(SessionId, ThreadState)>>,
    canvas: Arc<dyn CanvasSink>,
}

impl DeepDiveApi {
    pub fn new(
        service: Arc<DeepDiveService>,
        sessions: Arc<dyn SessionStore>,
        canvas: Arc<dyn CanvasSink>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                service,
                sessions,
                threads: Mutex::new(Vec::new()),
                canvas,
            }),
        }
    }

    /// Refuse anything that is not a real conversation, before a slot is taken.
    ///
    /// [`MAX_LIVE_THREADS`] is a **global** bound, and eviction drops a whole
    /// session's entry — so without this, eight requests carrying invented (but
    /// well-formed) ULIDs evict every real conversation's canvas state, and
    /// `promote` mints a durable artifact attributed to a session that never
    /// existed. The message path already makes this check before it routes a
    /// turn ([`crate::runs::submit_message`]); these two entry points were the
    /// ones that did not.
    ///
    /// 404 with no distinction between "never existed" and "not readable": the
    /// two REST surfaces that already answer for a session id say exactly this.
    async fn live_session(&self, session_id: &SessionId) -> Result<(), Response> {
        match self.inner.sessions.get(session_id).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(no_such_session()),
            Err(e) => Err(session_lookup_problem(e)),
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
    ///
    /// Every caller has already established that the session is real — the
    /// message path by its own lookup, the two REST handlers through
    /// [`DeepDiveApi::live_session`]. This function is not the place to
    /// re-decide that: it is synchronous, it runs under the lock, and the
    /// register is I/O.
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
///
/// Two things happen before the thread lock is taken, and the order is the
/// point: the session has to exist, and the arrays have to be a plausible size.
/// Everything after the lock runs while every other conversation's turn waits.
#[tracing::instrument(skip_all, fields(session.id = tracing::field::Empty))]
pub async fn record_findings(
    State(api): State<DeepDiveApi>,
    Path(session_id): Path<String>,
    Extension(_device): Extension<DeviceContext>,
    Json(request): Json<DeepDiveFindingsRequest>,
) -> Result<Json<DeepDiveFindingsResponse>, Response> {
    let session_id = parse_session_id(&session_id).ok_or_else(not_a_session_id)?;
    tracing::Span::current().record("session.id", tracing::field::display(&session_id));
    if request.facts.len() > MAX_FINDINGS_PER_REQUEST
        || request.sources.len() > MAX_FINDINGS_PER_REQUEST
        || request.images.len() > MAX_FINDINGS_PER_REQUEST
    {
        return Err(too_many_findings());
    }
    api.live_session(&session_id).await?;

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
            Err(e) => response.refused.push(refusal_reason(&e).to_owned()),
        }
    }
    for source in &request.sources {
        match state
            .thread
            .record_source(source.title.clone(), source.url.clone())
        {
            Ok(()) => response.sources += 1,
            Err(e) => response.refused.push(refusal_reason(&e).to_owned()),
        }
    }
    for image in &request.images {
        match state.thread.record_image(
            image.alt.clone(),
            image.url.clone(),
            image.source_url.clone(),
        ) {
            Ok(()) => response.images += 1,
            Err(e) => response.refused.push(refusal_reason(&e).to_owned()),
        }
    }
    if !response.refused.is_empty() {
        // Counts only. The reasons are ours and are safe to render, but a count
        // is what a log line is for; the caller gets the detail in its response.
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
    // Before any slot is allocated: an artifact minted against a session that
    // does not exist is a durable, audited record of a conversation that never
    // happened.
    api.live_session(&session_id).await?;

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

fn no_such_session() -> Response {
    problem(
        StatusCode::NOT_FOUND,
        ErrorCode::ResourceNotFound,
        "no such session",
        None,
    )
}

/// The session register was unreadable. Storage details never cross the
/// boundary — they carry driver internals (docs/05 §7).
fn session_lookup_problem(error: RepositoryError) -> Response {
    tracing::error!(error = %error, "deep-dive session lookup failed");
    problem(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::ProviderUnavailable,
        "storage unavailable",
        None,
    )
}

/// 422 rather than 400: the body decoded and every field was well-typed, it was
/// the *content* that was out of range — the same distinction `lists::command`
/// draws.
fn too_many_findings() -> Response {
    problem(
        StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::ValidationFailed,
        "a findings request carries at most 64 facts, 64 sources and 64 images",
        None,
    )
}

/// The stable, caller-independent reason one entry was refused.
///
/// Exhaustive with no `_` arm, and that is the guarantee rather than the
/// convenience: these strings go into a response body, so a future
/// [`ThreadError`] variant that carries the offending text has to be given a
/// reason here before it can compile — it cannot arrive by defaulting into
/// `to_string()` and start reflecting untrusted input back at the client.
/// (`ThreadError`'s own `Display` is content-free today; this makes it not
/// matter whether it stays that way.)
fn refusal_reason(error: &jarvis_domain::deepdive::ThreadError) -> &'static str {
    use jarvis_domain::deepdive::ThreadError;
    match error {
        ThreadError::NotAParaphrase => "a fact must be a paraphrase, not fetched page text",
        ThreadError::Empty => "nothing to record",
        ThreadError::FactsFull => "this thread already holds the most findings it can",
        ThreadError::SourcesFull => "this thread already cites the most pages it can",
        ThreadError::ImagesFull => "this thread already references the most images it can",
        ThreadError::Unattributable => "not an attributable http(s) source",
        ThreadError::UrlTooLong => "that URL is too long to record",
    }
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
    use jarvis_application::ports::{
        ArtifactStore, BlobRead, BlobStore, BlobStoreError, RepositoryError,
    };
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

    /// A register that knows every session except the ones deliberately
    /// invented by a test (`session(90..)` — see [`UNKNOWN_SESSION_MARK`]).
    #[derive(Default)]
    struct FakeSessions;

    /// Sessions numbered from here up are *not* in the register, which is how a
    /// test spells "a well-formed ULID nobody ever created".
    const UNKNOWN_SESSION_MARK: u8 = 90;

    #[async_trait::async_trait]
    impl jarvis_application::ports::SessionStore for FakeSessions {
        async fn create(
            &self,
            _session: &jarvis_domain::conversations::Session,
            _idempotency_key: Option<&str>,
            _audit: &AuditEvent,
        ) -> Result<jarvis_application::ports::CreateOutcome, RepositoryError> {
            unreachable!("the deep-dive surface never creates a session")
        }
        async fn get(
            &self,
            id: &SessionId,
        ) -> Result<Option<jarvis_domain::conversations::Session>, RepositoryError> {
            let invented = (UNKNOWN_SESSION_MARK..=99).any(|n| *id == session(n));
            Ok((!invented).then(|| {
                jarvis_domain::conversations::Session::new(
                    id.clone(),
                    Some("a conversation".to_owned()),
                    UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                )
            }))
        }
        async fn list(
            &self,
            _limit: u32,
        ) -> Result<Vec<jarvis_domain::conversations::Session>, RepositoryError> {
            Ok(Vec::new())
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
            DeepDiveApi::new(service, Arc::new(FakeSessions), canvas.clone()),
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

    // --- the two REST entry points -----------------------------------------

    fn a_device() -> DeviceContext {
        DeviceContext {
            class: jarvis_domain::identity::DeviceClass::OwnerUi,
            device_id: crate::auth::fresh_id(),
            user_id: crate::auth::fresh_id(),
            scopes: Vec::new(),
        }
    }

    async fn post_findings(
        api: &DeepDiveApi,
        session: &SessionId,
        request: DeepDiveFindingsRequest,
    ) -> Result<DeepDiveFindingsResponse, StatusCode> {
        record_findings(
            State(api.clone()),
            Path(session.to_string()),
            Extension(a_device()),
            Json(request),
        )
        .await
        .map(|Json(body)| body)
        .map_err(|response| response.status())
    }

    #[tokio::test]
    async fn a_findings_request_is_bounded_before_the_global_lock_is_taken() {
        // S3. The loop that consumes these arrays holds the process-global
        // thread mutex — the same one every session's `submit_message` needs —
        // so an unbounded array is an unbounded stall for every other
        // conversation. A 2 MB body of four-byte facts is ~500k iterations.
        let (api, canvas, _, _) = api(3);
        let session = session(1);

        let too_many = DeepDiveFindingsRequest {
            facts: vec!["a fact".to_owned(); MAX_FINDINGS_PER_REQUEST + 1],
            ..DeepDiveFindingsRequest::default()
        };
        assert_eq!(
            post_findings(&api, &session, too_many).await.unwrap_err(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        // Refused whole: no slot was allocated, no canvas was published, and in
        // particular the lock was never taken.
        assert!(api.inner.threads.lock().await.is_empty());
        assert_eq!(canvas.count(), 0);

        // Each array is bounded on its own, not just their sum.
        for request in [
            DeepDiveFindingsRequest {
                sources: vec![a_source("t", "https://example.org/a"); MAX_FINDINGS_PER_REQUEST + 1],
                ..DeepDiveFindingsRequest::default()
            },
            DeepDiveFindingsRequest {
                images: vec![
                    jarvis_contracts::deepdive::ImageFindingDto {
                        alt: "a".to_owned(),
                        url: "https://cdn.example.org/a.jpg".to_owned(),
                        source_url: "https://example.org/p".to_owned(),
                    };
                    MAX_FINDINGS_PER_REQUEST + 1
                ],
                ..DeepDiveFindingsRequest::default()
            },
        ] {
            assert_eq!(
                post_findings(&api, &session, request).await.unwrap_err(),
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }

        // A request at the bound is a normal request — the cap is far past any
        // real turn and refuses nothing legitimate.
        let at_bound = DeepDiveFindingsRequest {
            facts: (0..MAX_FINDINGS_PER_REQUEST)
                .map(|i| format!("finding {i}"))
                .collect(),
            ..DeepDiveFindingsRequest::default()
        };
        let filed = post_findings(&api, &session, at_bound).await.unwrap();
        assert_eq!(filed.facts as usize, MAX_FINDINGS_PER_REQUEST);
    }

    #[tokio::test]
    async fn refusals_are_a_fixed_vocabulary_and_never_quote_the_caller_back() {
        // S4. `refused` used to carry `ThreadError::Unattributable(url)`
        // stringified — arbitrary-length, unsanitized, attacker-chosen text
        // reflected straight back into the response body.
        let (api, _, _, _) = api(3);
        let session = session(2);
        let marker = "marker9f3a";
        let request = DeepDiveFindingsRequest {
            facts: vec![format!("{marker}{}", "x".repeat(1000))],
            sources: vec![
                a_source("t", &format!("javascript:alert('{marker}')")),
                a_source("t", &format!("https://example.org/{}", marker.repeat(500))),
            ],
            images: vec![jarvis_contracts::deepdive::ImageFindingDto {
                alt: "a".to_owned(),
                url: format!("data:image/png;base64,{marker}"),
                source_url: "https://example.org/p".to_owned(),
            }],
        };

        let response = post_findings(&api, &session, request).await.unwrap();
        assert_eq!(response.refused.len(), 4);
        for reason in &response.refused {
            assert!(
                !reason.contains(marker),
                "the response echoed input: {reason}"
            );
            assert!(!reason.contains("javascript:"), "{reason}");
            assert!(!reason.contains("data:"), "{reason}");
        }
        // Bounded by construction: at most three arrays of
        // `MAX_FINDINGS_PER_REQUEST`, each mapping to one short constant.
        assert!(response.refused.len() <= 3 * MAX_FINDINGS_PER_REQUEST);
        assert!(response.refused.iter().all(|r| r.len() < 120));
    }

    #[tokio::test]
    async fn an_invented_session_id_gets_no_thread_slot_and_no_artifact() {
        // S6. `MAX_LIVE_THREADS` is a *global* bound and eviction drops a whole
        // session's entry, so eight well-formed but invented ULIDs used to be
        // enough to evict every real conversation's canvas state — and `promote`
        // would mint a durable, audited artifact against a session that never
        // existed.
        let (api, canvas, blobs, artifacts) = api(3);
        let real = session(3);
        let invented = session(UNKNOWN_SESSION_MARK);

        // The real session works, so the guard is not simply refusing everything.
        post_findings(
            &api,
            &real,
            DeepDiveFindingsRequest {
                facts: vec!["Kome opens at noon.".to_owned()],
                ..DeepDiveFindingsRequest::default()
            },
        )
        .await
        .unwrap();
        let live_before = api.inner.threads.lock().await.len();
        assert_eq!(live_before, 1);

        assert_eq!(
            post_findings(&api, &invented, DeepDiveFindingsRequest::default())
                .await
                .unwrap_err(),
            StatusCode::NOT_FOUND
        );
        let promotion = promote(
            State(api.clone()),
            Path(invented.to_string()),
            Extension(a_device()),
        )
        .await;
        assert_eq!(
            promotion.err().map(|r| r.status()),
            Some(StatusCode::NOT_FOUND)
        );

        // No slot was allocated, so the real conversation's thread is untouched,
        // and nothing durable was written for a session that does not exist.
        assert_eq!(api.inner.threads.lock().await.len(), live_before);
        assert!(blobs.stored.lock().unwrap().is_empty());
        assert!(artifacts.versions.lock().unwrap().is_empty());
        assert!(artifacts.audits.lock().unwrap().is_empty());
        // Exactly the one publish the real request made.
        assert_eq!(canvas.count(), 1);
    }

    #[tokio::test]
    async fn a_hostile_page_title_reaches_the_card_stripped() {
        // S2, at the surface it matters on: the title renders inline beside the
        // honestly-computed domain chip, and the alt text is spoken by TTS. The
        // stripping is done in the recorder, so this holds for every projection
        // rather than for the ones that remembered.
        let (api, canvas, _, _) = api(3);
        let session = session(4);
        post_findings(
            &api,
            &session,
            DeepDiveFindingsRequest {
                sources: vec![a_source(
                    "Wikipedia\u{202e}gpj.exe\u{202c}",
                    "https://en.wikipedia.org/wiki/Ramen",
                )],
                images: vec![jarvis_contracts::deepdive::ImageFindingDto {
                    alt: "a bowl\u{200b}\u{0007} of ramen".to_owned(),
                    url: "https://cdn.example.org/one.jpg".to_owned(),
                    source_url: "https://kome.example/menu".to_owned(),
                }],
                ..DeepDiveFindingsRequest::default()
            },
        )
        .await
        .unwrap();

        let cards = canvas.last().cards;
        let HudCardDto::Sources { items, .. } = &cards[0] else {
            panic!("expected a sources card");
        };
        let HudCardDto::Gallery { images, .. } = &cards[1] else {
            panic!("expected a gallery card");
        };
        for hostile in ['\u{202e}', '\u{202c}', '\u{200b}', '\u{0007}'] {
            assert!(!items[0].title.contains(hostile), "{:?}", items[0].title);
            assert!(!images[0].alt.contains(hostile), "{:?}", images[0].alt);
        }
        // Stripped, not dropped: the reference is still cited and still readable.
        assert!(items[0].title.starts_with("Wikipedia"));
        assert_eq!(items[0].domain, "en.wikipedia.org");
        assert_eq!(images[0].alt, "a bowl of ramen");
    }

    #[tokio::test]
    async fn an_over_long_url_is_refused_rather_than_stored() {
        // S1. The URL was the one untrusted field with no bound: 50 sources plus
        // 32 images x 2 URLs, each up to the body limit, per thread — and every
        // publish clones the whole card set into the broadcast ring.
        let (api, canvas, _, _) = api(3);
        let session = session(5);
        let long = format!(
            "https://example.org/{}",
            "a".repeat(jarvis_domain::deepdive::MAX_URL_CHARS)
        );
        let response = post_findings(
            &api,
            &session,
            DeepDiveFindingsRequest {
                sources: vec![
                    a_source("Fine title", &long),
                    a_source("Real", "https://guide.example/ramen"),
                ],
                ..DeepDiveFindingsRequest::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(response.sources, 1);
        assert_eq!(response.refused.len(), 1);
        // One bad entry never costs the good ones, and nothing over the ceiling
        // reached the card the client renders and the ring broadcasts.
        let cards = canvas.last().cards;
        let HudCardDto::Sources { items, .. } = &cards[0] else {
            panic!("expected a sources card");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].url, "https://guide.example/ramen");
    }

    #[test]
    fn every_canvas_action_has_a_wire_mapping() {
        // Exhaustive by construction: a new variant fails to compile here
        // before it can ship as a silently-defaulted wire value.
        assert_eq!(canvas_action(CanvasAction::Extend), CanvasActionDto::Extend);
        assert_eq!(canvas_action(CanvasAction::Shelve), CanvasActionDto::Shelve);
    }
}
