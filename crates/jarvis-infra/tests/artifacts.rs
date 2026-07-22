//! F3a.2 acceptance — artifact manifest persistence against live Postgres
//! (FR-08, docs/04 §4, invariant #6). Proves: a manifest round-trips by exact
//! version and as "latest"; versions are an append-only chain; a duplicate
//! version is a conflict; the `artifact.created` event lands in the audit chain
//! in the same transaction; and the DB itself refuses to mutate or delete a
//! stored manifest (immutable manifests, docs/04 §4).

use std::time::SystemTime;

use jarvis_application::ports::{ArtifactStore, RepositoryError};
use jarvis_domain::artifact::{
    ArtifactContent, ArtifactKind, ArtifactManifest, ArtifactSource, ArtifactVersion,
    BuildProvenance, MediaType,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::grants::Sha256;
use jarvis_domain::ids::{ArtifactId, RunId};
use jarvis_domain::location::Sensitivity;
use jarvis_infra::artifacts::PgArtifactStore;
use jarvis_infra::audit::verify_chain;
use sqlx::PgPool;

const ARTIFACT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
const RUN2: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB2";

fn artifact_id() -> ArtifactId {
    ARTIFACT.parse().unwrap()
}

fn sha(byte: u8) -> Sha256 {
    Sha256::from_bytes([byte; 32])
}

fn content(sha_byte: u8) -> ArtifactContent {
    ArtifactContent {
        sha256: sha(sha_byte),
        media_type: "text/markdown".parse::<MediaType>().unwrap(),
        kind: ArtifactKind::MarkdownHtml,
        sources: vec![
            ArtifactSource::Run(RUN.parse::<RunId>().unwrap()),
            ArtifactSource::Web {
                url: "https://en.wikipedia.org/wiki/Mitochondrion".to_owned(),
            },
        ],
        sensitivity: Sensitivity::Sensitive,
        build: BuildProvenance::none(),
        capabilities: vec!["artifact.read-own-data".parse().unwrap()],
    }
}

fn manifest_v1() -> ArtifactManifest {
    ArtifactManifest::initial(artifact_id(), RUN.parse::<RunId>().unwrap(), content(0xAA))
}

fn created_event() -> AuditEvent {
    AuditEvent {
        occurred_at: SystemTime::now(),
        actor: format!("run:{RUN}"),
        event_type: "artifact.created".to_owned(),
        target: format!("artifact:{ARTIFACT}"),
        correlation_id: Some(RUN.to_owned()),
        payload_json: serde_json::json!({ "version": 1, "kind": "markdown_html" }).to_string(),
    }
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn create_then_get_and_latest_round_trip(pool: PgPool) {
    let store = PgArtifactStore::new(pool);
    let m = manifest_v1();
    store.create_version(&m, &created_event()).await.unwrap();

    let got = store
        .get(&artifact_id(), ArtifactVersion::FIRST)
        .await
        .unwrap()
        .expect("version 1 should exist");
    assert_eq!(got, m, "manifest round-trips byte-for-byte");
    // Full provenance survived the round-trip.
    assert_eq!(got.sha256(), &sha(0xAA));
    assert_eq!(got.sensitivity(), Sensitivity::Sensitive);
    assert_eq!(got.sources().len(), 2);
    assert_eq!(got.capabilities().len(), 1);

    let latest = store.latest(&artifact_id()).await.unwrap().unwrap();
    assert_eq!(latest, m);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn versions_form_an_append_only_chain(pool: PgPool) {
    let store = PgArtifactStore::new(pool);
    let v1 = manifest_v1();
    store.create_version(&v1, &created_event()).await.unwrap();

    let v2 = v1
        .next_version(RUN2.parse::<RunId>().unwrap(), content(0xBB))
        .unwrap();
    store.create_version(&v2, &created_event()).await.unwrap();

    // latest resolves to the highest version.
    let latest = store.latest(&artifact_id()).await.unwrap().unwrap();
    assert_eq!(latest.version().get(), 2);
    assert_eq!(latest.sha256(), &sha(0xBB));

    // list_versions is the whole chain, oldest first.
    let chain = store.list_versions(&artifact_id()).await.unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].version().get(), 1);
    assert_eq!(chain[1].version().get(), 2);
    // v1 is unchanged by v2's creation (immutability at the read layer).
    assert_eq!(chain[0], v1);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn duplicate_version_is_a_conflict(pool: PgPool) {
    let store = PgArtifactStore::new(pool);
    let m = manifest_v1();
    store.create_version(&m, &created_event()).await.unwrap();

    let err = store
        .create_version(&m, &created_event())
        .await
        .expect_err("re-creating the same version must conflict");
    assert!(
        matches!(err, RepositoryError::Conflict(_)),
        "expected Conflict, got {err:?}"
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn create_writes_the_audit_event_in_the_same_transaction(pool: PgPool) {
    let store = PgArtifactStore::new(pool.clone());
    store
        .create_version(&manifest_v1(), &created_event())
        .await
        .unwrap();

    // The chain verifies and contains exactly the one artifact.created event.
    let mut conn = pool.acquire().await.unwrap();
    let count = verify_chain(&mut conn).await.unwrap();
    assert_eq!(count, 1, "the create must have appended one audit event");
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn unknown_artifact_reads_are_none(pool: PgPool) {
    let store = PgArtifactStore::new(pool);
    let unknown: ArtifactId = "01ARZ3NDEKTSV4RRFFQ69G5FZZ".parse().unwrap();
    assert!(store.latest(&unknown).await.unwrap().is_none());
    assert!(
        store
            .get(&unknown, ArtifactVersion::FIRST)
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.list_versions(&unknown).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn stored_manifests_are_immutable_at_the_db_level(pool: PgPool) {
    let store = PgArtifactStore::new(pool.clone());
    store
        .create_version(&manifest_v1(), &created_event())
        .await
        .unwrap();

    // The append-only trigger (migration 0010) refuses both UPDATE and DELETE,
    // so provenance cannot be rewritten even by raw SQL.
    let update = sqlx::query("UPDATE artifacts.manifests SET media_type = 'text/plain'")
        .execute(&pool)
        .await;
    assert!(update.is_err(), "manifests must not be updatable");

    let delete = sqlx::query("DELETE FROM artifacts.manifests")
        .execute(&pool)
        .await;
    assert!(delete.is_err(), "manifests must not be deletable");

    let truncate = sqlx::query("TRUNCATE artifacts.manifests")
        .execute(&pool)
        .await;
    assert!(truncate.is_err(), "manifests must not be truncatable");

    // The row is still there and intact.
    let latest = store.latest(&artifact_id()).await.unwrap().unwrap();
    assert_eq!(latest.media_type().as_str(), "text/markdown");
}
