//! F10.3: upgrade across a migration with live data, then actually roll back.
//!
//! The feature's acceptance is "the documented rollback path, **executed**" —
//! so this executes it, using `infra/install/restore.sh`, the same script the
//! docs tell an operator to run. A rollback procedure that has only ever been
//! written down is a procedure nobody knows the state of.
//!
//! # What is actually being claimed
//!
//! There are no `down` migrations in this project — 21 forward-only files, not
//! one `.down.sql`. So "rollback" is not `sqlx migrate revert`; it is restoring
//! the backup taken immediately before the upgrade. That is a real position
//! with a real cost (everything written after the backup is lost), and it is
//! only honest if the restore genuinely returns a working house. These tests
//! are what make it honest.
//!
//! They skip when no Postgres container is reachable, like `backup_restore.rs`.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jarvis_application::ports::IdentityStore;
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::identity::{Device, DeviceClass};
use jarvis_infra::identity::PgIdentityStore;
use sqlx::PgPool;

const OWNER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA1";
const OWNER_USER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";

fn pg_container() -> Option<String> {
    let name =
        std::env::var("JARVIS_PG_CONTAINER").unwrap_or_else(|_| "jarvis-dev-postgres-1".to_owned());
    Command::new("docker")
        .args(["exec", &name, "pg_dump", "--version"])
        .output()
        .is_ok_and(|o| o.status.success())
        .then_some(name)
}

macro_rules! require_pg {
    () => {
        match pg_container() {
            Some(name) => name,
            None => {
                eprintln!("SKIP: no reachable Postgres container (see backup_restore.rs).");
                return;
            }
        }
    };
}

fn repo_root() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

fn t0() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

fn url_of(pool: &PgPool) -> String {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let opts = pool.connect_options();
    let db = opts.get_database().expect("test database name");
    swap_database(&base, db)
}

fn swap_database(url: &str, db: &str) -> String {
    match url.rfind('/') {
        Some(i) => format!("{}/{db}", &url[..i]),
        None => url.to_owned(),
    }
}

/// A restore target that drops itself even when a test panics.
struct Target {
    url: String,
    name: String,
    base: String,
    container: String,
}

impl Drop for Target {
    fn drop(&mut self) {
        let admin = swap_database(&self.base, "postgres");
        let _ = Command::new("docker")
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
        "jarvis_rollback_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let admin = swap_database(base, "postgres");
    let out = Command::new("docker")
        .args(["exec", "-i", container, "psql", &admin, "-c"])
        .arg(format!("CREATE DATABASE {name}"))
        .output()
        .expect("create database");
    assert!(
        out.status.success(),
        "creating {name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Target {
        url: swap_database(base, &name),
        name,
        base: base.to_owned(),
        container: container.to_owned(),
    }
}

fn script(name: &str, args: &[&str], db: &str, artifacts: &Path, container: &str) -> bool {
    let out = Command::new("bash")
        .arg(repo_root().join("infra/install").join(name))
        .args(args)
        .env("DATABASE_URL", db)
        .env("JARVIS__STORAGE__ARTIFACTS_ROOT", artifacts)
        .env("JARVIS_PG_CONTAINER", container)
        .output()
        .unwrap_or_else(|e| panic!("running {name}: {e}"));
    if !out.status.success() {
        eprintln!(
            "{name} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out.status.success()
}

fn owner() -> Device {
    Device {
        id: OWNER.parse().expect("ulid"),
        user_id: OWNER_USER.parse().expect("ulid"),
        name: "the laptop".into(),
        token_hash: "hash-owner".into(),
        public_key: None,
        class: DeviceClass::OwnerUi,
        created_at: t0(),
        last_seen_at: None,
        revoked_at: None,
        revoked_reason: None,
    }
}

fn audit(event_type: &str) -> AuditEvent {
    AuditEvent {
        occurred_at: t0(),
        actor: format!("device:{OWNER}"),
        event_type: event_type.to_owned(),
        target: format!("device:{OWNER}"),
        correlation_id: None,
        payload_json: "{}".into(),
    }
}

/// **The claim, executed.** A house with a paired device, fully migrated, backed
/// up and then restored — and the device is still paired afterwards.
///
/// `#[sqlx::test]` hands us a database with the *whole* migration stream already
/// applied, which is precisely the post-upgrade state an operator rolls back
/// from. The rollback target is a separate database, so nothing here can pass
/// because the original was quietly still there.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn the_documented_rollback_path_returns_a_working_house(pool: PgPool) {
    let container = require_pg!();
    let scratch = tempfile::tempdir().expect("tempdir");
    let artifacts = scratch.path().join("artifacts");
    let backups = scratch.path().join("backups");
    std::fs::create_dir_all(&artifacts).expect("artifact root");

    PgIdentityStore::new(pool.clone())
        .pair_device("owner", &owner(), &audit("device.paired"))
        .await
        .expect("pair the owner");

    let db = url_of(&pool);
    assert!(
        script(
            "backup.sh",
            &[backups.to_str().expect("path")],
            &db,
            &artifacts,
            &container
        ),
        "the rollback point must be takeable — an upgrade without one is a gamble"
    );
    let taken = std::fs::read_dir(&backups)
        .expect("backups")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .next()
        .expect("one backup");

    // The rollback, exactly as docs/09 §3a instructs.
    let target = fresh_database(&db, &container);
    assert!(
        script(
            "restore.sh",
            &[taken.to_str().expect("path")],
            &target.url,
            &artifacts,
            &container
        ),
        "the documented rollback path must succeed"
    );

    let restored = PgPool::connect(&target.url)
        .await
        .expect("connect to the rolled-back database");
    let devices = PgIdentityStore::new(restored.clone())
        .list_devices()
        .await
        .expect("list devices");
    assert_eq!(
        devices.len(),
        1,
        "a rollback that loses the owner's pairing has locked them out of their own house"
    );
    assert_eq!(devices[0].class, DeviceClass::OwnerUi);
    restored.close().await;
}

/// The schema really is fully applied after an upgrade — the state the rollback
/// above starts from.
///
/// Weak-looking, load-bearing: if `MIGRATOR` silently applied fewer migrations
/// than the tree contains, every other test here would still pass while the
/// upgrade path was broken.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn an_upgraded_database_has_every_migration_applied(pool: PgPool) {
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .expect("read the migration ledger");

    let on_disk = std::fs::read_dir(repo_root().join("migrations"))
        .expect("migrations directory")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "sql"))
        .count() as i64;

    assert_eq!(
        applied, on_disk,
        "every migration in the tree must be applied after an upgrade"
    );
}

/// **There is no `down` migration, and the docs must not imply one.**
///
/// This is the test that keeps F10.3's central claim honest. The moment someone
/// adds a `.down.sql`, the documented rollback story ("restore from backup")
/// stops being the whole truth and this fails, forcing the docs to be updated
/// with it rather than drifting quietly out of date.
#[test]
fn rollback_is_restore_because_there_are_no_down_migrations() {
    let down: Vec<_> = std::fs::read_dir(repo_root().join("migrations"))
        .expect("migrations directory")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".down.sql"))
        .collect();

    assert!(
        down.is_empty(),
        "found down migrations {down:?} — docs/09 §3a documents rollback as \
         'restore from backup' on the grounds that reverting is impossible. \
         Update that section (and update.sh's header) before adding these."
    );
}
