//! Golden 10 — "the house survives" (F10.8, M10 exit evidence, docs/07 §2).
//!
//! The M10 row promises an operational lifecycle: **install, talk, break,
//! restore, upgrade, roll back**. This holds the parts a script can hold.
//!
//! # What makes this different from F10.2's restore test
//!
//! `jarvis-infra/tests/backup_restore.rs` proves the *data* survives: the
//! manifest is there, the blob is readable, the device is still paired. That is
//! necessary and it is not the same claim.
//!
//! This proves the restored house is still **usable**, which is a stronger
//! claim than "the rows came back":
//!
//! * the scheduler's own re-arm query (`list_live`) returns the timer, so it
//!   would actually ring — not merely that a row exists;
//! * the automation still holds its `created_by`, the authority it borrows at
//!   fire time;
//! * the artifact resolves to its bytes through the restored blob root;
//! * and the **write path works** — a new session and message are created and
//!   read back. A restored database that returned everything but rejected the
//!   next INSERT (an unrestored sequence, a constraint left behind) would pass
//!   every assertion in F10.2 and still be a dead house.
//!
//! It stops short of driving a full model turn. That needs a provider, and a
//! golden trace whose result depended on a fake model answering would be
//! measuring the fake. `golden12_the_house_answers` covers the answering path
//! on a live database; this covers whether a *restored* one can still be
//! written to and scheduled from.
//!
//! # What it deliberately does not prove
//!
//! * **Install on a clean machine.** `docs/TRY-IT.md` is a verified runbook and
//!   `infra/install/first-run.sh` checks a real install, but neither is this
//!   process — a machine that has never had a toolchain, a container runtime or
//!   a Rust build cache is not something a test running inside that build cache
//!   can simulate. M10 acceptance names it as a human step.
//! * **The upgrade half of the lifecycle.** Applying migrations forward with
//!   live data is `jarvis-infra/tests/update_rollback.rs`; running the *old
//!   binary* against a rolled-back database needs two builds of two versions,
//!   which is a release-process step and not a unit of this suite.
//!
//! Naming those honestly is part of the deliverable. A golden trace that
//! quietly implied it had installed anything would be worse evidence than one
//! that says where the human picks up.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jarvis_application::ports::{
    ArtifactStore, AutomationStore, BlobStore, IdentityStore, MessageStore, SessionStore,
    TimerStore,
};
use jarvis_domain::artifact::{
    ArtifactContent, ArtifactKind, ArtifactManifest, ArtifactSource, BuildProvenance,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::automations::{Automation, AutomationAction, AutomationName, Trigger};
use jarvis_domain::identity::{Device, DeviceClass};
use jarvis_domain::location::Sensitivity;
use jarvis_domain::timers::{Timer, TimerKind, TimerName};
use jarvis_domain::tools::{CanonicalValue, ToolId};
use sqlx::PgPool;

const OWNER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA1";
const OWNER_USER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
const KITCHEN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA2";
const ARTIFACT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const RUN: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
const AUTOMATION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB4";
const TIMER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB5";
const NOTE: &[u8] = b"# the note the owner would miss\n";

fn t0() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

fn pg_container() -> Option<String> {
    let name =
        std::env::var("JARVIS_PG_CONTAINER").unwrap_or_else(|_| "jarvis-dev-postgres-1".to_owned());
    std::process::Command::new("docker")
        .args(["exec", &name, "pg_dump", "--version"])
        .output()
        .is_ok_and(|o| o.status.success())
        .then_some(name)
}

fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

fn swap_database(url: &str, db: &str) -> String {
    match url.rfind('/') {
        Some(i) => format!("{}/{db}", &url[..i]),
        None => url.to_owned(),
    }
}

fn url_of(pool: &PgPool) -> String {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let opts = pool.connect_options();
    swap_database(&base, opts.get_database().expect("database name"))
}

/// A restore target that drops itself even when the test panics.
struct Target {
    url: String,
    name: String,
    base: String,
    container: String,
}

impl Drop for Target {
    fn drop(&mut self) {
        let admin = swap_database(&self.base, "postgres");
        let _ = std::process::Command::new("docker")
            .args(["exec", "-i", &self.container, "psql", &admin, "-c"])
            .arg(format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                self.name
            ))
            .output();
    }
}

