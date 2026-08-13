//! **M3b UX acceptance scenarios** (F3b.9, docs/12 §9, docs/07 §2/§3).
//!
//! docs/12 §9 asks for the HUD's user-visible behaviours to be *"tested"* and
//! *"repeatable"*, not merely demonstrated once in a gate demo. This file is the
//! runnable half: one named scenario per M3b feature, each driving the real use
//! case over the **real** infrastructure (live Postgres, the content-addressed
//! blob store, the real audit chain and outbox) with doubles only where the
//! outermost hop is a device this host does not have — a speaker and a TTS
//! pipeline. `cargo xtask golden` runs each of them by name and fails if one
//! stops existing.
//!
//! Why these seams and not HTTP:
//!
//! * **F3b.7 (timers)** and **F3b.8 (lists)** already have HTTP suites
//!   (`timers_api.rs`, and the lists routes' own tests); what neither proves is
//!   the property the *user* cares about — that the thing survives the daemon
//!   dying. Those scenarios therefore run the service over live Postgres and
//!   rebuild it against the same database, which is what a restart actually is.
//! * **F3b.6 (deep dive)** has no runtime path at all yet: `DeepDiveService` is
//!   constructed nowhere in `jarvisd::run` (F3b.9's sibling work wires it). The
//!   highest *existing* seam is therefore the use case plus jarvisd's card
//!   projection, which is exactly where the two acceptance properties live
//!   (continuation-vs-new-topic, per-tile attribution). See
//!   `docs/milestones/M3b-acceptance.md` §4 for the standing caveat.
//! * **F3b.4 (panel lifecycle)** and the client half of **F3b.5 (map fallback)**
//!   are client-side by construction and are covered by the Angular suite; they
//!   need a browser binary, which this file cannot substitute for and does not
//!   pretend to. `crates/xtask/tests/hud_acceptance.rs` carries the parts of the
//!   §9 checklist that *can* be mechanised without one.
//!
//! Nothing here is scripted through a model: every scenario is deterministic
//! grammar plus stored state, which is also why it is quota-free (ADR-023,
//! ADR-024, and the fixture-driven rule in CLAUDE.md).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use jarvis_application::deepdive::{CanvasAction, DeepDiveService, ThreadState};
use jarvis_application::lists::ListsService;
use jarvis_application::ports::{
    AlertError, AlertPlayer, AnnouncementOutcome, Announcer, ArtifactStore, BlobStore,
};
use jarvis_application::testing::ManualClock;
use jarvis_application::timers::{NewTimer, TimerService, TimerWhen};
use jarvis_contracts::cards::HudCardDto;
use jarvis_domain::artifact::ArtifactVersion;
use jarvis_domain::grants::Sha256;
use jarvis_domain::ids::{ArtifactId, ListId, ListItemId, RunId, TimerId};
use jarvis_domain::lists::{ItemText, ListName};
use jarvis_domain::timers::{TimerAction, TimerKind, TimerState};
use jarvis_infra::artifact_cas::FileBlobStore;
use jarvis_infra::artifacts::PgArtifactStore;
use jarvis_infra::audit::verify_chain;
use jarvis_infra::lists::PgListStore;
use jarvis_infra::timers::PgTimerStore;
use jarvisd::cards::{gallery_card, sources_card};
use jarvisd::timers::TimerEncoder;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

const ACTOR: &str = "device:01ARZ3NDEKTSV4RRFFQ69G5FB3";
const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
const NOTES_ARTIFACT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const LIST_ARTIFACT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB7";
const SHOPPING: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB8";
const PASTA: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB2";

/// A fixed "now" well clear of the epoch, so a scenario can backdate without
/// underflowing.
const T0: u64 = 1_700_000_000;

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("jarvis-m3b-{tag}-{}-{nanos}", std::process::id()))
}

fn item_id(n: u16) -> ListItemId {
    format!("01J8Z{n:021}").parse().expect("valid test ulid")
}

