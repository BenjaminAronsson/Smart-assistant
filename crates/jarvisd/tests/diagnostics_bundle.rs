//! F10.4: the bundle carries the diagnosis and nothing you would regret sending.
//!
//! The feature's acceptance, verbatim: "the bundle contains the diagnostic
//! fields; a seeded secret, transcript and message body appear **nowhere** in
//! it."
//!
//! Both halves matter and the second is the harder one. A bundle nobody dares
//! share is useless — the owner sits on it, describes the symptom from memory,
//! and everyone guesses. So the negative test is not a nicety; it is what makes
//! the artifact usable at all.
//!
//! # Why the negative test searches the whole serialization
//!
//! It does not assert "field X is absent". It serializes the entire bundle and
//! greps the bytes for the seeded strings. Asserting on named fields would only
//! ever test the fields I thought of, and the leak that matters is the one added
//! next year by someone who never read this file.

use std::sync::Arc;

use jarvis_application::policy::ToolRegistry;
use jarvisd::diagnostics::DiagnosticsApi;
use sqlx::PgPool;

/// Distinctive enough that a match cannot be coincidence, and shaped like the
/// three things that must never travel: a credential, a spoken transcript, and
/// a typed message body.
const SECRET: &str = "SEEDED-KEYRING-VALUE-hunter2-do-not-leak";
const TRANSCRIPT: &str = "SEEDED-TRANSCRIPT-turn-the-bedroom-lights-off";
const MESSAGE_BODY: &str = "SEEDED-MESSAGE-my-mother-is-in-hospital";

fn adapters() -> Arc<
    std::sync::RwLock<std::collections::BTreeMap<String, jarvis_contracts::health::AdapterHealth>>,
> {
    let mut map = std::collections::BTreeMap::new();
    map.insert(
        "database".to_owned(),
        jarvis_contracts::health::AdapterHealth {
            state: jarvis_contracts::health::AdapterState::Up,
            detail: None,
        },
    );
    map.insert(
        "web-search".to_owned(),
        jarvis_contracts::health::AdapterHealth {
            state: jarvis_contracts::health::AdapterState::Disabled,
            detail: Some("set [integrations.web_search]".to_owned()),
        },
    );
    Arc::new(std::sync::RwLock::new(map))
}

fn api(pool: PgPool) -> DiagnosticsApi {
    DiagnosticsApi::new(pool, Arc::new(ToolRegistry::new()), adapters())
}

/// Put all three sensitive things into the database, in the columns they really
/// live in.
async fn seed_sensitive(pool: &PgPool) {
    let session = "01ARZ3NDEKTSV4RRFFQ69G5FA1";
    let user = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
    let device = "01ARZ3NDEKTSV4RRFFQ69G5FB2";

    sqlx::query("INSERT INTO identity.users (id, name, created_at) VALUES ($1, $2, now())")
        .bind(user)
        // A device/user name can be personal; seed it as sensitive too.
        .bind("Mum's iPad")
        .execute(pool)
        .await
        .expect("seed user");

    sqlx::query(
        "INSERT INTO identity.devices (id, user_id, name, token_hash, scopes, created_at, device_class) \
         VALUES ($1, $2, $3, $4, '{}', now(), 'owner-ui')",
    )
    .bind(device)
    .bind(user)
    .bind("Mum's iPad")
    .bind(SECRET) // stands in for a credential at rest
    .execute(pool)
    .await
    .expect("seed device");

    sqlx::query(
        "INSERT INTO conversation.sessions (id, title, status, created_at, updated_at) \
         VALUES ($1, $2, 'active', now(), now())",
    )
    .bind(session)
    .bind(TRANSCRIPT) // a session title can carry what was said aloud
    .execute(pool)
    .await
    .expect("seed session");

    sqlx::query(
        "INSERT INTO conversation.messages (id, session_id, role, content, created_at) \
         VALUES ($1, $2, 'user', $3::jsonb, now())",
    )
    .bind("01ARZ3NDEKTSV4RRFFQ69G5FB3")
    .bind(session)
    .bind(serde_json::json!({ "text": MESSAGE_BODY }).to_string())
    .execute(pool)
    .await
    .expect("seed message");

    sqlx::query(
        "INSERT INTO audit.audit_events (occurred_at, actor, event_type, target, payload, prev_hash, hash) \
         VALUES (now(), $1, 'message.created', $2, $3::jsonb, '', 'h')",
    )
    .bind(format!("device:{device}"))
    .bind(format!("session:{session}"))
    // The payload is the audit table's most dangerous column: it is JSON the
    // daemon writes, and it is where a body would leak if anything ever copied
    // one in.
    .bind(serde_json::json!({ "body": MESSAGE_BODY, "token": SECRET }).to_string())
    .execute(pool)
    .await
    .expect("seed audit");
}

