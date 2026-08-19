//! F10.2: a backup is worth exactly what a restore proves (FR-30).
//!
//! These tests run `infra/install/backup.sh` and `restore.sh` — the **real
//! scripts an operator runs**, not a Rust reimplementation of what they do. A
//! second implementation would be a second thing to be wrong, and the whole
//! point of this feature is that the artifact everyone actually uses works.
//!
//! # The trap this exists for
//!
//! A house is two stores: Postgres holds artifact *manifests*, the filesystem
//! CAS holds the *bytes* they point at. Back up either alone and the restore
//! looks complete — every artifact listed, none of them readable — and you find
//! out when you open the one that mattered.
//!
//! They skip when no Postgres container is reachable, so a machine without one
//! stays green rather than red-for-the-wrong-reason.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jarvis_application::ports::{
    ArtifactStore, AutomationStore, BlobStore, IdentityStore, TimerStore,
};
use jarvis_domain::artifact::{
    ArtifactContent, ArtifactKind, ArtifactManifest, ArtifactSource, BuildProvenance,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::automations::{Automation, AutomationAction, AutomationName, Trigger};
use jarvis_domain::identity::{Device, DeviceClass};
use jarvis_domain::ids::{ArtifactId, AutomationId, RunId, TimerId};
use jarvis_domain::location::Sensitivity;
use jarvis_domain::timers::{Timer, TimerKind, TimerName};
use jarvis_domain::tools::{CanonicalValue, ToolId};
use jarvis_infra::artifact_cas::FileBlobStore;
use jarvis_infra::artifacts::PgArtifactStore;
use jarvis_infra::automations::PgAutomationStore;
use jarvis_infra::identity::PgIdentityStore;
use jarvis_infra::timers::PgTimerStore;
use sqlx::PgPool;

const ARTIFACT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const RUN: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
const BLOB: &[u8] = b"# The note the owner cared about\n";
const OWNER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA1";
const OWNER_USER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
const AUTOMATION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB2";
const TIMER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB3";

/// The container running the server, if these tests can reach one.
///
/// The scripts run Postgres' tools **inside the server's container** when told
/// to, and these tests use that path deliberately: it is the one documented for
/// a compose deployment, and it is the only way to guarantee the client is not
/// newer than the server. A newer client writes dumps an older server refuses —
/// found the hard way here, with a host `pg_dump` 18 against a server 16.
fn pg_container() -> Option<String> {
    let name =
        std::env::var("JARVIS_PG_CONTAINER").unwrap_or_else(|_| "jarvis-dev-postgres-1".to_owned());
    let reachable = Command::new("docker")
        .args(["exec", &name, "pg_dump", "--version"])
        .output()
        .is_ok_and(|o| o.status.success());
    reachable.then_some(name)
}

macro_rules! require_pg_tools {
    () => {
        match pg_container() {
            Some(name) => name,
            None => {
                eprintln!(
                    "SKIP: no reachable Postgres container. Start it with \
                     `docker compose -f infra/compose/dev.yml up -d postgres`, or set \
                     JARVIS_PG_CONTAINER."
                );
                return;
            }
        }
    };
}

fn t0() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

fn house_audit(event_type: &str) -> AuditEvent {
    AuditEvent {
        occurred_at: t0(),
        actor: format!("device:{OWNER}"),
        event_type: event_type.to_owned(),
        target: format!("device:{OWNER}"),
        correlation_id: None,
        payload_json: "{}".into(),
    }
}

/// The single directory `backup.sh` just wrote.
fn one_backup(backups: &Path) -> std::path::PathBuf {
    std::fs::read_dir(backups)
        .expect("backups dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .next()
        .expect("one backup")
}

fn repo_root() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

/// The URL `#[sqlx::test]` handed us, which points at its throwaway database.
fn url_of(pool: &PgPool) -> String {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let opts = pool.connect_options();
    let db = opts.get_database().expect("the test database has a name");
    swap_database(&base, db)
}

fn swap_database(url: &str, db: &str) -> String {
    match url.rfind('/') {
        Some(i) => format!("{}/{db}", &url[..i]),
        None => url.to_owned(),
    }
}

fn manifest() -> ArtifactManifest {
    ArtifactManifest::initial(
        ARTIFACT.parse::<ArtifactId>().expect("ulid"),
        RUN.parse::<RunId>().expect("ulid"),
        ArtifactContent {
            sha256: jarvis_domain::grants::Sha256::from_bytes(
                <[u8; 32]>::try_from(&*sha2_hex(BLOB)).expect("32 bytes"),
            ),
            media_type: "text/markdown".parse().expect("media type"),
            kind: ArtifactKind::MarkdownHtml,
            sources: vec![ArtifactSource::Run(RUN.parse().expect("ulid"))],
            sensitivity: Sensitivity::Normal,
            build: BuildProvenance::none(),
            capabilities: vec![],
        },
    )
}

fn sha2_hex(bytes: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).to_vec()
}

fn audit() -> AuditEvent {
    AuditEvent {
        occurred_at: std::time::SystemTime::now(),
        actor: "system".into(),
        event_type: "artifact.created".into(),
        target: format!("artifact:{ARTIFACT}"),
        correlation_id: None,
        payload_json: "{}".into(),
    }
}

/// Create a **fresh, empty database** and hand back its URL.
///
/// Load-bearing, and the second attempt: the first version of these tests
/// restored into the same database it had dumped, so every assertion read back
/// rows that had never left. They passed with `pg_restore` replaced by `echo` —
/// checked by mutation, which is the only way that class of test bug is ever
/// found. Restoring somewhere else is what makes the assertions mean anything.
/// A restore target that drops itself, **including when a test panics**.
///
/// Cleanup on the success path only is cleanup that stops working exactly when
/// you need the machine to stay usable: a failing assertion is precisely when
/// the run leaks a database, and the next hundred runs leak a hundred more.
struct RestoreTarget {
    url: String,
    name: String,
    base: String,
    container: String,
}

impl Drop for RestoreTarget {
    fn drop(&mut self) {
        drop_database(&self.base, &self.container, &self.name);
    }
}

async fn fresh_database(base: &str, container: &str) -> RestoreTarget {
    let name = format!(
        "jarvis_restore_{}",
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let admin = swap_database(base, "postgres");
    let out = Command::new("docker")
        .args(["exec", "-i", container, "psql", &admin, "-c"])
        .arg(format!("CREATE DATABASE {name}"))
        .output()
        .expect("create the restore target");
    assert!(
        out.status.success(),
        "creating {name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    RestoreTarget {
        url: swap_database(base, &name),
        name,
        base: base.to_owned(),
        container: container.to_owned(),
    }
}

fn drop_database(base: &str, container: &str, name: &str) {
    let admin = swap_database(base, "postgres");
    let _ = Command::new("docker")
        .args(["exec", "-i", container, "psql", &admin, "-c"])
        .arg(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .output();
}

fn run_script(
    script: &str,
    args: &[&str],
    db: &str,
    artifacts: &Path,
    container: &str,
) -> std::process::Output {
    Command::new("bash")
        .arg(repo_root().join("infra/install").join(script))
        .args(args)
        .env("DATABASE_URL", db)
        .env("JARVIS__STORAGE__ARTIFACTS_ROOT", artifacts)
        .env("JARVIS_PG_CONTAINER", container)
        .output()
        .unwrap_or_else(|e| panic!("running {script}: {e}"))
}

/// **The feature, stated as a test.** Back up a house with an artifact in it,
/// restore into a *different* database and a *different* blob root, and the
/// artifact still resolves to its bytes.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_restored_house_still_has_its_artifacts(pool: PgPool) {
    let container = require_pg_tools!();
    let scratch = tempfile::tempdir().expect("tempdir");
    let source_blobs = scratch.path().join("source-artifacts");
    let restored_blobs = scratch.path().join("restored-artifacts");
    let backups = scratch.path().join("backups");

    // A house with one artifact: manifest in Postgres, bytes on disk.
    let blobs = FileBlobStore::new(&source_blobs);
    let address = blobs.put(BLOB).await.expect("store the blob");
    PgArtifactStore::new(pool.clone())
        .create_version(&manifest(), &audit())
        .await
        .expect("record the manifest");

    let db = url_of(&pool);
    let out = run_script(
        "backup.sh",
        &[backups.to_str().expect("path")],
        &db,
        &source_blobs,
        &container,
    );
    assert!(
        out.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let taken = one_backup(&backups);

    // Restore into a *different* database and a *different* blob root, so a
    // pass cannot be either source store quietly still being there.
    let target = fresh_database(&db, &container).await;
    let out = run_script(
        "restore.sh",
        &[taken.to_str().expect("path")],
        &target.url,
        &restored_blobs,
        &container,
    );
    assert!(
        out.status.success(),
        "restore failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The manifest survived — read from the restored database, not the one it
    // was dumped from.
    let restored_pool = PgPool::connect(&target.url)
        .await
        .expect("connect to the restored database");
    let restored = PgArtifactStore::new(restored_pool.clone())
        .latest(&ARTIFACT.parse().expect("ulid"))
        .await
        .expect("query")
        .expect("the artifact is still listed");
    // ...and so did the bytes it points at, which is the half that gets lost.
    let bytes = FileBlobStore::new(&restored_blobs)
        .get(&address)
        .await
        .expect("read")
        .expect("the artifact's blob must be readable after a restore");
    assert_eq!(bytes, BLOB);
    assert_eq!(*restored.sha256(), address);

    restored_pool.close().await;
}

/// A restore missing its blobs must **fail loudly**.
///
/// This is the failure mode worth engineering against: a database restored
/// without its CAS looks like a working house — every artifact listed — right
/// up until somebody opens one. Silence here would be worse than an error.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_restore_without_the_blobs_is_refused_not_half_done(pool: PgPool) {
    let container = require_pg_tools!();
    let scratch = tempfile::tempdir().expect("tempdir");
    let source_blobs = scratch.path().join("source-artifacts");
    let backups = scratch.path().join("backups");

    let blobs = FileBlobStore::new(&source_blobs);
    blobs.put(BLOB).await.expect("store the blob");
    PgArtifactStore::new(pool.clone())
        .create_version(&manifest(), &audit())
        .await
        .expect("record the manifest");

    let db = url_of(&pool);
    assert!(
        run_script(
            "backup.sh",
            &[backups.to_str().expect("path")],
            &db,
            &source_blobs,
            &container
        )
        .status
        .success()
    );
    let taken = one_backup(&backups);

    // Somebody restores the database and forgets the blobs — or the blob
    // archive was silently empty all along.
    std::fs::remove_dir_all(taken.join("blobs")).expect("drop the blobs");
    std::fs::create_dir_all(taken.join("blobs")).expect("empty blob dir");

    let target = fresh_database(&db, &container).await;
    let out = run_script(
        "restore.sh",
        &[taken.to_str().expect("path")],
        &target.url,
        &scratch.path().join("restored-artifacts"),
        &container,
    );
    assert!(
        !out.status.success(),
        "a restore whose artifacts cannot be read must fail, not report success"
    );
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("missing"),
        "the failure must name what is wrong, got: {said}"
    );
}

/// The rest of the house, not just its artifacts.
///
/// F10.2's acceptance is "timers still fire, devices are still paired,
/// automations still hold their creator" — and the last of those is the one
/// worth asserting by name. An automation borrows its creator's authority *at
/// fire time*: lose `created_by` in a restore and every automation in the house
/// either dies or, far worse, fires under whatever the column defaulted to.
///
/// Everything here is written through the **real repositories**, not INSERTs
/// composed for the test. A backup test that seeds its own rows its own way
/// proves the dump preserves rows the product never writes.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn a_restored_house_still_knows_its_devices_timers_and_automations(pool: PgPool) {
    let container = require_pg_tools!();
    let scratch = tempfile::tempdir().expect("tempdir");
    let source_blobs = scratch.path().join("source-artifacts");
    let backups = scratch.path().join("backups");
    std::fs::create_dir_all(&source_blobs).expect("blob root");

    let owner = Device {
        id: OWNER.parse().expect("ulid"),
        user_id: OWNER_USER.parse().expect("ulid"),
        name: "laptop".into(),
        token_hash: "hash-owner".into(),
        public_key: None,
        class: DeviceClass::OwnerUi,
        created_at: t0(),
        last_seen_at: None,
        revoked_at: None,
        revoked_reason: None,
    };
    PgIdentityStore::new(pool.clone())
        .pair_device("owner", &owner, &house_audit("device.paired"))
        .await
        .expect("pair the owner");

    PgAutomationStore::new(pool.clone())
        .create(
            &Automation::create(
                AUTOMATION.parse::<AutomationId>().expect("ulid"),
                AutomationName::new("evening lights").expect("name"),
                Trigger::DailyAt {
                    minutes_since_midnight: 420,
                },
                AutomationAction {
                    tool_id: ToolId::home_set_light(),
                    arguments: CanonicalValue::obj([
                        ("entity_id", CanonicalValue::str("light.kitchen")),
                        ("state", CanonicalValue::str("on")),
                    ]),
                },
                OWNER.parse().expect("device id"),
                t0(),
            ),
            &house_audit("automation.created"),
        )
        .await
        .expect("create the automation");

    let fire_at = t0() + Duration::from_secs(600);
    PgTimerStore::new(pool.clone())
        .create(
            &Timer::schedule(
                TIMER.parse::<TimerId>().expect("ulid"),
                TimerName::new("pasta").expect("name"),
                TimerKind::Countdown {
                    duration: Duration::from_secs(600),
                },
                fire_at,
                t0(),
            )
            .expect("schedulable"),
            &house_audit("timer.created"),
        )
        .await
        .expect("create the timer");

    let db = url_of(&pool);
    let out = run_script(
        "backup.sh",
        &[backups.to_str().expect("path")],
        &db,
        &source_blobs,
        &container,
    );
    assert!(
        out.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let taken = one_backup(&backups);

    let target = fresh_database(&db, &container).await;
    let out = run_script(
        "restore.sh",
        &[taken.to_str().expect("path")],
        &target.url,
        &scratch.path().join("restored-artifacts"),
        &container,
    );
    assert!(
        out.status.success(),
        "restore failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let pool = PgPool::connect(&target.url)
        .await
        .expect("connect to the restored database");

    // Devices are still paired — a restored house you cannot log into is not
    // restored.
    let devices = PgIdentityStore::new(pool.clone())
        .list_devices()
        .await
        .expect("list");
    assert_eq!(devices.len(), 1, "the owner device must survive a restore");
    assert_eq!(devices[0].class, DeviceClass::OwnerUi);

    // The automation still holds its creator, and is still enabled.
    let automations = PgAutomationStore::new(pool.clone())
        .list_all()
        .await
        .expect("list");
    assert_eq!(automations.len(), 1);
    assert_eq!(
        automations[0].created_by().to_string(),
        OWNER,
        "an automation that loses its creator fires under the wrong authority, \
         or not at all"
    );
    assert!(automations[0].is_enabled());

    // The timer is still live and still due at the moment it was set for.
    // `list_live` is the query jarvisd runs at startup to re-arm its schedule,
    // so this is the difference between a restored timer that rings and a row
    // that merely exists.
    let live = PgTimerStore::new(pool)
        .list_live()
        .await
        .expect("list live");
    assert_eq!(live.len(), 1, "a restored timer must still be armed");
    assert_eq!(live[0].fire_at(), fire_at);
}

/// Restoring over a live house is refused unless it is asked for explicitly.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn restoring_over_an_existing_house_needs_force(pool: PgPool) {
    let container = require_pg_tools!();
    let scratch = tempfile::tempdir().expect("tempdir");
    let source_blobs = scratch.path().join("source-artifacts");
    let backups = scratch.path().join("backups");
    FileBlobStore::new(&source_blobs)
        .put(BLOB)
        .await
        .expect("store");

    let db = url_of(&pool);
    assert!(
        run_script(
            "backup.sh",
            &[backups.to_str().expect("path")],
            &db,
            &source_blobs,
            &container
        )
        .status
        .success()
    );
    let taken = one_backup(&backups);

    // The pool's database is migrated, so it is emphatically not empty.
    let out = run_script(
        "restore.sh",
        &[taken.to_str().expect("path")],
        &db,
        &scratch.path().join("restored-artifacts"),
        &container,
    );
    assert!(
        !out.status.success(),
        "restoring over a populated database must be refused without --force"
    );
}