/// Runtime-checked (not `query!`) on purpose: an acceptance scenario should not
/// be able to go stale against the offline sqlx cache, and these two reads own
/// no production SQL.
async fn audit_types(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT event_type FROM audit.audit_events ORDER BY seq ASC")
        .fetch_all(pool)
        .await
        .expect("audit rows")
}

async fn outbox_types(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT event_type FROM outbox.outbox_events ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("outbox rows")
}

/// The speaker. A real one needs an audio device this host may not have, so the
/// scenario records the tone instead of emitting it — the outermost hop, and the
/// only thing faked on the timer path.
#[derive(Default)]
struct RecordingAlert {
    plays: Mutex<u32>,
}

impl RecordingAlert {
    fn count(&self) -> u32 {
        *self.plays.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl AlertPlayer for RecordingAlert {
    async fn play(
        &self,
        _target: Option<&jarvis_domain::ids::DeviceId>,
        _cancel: CancellationToken,
    ) -> Result<(), AlertError> {
        *self.plays.lock().unwrap() += 1;
        Ok(())
    }
}

/// The voice. There is no TTS pipeline before M5, so this records the line that
/// *would* be spoken — which is what the acceptance actually checks: a missed
/// alarm must say it was missed.
#[derive(Default)]
struct RecordingAnnouncer {
    lines: Mutex<Vec<String>>,
}

impl RecordingAnnouncer {
    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Announcer for RecordingAnnouncer {
    async fn announce(
        &self,
        text: &str,
        _target: Option<&jarvis_domain::ids::DeviceId>,
        _cancel: CancellationToken,
    ) -> AnnouncementOutcome {
        self.lines.lock().unwrap().push(text.to_owned());
        AnnouncementOutcome::Spoken
    }
}

// ---------------------------------------------------------------------------
// F3b.6 — deep-dive threads (FR-27, ADR-017, docs/12 §2.5)
// ---------------------------------------------------------------------------

/// **Acceptance: continuation-vs-new-topic, per-item gallery attribution, and
/// Research Notes promotion past the threshold.**
///
/// Three user-visible promises in one narrative, because they are one
/// experience: asking a follow-up must not wipe what is on screen, every image
/// must say where it came from, and a thread worth keeping becomes one document
/// that grows rather than a pile of rival ones.
///
/// The artifact half runs against **live Postgres and the real content-addressed
/// blob store**, and the notes are reopened through a *fresh* pair of stores —
/// the restart analogue — because "keep this" that does not survive a restart is
/// not keeping anything.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn f3b6_a_follow_up_extends_the_canvas_a_new_topic_shelves_it_and_a_thread_promotes_to_one_growing_document(
    pool: PgPool,
) {
    let root = temp_root("deepdive");
    let blobs = Arc::new(FileBlobStore::new(&root));
    let artifacts = Arc::new(PgArtifactStore::new(pool.clone()));
    // `[ui] deepdive_promote_after = 2` so the scenario crosses the threshold
    // without a wall of filler turns; the default is 3 (docs/09 §1).
    // The clock is injected, never read from the wall — same rule the
    // `the_scenarios_read_time_only_from_an_injected_clock` guard below
    // enforces, and what makes the promotion's `occurredAt` assertable.
    let service = DeepDiveService::new(
        blobs.clone(),
        artifacts.clone(),
        2,
        ACTOR,
        Arc::new(ManualClock::at_unix(T0)),
    );
    let cancel = CancellationToken::new();

    // --- the thread as a real turn would have left it ----------------------
    let mut state = ThreadState::default();
    state.begin_topic("ramen in Kreuzberg");
    state
        .thread
        .record_fact("Tonkotsu broth is simmered for many hours.")
        .unwrap();
    state
        .thread
        .record_source("Ramen — Wikipedia", "https://en.wikipedia.org/wiki/Ramen")
        .unwrap();
    state
        .thread
        .record_source("Berlin Ramen Guide", "https://www.guide.example/ramen?p=2")
        .unwrap();
    // Two images from two *different* pages — the case card-level attribution
    // would get wrong (ADR-017).
    state
        .thread
        .record_image(
            "a bowl of shoyu ramen",
            "https://cdn.a.example/1.jpg",
            "https://a.example/page",
        )
        .unwrap();
    state
        .thread
        .record_image(
            "a bowl of miso ramen",
            "https://cdn.b.example/2.jpg",
            "https://b.example/other",
        )
        .unwrap();

    // --- 1. a follow-up EXTENDS -------------------------------------------
    let turn = service.observe_turn(&mut state, "what about the broth?");
    assert_eq!(
        turn.canvas,
        CanvasAction::Extend,
        "a continuation appends to the canvas; it must not shelve what the \
         human is still looking at"
    );
    assert!(
        turn.retired.is_none(),
        "nothing is retired by a follow-up — the thread is still live"
    );
    assert_eq!(state.follow_ups(), 1);

    // …including the one phrasing that could plausibly be read as a new topic:
    // "open the second one" is *about* what is already on the canvas, so it
    // extends and hands back a proposal for the browser worker rather than
    // shelving the very references it points at.
    let handoff_turn = service.observe_turn(&mut state, "open the second one");
    assert_eq!(handoff_turn.canvas, CanvasAction::Extend);
    let handoff = handoff_turn.handoff.expect("a cited source was resolved");
    assert_eq!(handoff.url, "https://www.guide.example/ramen?p=2");
    assert_eq!(
        handoff.domain, "guide.example",
        "the spoken attribution is the parsed host, not anything from the page"
    );

    // The offer is spoken once, at the threshold, and is a single line — never
    // a dialog (docs/12 §2.5).
    let offer = handoff_turn.offer.expect("the threshold was crossed");
    assert!(offer.contains("Research Notes"));
    assert_eq!(offer.lines().count(), 1);

    // --- 2. the cards the canvas would show --------------------------------
    let sources = sources_card("card-src", "References", &state.thread).expect("a sources card");
    let HudCardDto::Sources { items, .. } = &sources else {
        panic!("expected a sources card");
    };
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].domain, "en.wikipedia.org");
    assert_eq!(items[1].domain, "guide.example");