/// **Half one:** the bundle actually carries a diagnosis.
///
/// A bundle that leaks nothing because it says nothing would pass the negative
/// test perfectly, which is why this comes first.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn the_bundle_carries_what_a_diagnosis_needs(pool: PgPool) {
    seed_sensitive(&pool).await;
    let bundle = api(pool).bundle().await;

    assert!(!bundle.version.is_empty());
    assert!(
        !bundle.generated_at.is_empty(),
        "a bundle read days later must date itself"
    );

    // Migration state — the first thing to check after a bad upgrade.
    assert!(bundle.migrations.applied > 0);
    assert!(bundle.migrations.latest_version.is_some());

    // Capability readiness, including *why* something is off.
    let web = bundle
        .adapters
        .iter()
        .find(|a| a.name == "web-search")
        .expect("web-search reported");
    assert_eq!(web.state, "disabled");
    assert_eq!(web.detail.as_deref(), Some("set [integrations.web_search]"));

    // Shapes, not contents.
    assert!(
        bundle
            .audit_shapes
            .iter()
            .any(|s| s.event_type == "message.created" && s.count == 1),
        "audit shapes must report what happened: {:?}",
        bundle.audit_shapes
    );

    // Counts of the things whose contents must never appear.
    assert_eq!(bundle.device_count, 1);
    assert_eq!(bundle.session_count, 1);
    assert_eq!(bundle.message_count, 1);
    assert!(bundle.resources.rss_kib.is_some_and(|kib| kib > 0));
}

/// **Half two, the one that makes it sendable.** Nothing seeded appears
/// anywhere in the serialized bundle.
///
/// Whole-serialization search rather than per-field assertions: naming fields
/// would only ever test the ones I thought of, and the leak that matters is the
/// field somebody adds later without reading this.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn no_secret_transcript_or_message_body_appears_anywhere(pool: PgPool) {
    seed_sensitive(&pool).await;
    let bundle = api(pool).bundle().await;

    let json = serde_json::to_string(&bundle).expect("bundle serializes");

    for (what, needle) in [
        ("a credential", SECRET),
        ("a spoken transcript", TRANSCRIPT),
        ("a message body", MESSAGE_BODY),
        ("a personal device name", "Mum's iPad"),
    ] {
        assert!(
            !json.contains(needle),
            "{what} reached the diagnostics bundle. A bundle nobody dares send is \
             useless, so this is not a cosmetic failure.\n\nfound {needle:?} in:\n{json}"
        );
    }
}

/// The seeds really are in the database — otherwise the test above passes by
/// finding nothing, and would keep passing if redaction were removed entirely.
///
/// This is the check that stops the negative test being vacuous, which is the
/// same trap that made F10.2's first backup tests worthless.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn the_negative_test_is_not_vacuous(pool: PgPool) {
    seed_sensitive(&pool).await;

    let body: String =
        sqlx::query_scalar("SELECT content::text FROM conversation.messages LIMIT 1")
            .fetch_one(&pool)
            .await
            .expect("the message body is really stored");
    assert!(
        body.contains(MESSAGE_BODY),
        "the message body must genuinely be in the database: {body}"
    );

    let payload: String =
        sqlx::query_scalar("SELECT payload::text FROM audit.audit_events LIMIT 1")
            .fetch_one(&pool)
            .await
            .expect("the audit payload is really stored");
    assert!(
        payload.contains(SECRET) && payload.contains(MESSAGE_BODY),
        "the audit payload must genuinely contain both, or the redaction test proves nothing"
    );
}