fn fresh_database(base: &str, container: &str) -> Target {
    let name = format!(
        "jarvis_golden10_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let admin = swap_database(base, "postgres");
    let out = std::process::Command::new("docker")
        .args(["exec", "-i", container, "psql", &admin, "-c"])
        .arg(format!("CREATE DATABASE {name}"))
        .output()
        .expect("create database");
    assert!(out.status.success(), "creating {name}");
    Target {
        url: swap_database(base, &name),
        name,
        base: base.to_owned(),
        container: container.to_owned(),
    }
}

fn script(
    name: &str,
    args: &[&str],
    db: &str,
    artifacts: &std::path::Path,
    container: &str,
) -> bool {
    let out = std::process::Command::new("bash")
        .arg(repo_root().join("infra/install").join(name))
        .args(args)
        .env("DATABASE_URL", db)
        .env("JARVIS__STORAGE__ARTIFACTS_ROOT", artifacts)
        .env("JARVIS_PG_CONTAINER", container)
        .output()
        .unwrap_or_else(|e| panic!("running {name}: {e}"));
    if !out.status.success() {
        eprintln!(
            "{name}:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out.status.success()
}

fn audit(event_type: &str, target: &str) -> AuditEvent {
    AuditEvent {
        occurred_at: t0(),
        actor: format!("device:{OWNER}"),
        event_type: event_type.to_owned(),
        target: target.to_owned(),
        correlation_id: None,
        payload_json: "{}".into(),
    }
}

fn device(id: &str, name: &str, class: DeviceClass, hash: &str) -> Device {
    Device {
        id: id.parse().expect("ulid"),
        user_id: OWNER_USER.parse().expect("ulid"),
        name: name.to_owned(),
        token_hash: hash.to_owned(),
        public_key: None,
        class,
        created_at: t0(),
        last_seen_at: None,
        revoked_at: None,
        revoked_reason: None,
    }
}

fn sha256(bytes: &[u8]) -> jarvis_domain::grants::Sha256 {
    use sha2::{Digest, Sha256 as S};
    jarvis_domain::grants::Sha256::from_bytes(
        <[u8; 32]>::try_from(&*S::digest(bytes).to_vec()).expect("32 bytes"),
    )
}

/// **Golden 10.** A lived-in house is backed up, lost, restored — and still
/// works.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn golden10_a_restored_house_still_answers(pool: PgPool) {
    let Some(container) = pg_container() else {
        eprintln!("SKIP: no reachable Postgres container");
        return;
    };
    let scratch = tempfile::tempdir().expect("tempdir");
    let blobs_before = scratch.path().join("artifacts");
    let blobs_after = scratch.path().join("restored-artifacts");
    let backups = scratch.path().join("backups");

    // ---- a house somebody actually lives in -----------------------------
    let identity = jarvis_infra::identity::PgIdentityStore::new(pool.clone());
    identity
        .pair_device(
            "owner",
            &device(OWNER, "the laptop", DeviceClass::OwnerUi, "hash-owner"),
            &audit("device.paired", &format!("device:{OWNER}")),
        )
        .await
        .expect("pair the owner");

    let blob = jarvis_infra::artifact_cas::FileBlobStore::new(&blobs_before);
    let address = blob.put(NOTE).await.expect("store the note");
    jarvis_infra::artifacts::PgArtifactStore::new(pool.clone())
        .create_version(
            &ArtifactManifest::initial(
                ARTIFACT.parse().expect("ulid"),
                RUN.parse().expect("ulid"),
                ArtifactContent {
                    sha256: sha256(NOTE),
                    media_type: "text/markdown".parse().expect("media type"),
                    kind: ArtifactKind::MarkdownHtml,
                    sources: vec![ArtifactSource::Run(RUN.parse().expect("ulid"))],
                    sensitivity: Sensitivity::Normal,
                    build: BuildProvenance::none(),
                    capabilities: vec![],
                },
            ),
            &audit("artifact.created", &format!("artifact:{ARTIFACT}")),
        )
        .await
        .expect("record the artifact");

    let fire_at = t0() + Duration::from_secs(900);
    jarvis_infra::timers::PgTimerStore::new(pool.clone())
        .create(
            &Timer::schedule(
                TIMER.parse().expect("ulid"),
                TimerName::new("bread").expect("name"),
                TimerKind::Countdown {
                    duration: Duration::from_secs(900),
                },
                fire_at,
                t0(),
            )
            .expect("schedulable"),
            &audit("timer.created", &format!("timer:{TIMER}")),
        )
        .await
        .expect("set a timer");

    jarvis_infra::automations::PgAutomationStore::new(pool.clone())
        .create(
            &Automation::create(
                AUTOMATION.parse().expect("ulid"),
                AutomationName::new("evening lights").expect("name"),
                Trigger::DailyAt {
                    minutes_since_midnight: 1140,
                },
                AutomationAction {
                    tool_id: ToolId::home_set_light(),
                    arguments: CanonicalValue::obj([
                        ("entity_id", CanonicalValue::str("light.kitchen")),
                        ("state", CanonicalValue::str("on")),
                    ]),
                },
                KITCHEN.parse().expect("device id"),
                t0(),
            ),
            &audit("automation.created", &format!("automation:{AUTOMATION}")),
        )
        .await
        .expect("create an automation");

    let db = url_of(&pool);

    // ---- back up --------------------------------------------------------
    assert!(
        script(
            "backup.sh",
            &[backups.to_str().expect("path")],
            &db,
            &blobs_before,
            &container
        ),
        "a house that cannot be backed up has no lifecycle to test"
    );
    let taken = std::fs::read_dir(&backups)
        .expect("backups")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .next()
        .expect("one backup");

    // ---- break, then restore somewhere else ------------------------------
    // A different database *and* a different blob root: a pass here cannot be
    // either original store quietly still being present.
    let target = fresh_database(&db, &container);
    assert!(
        script(
            "restore.sh",
            &[taken.to_str().expect("path")],
            &target.url,
            &blobs_after,
            &container
        ),
        "the documented restore must succeed"
    );

    let restored = PgPool::connect(&target.url)
        .await
        .expect("connect to the restored house");

    // ---- the claim: it still WORKS, not merely that rows came back -------
    //
    // Everything below reads through the production repositories against the
    // restored database, because that is what the daemon would do on its next
    // start. `list_live` in particular is the scheduler's own re-arm query, so
    // a timer that appears here is a timer that would actually ring.

    let devices = jarvis_infra::identity::PgIdentityStore::new(restored.clone())
        .list_devices()
        .await
        .expect("list devices");
    assert_eq!(devices.len(), 1, "the owner must still be able to get in");
    assert_eq!(devices[0].class, DeviceClass::OwnerUi);

    let live = jarvis_infra::timers::PgTimerStore::new(restored.clone())
        .list_live()
        .await
        .expect("re-arm the schedule");
    assert_eq!(live.len(), 1, "a restored timer must still be armed");
    assert_eq!(
        live[0].fire_at(),
        fire_at,
        "and still due when it was set for"
    );

    let automations = jarvis_infra::automations::PgAutomationStore::new(restored.clone())
        .list_all()
        .await
        .expect("list automations");
    assert_eq!(automations.len(), 1);
    assert_eq!(
        automations[0].created_by().to_string(),
        KITCHEN,
        "an automation borrows its creator's authority at fire time — losing that \
         column means firing under the wrong one, or not at all"
    );

    // The artifact resolves to its bytes, from the restored blob root.
    let manifest = jarvis_infra::artifacts::PgArtifactStore::new(restored.clone())
        .latest(&ARTIFACT.parse().expect("ulid"))
        .await
        .expect("query")
        .expect("the artifact is still listed");
    let bytes = jarvis_infra::artifact_cas::FileBlobStore::new(&blobs_after)
        .get(&address)
        .await
        .expect("read")
        .expect("the artifact's bytes must be readable after a restore");
    assert_eq!(bytes, NOTE);
    assert_eq!(*manifest.sha256(), address);

    // And the house can still record a conversation — the write path, not just
    // the read path. A restored database that rejected the next INSERT (a
    // sequence not restored, a constraint left behind) would satisfy every
    // assertion above and still be a dead house.
    let session: jarvis_domain::ids::SessionId =
        "01ARZ3NDEKTSV4RRFFQ69G5FC1".parse().expect("ulid");
    jarvis_infra::sessions::PgSessionStore::new(restored.clone())
        .create(
            &jarvis_domain::conversations::Session::new(
                session.clone(),
                Some("after the restore".to_owned()),
                t0(),
            ),
            None,
            &audit("session.created", &format!("session:{session}")),
        )
        .await
        .expect("a restored house must still accept a new conversation");

    jarvis_infra::messages::PgMessageStore::new(restored.clone())
        .append(&jarvis_domain::conversations::Message::new(
            "01ARZ3NDEKTSV4RRFFQ69G5FC2".parse().expect("ulid"),
            session.clone(),
            jarvis_domain::conversations::MessageRole::User,
            "is everything still here?".to_owned(),
            t0(),
        ))
        .await
        .expect("a restored house must still accept a message");

    let history = jarvis_infra::messages::PgMessageStore::new(restored.clone())
        .list_by_session(&session, 10)
        .await
        .expect("read the conversation back");
    assert_eq!(
        history.len(),
        1,
        "the write path must work after a restore, not only the read path"
    );

    restored.close().await;
}