    let gallery = gallery_card("card-gal", "Pictures", &state.thread).expect("a gallery card");
    let HudCardDto::Gallery { images, .. } = &gallery else {
        panic!("expected a gallery card");
    };
    assert_eq!(images.len(), 2);
    // THE F3b.6 acceptance property: attribution is **per item**, and it is the
    // item's own page — not a card-level badge, which the wire type cannot even
    // express (ADR-017).
    assert_eq!(images[0].source_url, "https://a.example/page");
    assert_eq!(images[0].source_domain, "a.example");
    assert_eq!(images[1].source_url, "https://b.example/other");
    assert_eq!(images[1].source_domain, "b.example");
    for image in images {
        assert!(!image.source_url.is_empty(), "every tile links its source");
        assert!(!image.alt.is_empty(), "and carries alt text (docs/12 §8)");
    }

    // --- 3. promotion writes ONE document that grows -----------------------
    let run: RunId = RUN.parse().unwrap();
    let first = service
        .promote(
            &mut state,
            run.clone(),
            NOTES_ARTIFACT.parse().unwrap(),
            &cancel,
        )
        .await
        .expect("the thread promotes");
    assert_eq!(first.version, ArtifactVersion::FIRST);

    // Another turn adds a finding, and a second "keep this" appends a version
    // to the same document rather than minting a rival one.
    state
        .thread
        .record_fact("Shio broth is the lightest of the three.")
        .unwrap();
    let second = service
        .promote(&mut state, run.clone(), fresh_artifact_id(), &cancel)
        .await
        .expect("the second promotion succeeds");
    assert_eq!(
        second.artifact_id, first.artifact_id,
        "a thread is one document that grows (FR-08) — never a second document"
    );
    assert_eq!(second.version.get(), 2);
    assert_ne!(
        second.sha256_hex, first.sha256_hex,
        "and v2 genuinely differs from v1"
    );

    // --- 4. a genuine topic change SHELVES ---------------------------------
    let switched = service.observe_turn(&mut state, "book me a flight to Rome next month");
    assert_eq!(
        switched.canvas,
        CanvasAction::Shelve,
        "only a genuine topic change collapses the canvas into a shelf chip"
    );
    let retired = switched
        .retired
        .expect("the old thread is handed back, not dropped");
    assert_eq!(retired.topic(), "ramen in Kreuzberg");
    assert_eq!(
        state.follow_ups(),
        0,
        "a new thread starts its own promotion count"
    );

    // --- 5. it is all still there after a restart --------------------------
    drop(service);
    let after_restart_artifacts = PgArtifactStore::new(pool.clone());
    let after_restart_blobs = FileBlobStore::new(&root);
    let latest = after_restart_artifacts
        .latest(&first.artifact_id)
        .await
        .unwrap()
        .expect("the notes reopen through a fresh store");
    assert_eq!(latest.version().get(), 2);
    let hash: Sha256 = second.sha256_hex.parse().unwrap();
    let bytes = after_restart_blobs
        .get(&hash)
        .await
        .unwrap()
        .expect("the notes blob reopens");
    let markdown = String::from_utf8(bytes).expect("markdown is utf-8");
    assert!(markdown.contains("ramen in Kreuzberg"));
    assert!(markdown.contains("Shio broth is the lightest of the three."));
    assert!(
        markdown.contains("https://en.wikipedia.org/wiki/Ramen"),
        "a promoted fact never loses which page it came from"
    );

    // Two promotions, two `artifact.created` rows, chain intact (invariant #6).
    assert_eq!(
        audit_types(&pool).await,
        vec!["artifact.created", "artifact.created"]
    );
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(verify_chain(&mut conn).await.unwrap(), 2);

    std::fs::remove_dir_all(&root).ok();
}

/// A second id for the second promotion. It must go **unused** — the thread
/// already owns a document — which is exactly what the scenario asserts.
fn fresh_artifact_id() -> ArtifactId {
    "01ARZ3NDEKTSV4RRFFQ69G5FB6".parse().unwrap()
}

// ---------------------------------------------------------------------------
// F3b.7 — timers (FR-33, ADR-023)
// ---------------------------------------------------------------------------

/// **Acceptance: set → fire → persist across a restart → missed alarm
/// announced.**
///
/// The whole feature in one pass, over live Postgres. The restart is real in the
/// way that matters: the service and its store are dropped and rebuilt against
/// the same database, and the clock the rebuilt service reads is well past the
/// timer's moment — which is what "jarvisd was stopped when the pasta was done"
/// looks like from the inside.
///
/// Every assertion here is one the *user* would notice: the alarm still goes
/// off, it says it was missed rather than pretending to be fresh, it rings
/// exactly once, and dismissing it makes it go away for good.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn f3b7_a_timer_set_before_a_restart_rings_as_a_missed_alarm_after_it(pool: PgPool) {
    let cancel = CancellationToken::new();
    let timer_id: TimerId = PASTA.parse().unwrap();

    // --- before the crash --------------------------------------------------
    let before_alert = Arc::new(RecordingAlert::default());
    let before = TimerService::new(
        Arc::new(PgTimerStore::new(pool.clone())),
        before_alert.clone(),
        Arc::new(RecordingAnnouncer::default()),
        Arc::new(TimerEncoder),
        Arc::new(ManualClock::at_unix(T0)),
    );
    let set = before
        .set(
            timer_id.clone(),
            NewTimer {
                name: Some("pasta timer".to_owned()),
                kind: TimerKind::Countdown {
                    duration: Duration::from_secs(600),
                },
                when: TimerWhen::In(Duration::from_secs(600)),
            },
            ACTOR,
            &cancel,
        )
        .await
        .expect("the timer is set");
    assert_eq!(set.state(), TimerState::Pending);
    assert_eq!(
        before.next_wakeup(&cancel).await.unwrap(),
        Some(Duration::from_secs(600)),
        "the scheduler sleeps exactly until the moment — it never polls"
    );
    assert!(
        before.fire_due(&cancel).await.unwrap().is_empty(),
        "nothing is due yet"
    );
    assert_eq!(before_alert.count(), 0);

    // --- the daemon dies ---------------------------------------------------
    drop(before);

    // --- and comes back an hour late ---------------------------------------
    let alert = Arc::new(RecordingAlert::default());
    let announcer = Arc::new(RecordingAnnouncer::default());
    let after = TimerService::new(
        Arc::new(PgTimerStore::new(pool.clone())),
        alert.clone(),
        announcer.clone(),
        Arc::new(TimerEncoder),
        Arc::new(ManualClock::at_unix(T0 + 3_600)),
    );

    let live = after.list(&cancel).await.unwrap();
    assert_eq!(live.len(), 1, "the timer survived the restart");
    assert_eq!(live[0].name().as_str(), "pasta timer");
    assert_eq!(
        after.next_wakeup(&cancel).await.unwrap(),
        Some(Duration::ZERO),
        "an overdue timer is due now — the startup sweep is not a special path"
    );

    let fired = after.fire_due(&cancel).await.expect("the sweep succeeds");
    assert_eq!(fired.len(), 1);
    assert!(
        fired[0].missed,
        "an hour late is MISSED — never presented as if it had just rung"
    );
    assert!(fired[0].alerted, "the tone sounded");
    assert_eq!(alert.count(), 1);
    assert_eq!(
        announcer.lines(),
        vec!["Missed while I was offline — pasta timer is up"],
        "the human is told it was missed rather than left to infer it"
    );

    // Rings exactly once, even if the scheduler's first wakeup races the sweep.
    assert!(after.fire_due(&cancel).await.unwrap().is_empty());
    assert_eq!(alert.count(), 1, "exactly one tone");

    // Durable state, one outbox event, an intact audit chain (invariant #6).
    assert_eq!(
        after.get(&timer_id, &cancel).await.unwrap().state(),
        TimerState::Fired
    );
    assert_eq!(outbox_types(&pool).await, vec!["timer.fired"]);
    assert_eq!(audit_types(&pool).await, vec!["timer.set", "timer.fired"]);
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(verify_chain(&mut conn).await.unwrap(), 2);

    // --- and dismissing it ends it ----------------------------------------
    let dismissed = after
        .act(&timer_id, TimerAction::Dismiss, None, ACTOR, &cancel)
        .await
        .unwrap();
    assert_eq!(dismissed.state(), TimerState::Dismissed);
    assert!(
        after.list(&cancel).await.unwrap().is_empty(),
        "a dismissed timer leaves the live list for good"
    );
}

// ---------------------------------------------------------------------------
// F3b.8 — lists and quick notes (FR-34, ADR-024)
// ---------------------------------------------------------------------------

/// **Acceptance: add → check off → promote to a versioned artifact.**
///
/// The grocery-list path end to end over live Postgres and the real CAS: items
/// keep the order they were spoken in, a check-off addresses exactly one line by
/// id, and "keep this" produces one document that gains a version each time —
/// never a second rival document for the same list (ADR-024's whole point).
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn f3b8_a_list_item_is_added_checked_off_and_promoted_to_one_versioned_document(
    pool: PgPool,
) {
    let root = temp_root("lists");
    let blobs = Arc::new(FileBlobStore::new(&root));
    let artifacts = Arc::new(PgArtifactStore::new(pool.clone()));
    let service = ListsService::new(
        Arc::new(PgListStore::new(pool.clone())),
        blobs.clone(),
        artifacts.clone(),
        Arc::new(ManualClock::at_unix(T0)),
    );
    let cancel = CancellationToken::new();
    let list_id: ListId = SHOPPING.parse().unwrap();

    // --- "add milk to the shopping list" (before a shopping list exists) ---
    let ensured = service
        .ensure_list(
            list_id.clone(),
            ListName::new("Shopping").unwrap(),
            ACTOR,
            &cancel,
        )
        .await
        .expect("the list is created on first use");
    assert!(
        ensured.was_created(),
        "creation is implicit — a human never has to make a list first"
    );

    for (n, text) in [(1u16, "milk"), (2, "eggs"), (3, "coffee")] {
        service
            .add_item(
                &list_id,
                item_id(n),
                ItemText::new(text).unwrap(),
                ACTOR,
                &cancel,
            )
            .await
            .expect("the item is appended");
    }
    let list = service.get(&list_id).await.unwrap();
    assert_eq!(
        list.items()
            .iter()
            .map(|i| i.text.as_str())
            .collect::<Vec<_>>(),
        vec!["milk", "eggs", "coffee"],
        "items keep the order they were spoken in"
    );

    // --- "check off milk" --------------------------------------------------
    let checked = service
        .set_checked(&list_id, &item_id(1), true, ACTOR, &cancel)
        .await
        .expect("the check-off lands");
    assert!(
        checked
            .items()
            .iter()
            .find(|i| i.id == item_id(1))
            .unwrap()
            .checked,
        "exactly the addressed line is checked"
    );
    assert_eq!(
        checked.open_items().count(),
        2,
        "and only that one — a check-off is not a clear-all"
    );

    // --- "keep this" -------------------------------------------------------
    let run: RunId = RUN.parse().unwrap();
    let first = service
        .promote(
            &list_id,
            LIST_ARTIFACT.parse().unwrap(),
            run.clone(),
            ACTOR,
            &cancel,
        )
        .await
        .expect("the list promotes");
    assert_eq!(first.version, 1);
    assert!(first.first_promotion);

    let hash: Sha256 = first.sha256_hex.parse().unwrap();
    let markdown = String::from_utf8(blobs.get(&hash).await.unwrap().expect("the blob"))
        .expect("markdown is utf-8");
    assert!(markdown.contains("# Shopping"));
    assert!(
        markdown.contains("- [x] milk"),
        "the document records what is done…"
    );
    assert!(
        markdown.contains("- [ ] eggs"),
        "…and what is still open, in order"
    );

    // --- the list keeps growing, the document keeps its identity -----------
    service
        .add_item(
            &list_id,
            item_id(4),
            ItemText::new("bread").unwrap(),
            ACTOR,
            &cancel,
        )
        .await
        .unwrap();
    let second = service
        .promote(
            &list_id,
            // A *different* fresh id, deliberately: the list already owns a
            // document, so this one must be ignored.
            "01ARZ3NDEKTSV4RRFFQ69G5FB5".parse().unwrap(),
            run.clone(),
            ACTOR,
            &cancel,
        )
        .await
        .expect("the second promotion succeeds");
    assert_eq!(
        second.artifact_id, first.artifact_id,
        "one list is one document (ADR-024) — never a rival per save"
    );
    assert_eq!(second.version, 2);
    assert!(!second.first_promotion);

    // --- it all reopens after a restart ------------------------------------
    drop(service);
    let after_restart = PgArtifactStore::new(pool.clone());
    let latest = after_restart
        .latest(&first.artifact_id)
        .await
        .unwrap()
        .expect("the document reopens through a fresh store");
    assert_eq!(latest.version().get(), 2);
    let v2: Sha256 = second.sha256_hex.parse().unwrap();
    let markdown = String::from_utf8(
        FileBlobStore::new(&root)
            .get(&v2)
            .await
            .unwrap()
            .expect("the v2 blob"),
    )
    .expect("markdown is utf-8");
    assert!(markdown.contains("- [ ] bread"));

    // Every step audited, in order, hash chain intact (invariant #6).
    assert_eq!(
        audit_types(&pool).await,
        vec![
            "list.created",
            "list.item_added",
            "list.item_added",
            "list.item_added",
            "list.item_checked",
            "list.promoted",
            "artifact.created",
            "list.item_added",
            "artifact.created",
        ]
    );
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(verify_chain(&mut conn).await.unwrap(), 9);

    std::fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------------------------------
// docs/12 §9 — "every web-sourced image on a card shows its source link"
// ---------------------------------------------------------------------------

/// **Acceptance (mechanised): no card the HUD can be handed carries a web image
/// without its source link.**
///
/// The contract test in `jarvis-contracts` proves the *wire shape* cannot
/// express an unattributed image. This proves the other half — that the
/// **producers** jarvisd actually has cannot emit one either — by driving every
/// image-bearing projection jarvisd owns and walking the serialized JSON for any
/// image URL that is not accompanied by `sourceUrl` + `sourceDomain`.
///
/// It is deliberately a walk over the serialized card rather than a match on
/// known variants: a future card type that adds an image field is caught here
/// without anyone remembering to update the test.
#[test]
fn every_web_sourced_image_a_producer_can_emit_carries_its_source_link() {
    let mut thread = jarvis_domain::deepdive::ResearchThread::new("ramen");
    thread
        .record_source("Ramen — Wikipedia", "https://en.wikipedia.org/wiki/Ramen")
        .unwrap();
    thread
        .record_image(
            "a bowl of ramen",
            "https://cdn.a.example/1.jpg",
            "https://a.example/page",
        )
        .unwrap();
    thread
        .record_image("", "https://cdn.b.example/2.jpg", "https://b.example/other")
        .unwrap();

    let produced = [
        sources_card("card-src", "References", &thread).expect("a sources card"),
        gallery_card("card-gal", "Pictures", &thread).expect("a gallery card"),
    ];

    let mut checked = 0usize;
    for card in &produced {
        let value = serde_json::to_value(card).unwrap();
        checked += assert_images_are_attributed(&value, card.card_type());
    }
    assert!(
        checked >= 2,
        "the walk must actually have found images to check, not vacuously pass"
    );
}

/// Walk a serialized card; every object carrying a `url` that is an *image*
/// must also carry `sourceUrl`, `sourceDomain` and non-empty `alt`. Returns how
/// many images it checked so a caller can prove the walk was not vacuous.
fn assert_images_are_attributed(value: &serde_json::Value, card_type: &str) -> usize {
    match value {
        serde_json::Value::Object(map) => {
            let mut found = 0;
            if let Some(url) = map.get("url").and_then(|v| v.as_str())
                && map.contains_key("alt")
            {
                // This object is a SourcedImageDto-shaped value.
                for field in ["sourceUrl", "sourceDomain"] {
                    let present = map
                        .get(field)
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.trim().is_empty());
                    assert!(
                        present,
                        "{card_type}: image {url} has no {field} — every web-sourced \
                         image on a card shows its source link (docs/12 §9)"
                    );
                }
                assert!(
                    map.get("alt")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.trim().is_empty()),
                    "{card_type}: image {url} has no alt text (docs/12 §8)"
                );
                found += 1;
            }
            for nested in map.values() {
                found += assert_images_are_attributed(nested, card_type);
            }
            found
        }
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| assert_images_are_attributed(item, card_type))
            .sum(),
        _ => 0,
    }
}

/// A compile-time-ish guard on the harness itself: the map/timer/list scenarios
/// above all read their "now" from an injected clock, so nothing in this file
/// depends on when it runs. Kept as a cheap explicit check because a flaky
/// golden is a harness bug (docs/07 §2).
#[test]
fn the_scenarios_read_time_only_from_an_injected_clock() {
    let source = include_str!("m3b_acceptance.rs");
    let code = source
        .split("fn the_scenarios_read_time_only_from_an_injected_clock")
        .next()
        .expect("this test is last in the file");
    // `SystemTime::now()` appears exactly once, in `temp_root`, where it only
    // makes a unique directory name — never in a scenario's decisions.
    assert_eq!(
        code.matches("SystemTime::now()").count(),
        1,
        "a scenario must take its time from ManualClock, never the wall clock"
    );
}
