//! M4 memory review/edit/forget surface through the production router
//! (FR-16, docs/05 §1/§7, "memory forget verified" exit evidence).
//!
//! Real `PgIdentityStore` + `PgMemoryStore` against live Postgres, driven
//! through the actual axum router (auth middleware, route table, DTO
//! mapping) — the surface the Angular HUD calls, not just the storage layer
//! (which `jarvis-infra/tests/memory.rs` already covers). Covered: auth
//! required on every route, list scoping to the caller's own user, the
//! query/layer filters and the query length bound, patch's field updates and
//! its 404/422/400 error paths, and forget's 204 + genuine removal
//! (including the cascaded embedding row) with 404 on an unknown or
//! cross-user id — never a silent success and never a cross-user leak.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use jarvis_application::ports::{EmbeddedMemory, EmbeddedMemoryStore, MemoryStore};
use jarvis_domain::ids::{MemoryId, UserId};
use jarvis_domain::location::Sensitivity;
use jarvis_domain::memory::{Memory, MemoryLayer, MemoryScope, MemorySource, RetentionRule};
use jarvis_infra::memory::PgMemoryStore;
use jarvisd::api::{AppState, Wiring, router_with};
use jarvisd::auth::AuthState;
use jarvisd::memories::MemoryApi;
use sqlx::PgPool;
use tower::ServiceExt;

const T0: u64 = 1_700_000_000;

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// A ULID that is well-formed but belongs to no paired device — enough to
/// prove ownership scoping without needing a second real pairing (the
/// memories table carries no FK into `identity`, by design: docs/02 §7).
const OTHER_USER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FZZ";

fn other_user() -> UserId {
    OTHER_USER.parse().unwrap()
}

fn mid(raw: &str) -> MemoryId {
    raw.parse().unwrap()
}

#[allow(clippy::too_many_arguments)]
fn memory_for(id: &str, user: &UserId, text: &str, pinned: bool, now: SystemTime) -> Memory {
    Memory::new(
        mid(id),
        user.clone(),
        MemoryLayer::Semantic,
        text.to_owned(),
        MemorySource::Explicit,
        MemoryScope::User,
        RetentionRule::UntilForgotten,
        0.8,
        Sensitivity::Normal,
        pinned,
        now,
    )
    .expect("valid test memory")
}

fn audit(memory: &Memory, event_type: &str) -> jarvis_domain::audit::AuditEvent {
    jarvis_domain::audit::AuditEvent {
        occurred_at: memory.updated_at,
        actor: "device:test".to_owned(),
        event_type: event_type.to_owned(),
        target: format!("memory:{}", memory.id.as_str()),
        correlation_id: None,
        payload_json: format!(r#"{{"memoryId":"{}"}}"#, memory.id.as_str()),
    }
}

fn embedding(seed: f32) -> EmbeddedMemory {
    EmbeddedMemory {
        model_id: "bge-small-en-v1.5".to_owned(),
        dimensions: 384,
        embedding: (0..384).map(|i| seed + (i as f32) * 0.001).collect(),
    }
}

// --- harness --------------------------------------------------------------

struct Harness {
    app: Router,
    token: String,
    user_id: UserId,
    store: Arc<PgMemoryStore>,
    pool: PgPool,
}

impl Harness {
    async fn get(&self, path: &str) -> (StatusCode, serde_json::Value) {
        self.send(Request::get(path).body(Body::empty()).unwrap())
            .await
    }

    async fn patch(&self, path: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        self.send(
            Request::patch(path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    async fn delete(&self, path: &str) -> (StatusCode, serde_json::Value) {
        self.send(Request::delete(path).body(Body::empty()).unwrap())
            .await
    }

    async fn send(&self, mut request: Request<Body>) -> (StatusCode, serde_json::Value) {
        request.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {}", self.token).parse().unwrap(),
        );
        self.send_raw(request).await
    }

    async fn send_raw(&self, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }
}

async fn harness(pool: PgPool) -> Harness {
    let identity = Arc::new(jarvis_infra::identity::PgIdentityStore::new(pool.clone()));
    let auth = AuthState::bootstrap(identity).await;
    let code = auth.current_pairing_code().unwrap();

    let store = Arc::new(PgMemoryStore::new(pool.clone()));
    let api = MemoryApi::new(store.clone());

    let app = router_with(
        AppState::new().with_auth(auth),
        Wiring {
            memories: Some(api),
            ..Wiring::default()
        },
    );

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/pair")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"pairingCode":"{code}","deviceName":"laptop"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let token = body["deviceToken"].as_str().unwrap().to_owned();
    let device_id = body["deviceId"].as_str().unwrap().to_owned();

    let user_id: String = sqlx::query_scalar("SELECT user_id FROM identity.devices WHERE id = $1")
        .bind(&device_id)
        .fetch_one(&pool)
        .await
        .expect("paired device row");
    let user_id: UserId = user_id.parse().expect("valid ULID");

    Harness {
        app,
        token,
        user_id,
        store,
        pool,
    }
}

// --- tests ------------------------------------------------------------

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn every_memory_route_requires_a_device_token(pool: PgPool) {
    let h = harness(pool).await;
    let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let unauthenticated = [
        Request::get("/api/v1/memories")
            .body(Body::empty())
            .unwrap(),
        Request::patch(format!("/api/v1/memories/{id}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"pinned":true}"#))
            .unwrap(),
        Request::delete(format!("/api/v1/memories/{id}"))
            .body(Body::empty())
            .unwrap(),
    ];
    for request in unauthenticated {
        let path = request.uri().path().to_owned();
        let (status, _) = h.send_raw(request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} must require auth");
    }
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn list_is_scoped_to_the_authenticated_user_only(pool: PgPool) {
    let h = harness(pool).await;
    let mine = memory_for(
        "01ARZ3NDEKTSV4RRFFQ69G5FB0",
        &h.user_id,
        "likes oat milk",
        false,
        at(T0),
    );
    let theirs = memory_for(
        "01ARZ3NDEKTSV4RRFFQ69G5FB1",
        &other_user(),
        "likes soy milk",
        false,
        at(T0),
    );
    h.store
        .create(&mine, &audit(&mine, "memory.created"))
        .await
        .unwrap();
    h.store
        .create(&theirs, &audit(&theirs, "memory.created"))
        .await
        .unwrap();

    let (status, body) = h.get("/api/v1/memories").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["01ARZ3NDEKTSV4RRFFQ69G5FB0"],
        "only the caller's own memory is listed, never another user's"
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn list_filters_by_query_and_layer(pool: PgPool) {
    let h = harness(pool).await;
    let working = Memory::new(
        mid("01ARZ3NDEKTSV4RRFFQ69G5FB0"),
        h.user_id.clone(),
        MemoryLayer::Working,
        "likes oat milk".to_owned(),
        MemorySource::Explicit,
        MemoryScope::User,
        RetentionRule::UntilForgotten,
        0.8,
        Sensitivity::Normal,
        false,
        at(T0),
    )
    .unwrap();
    let semantic = Memory::new(
        mid("01ARZ3NDEKTSV4RRFFQ69G5FB1"),
        h.user_id.clone(),
        MemoryLayer::Semantic,
        "allergic to peanuts".to_owned(),
        MemorySource::Explicit,
        MemoryScope::User,
        RetentionRule::UntilForgotten,
        0.8,
        Sensitivity::Normal,
        false,
        at(T0 + 1),
    )
    .unwrap();
    for item in [&working, &semantic] {
        h.store
            .create(item, &audit(item, "memory.created"))
            .await
            .unwrap();
    }

    let (status, body) = h.get("/api/v1/memories?layer=working").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["01ARZ3NDEKTSV4RRFFQ69G5FB0"]);

    let (status, body) = h.get("/api/v1/memories?query=peanuts").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["01ARZ3NDEKTSV4RRFFQ69G5FB1"]);

    // Case-insensitive substring match (ILIKE), not an exact match.
    let (status, body) = h.get("/api/v1/memories?query=OAT").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["memories"].as_array().unwrap().len(), 1);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn list_query_length_is_bounded(pool: PgPool) {
    let h = harness(pool).await;

    let (status, body) = h.get("/api/v1/memories?query=a").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "1 char is under the floor: {body}"
    );
    assert_eq!(body["code"], "validation.failed");

    let (status, body) = h.get("/api/v1/memories?query=ab").await;
    assert_eq!(status, StatusCode::OK, "2 chars is the floor: {body}");

    let too_long = "a".repeat(129);
    let (status, body) = h.get(&format!("/api/v1/memories?query={too_long}")).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "129 bytes is over the 128 ceiling: {body}"
    );
    assert_eq!(body["code"], "validation.failed");

    let exactly_128 = "a".repeat(128);
    let (status, body) = h
        .get(&format!("/api/v1/memories?query={exactly_128}"))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "128 bytes is exactly the ceiling: {body}"
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn patch_updates_text_retention_and_pinned(pool: PgPool) {
    let h = harness(pool).await;
    let item = memory_for(
        "01ARZ3NDEKTSV4RRFFQ69G5FB0",
        &h.user_id,
        "likes tea",
        false,
        at(T0),
    );
    h.store
        .create(&item, &audit(&item, "memory.created"))
        .await
        .unwrap();

    let (status, body) = h
        .patch(
            "/api/v1/memories/01ARZ3NDEKTSV4RRFFQ69G5FB0",
            serde_json::json!({
                "text": "likes coffee now",
                "pinned": true,
                "retention": "session"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["text"], "likes coffee now");
    assert_eq!(body["pinned"], true);
    assert_eq!(body["retention"], "session");

    // The store itself reflects the update, not just the response DTO.
    let fetched = h
        .store
        .get(&h.user_id, &mid("01ARZ3NDEKTSV4RRFFQ69G5FB0"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.text, "likes coffee now");
    assert!(fetched.pinned);
    assert_eq!(fetched.retention, RetentionRule::Session);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn patch_unknown_id_is_404(pool: PgPool) {
    let h = harness(pool).await;
    let (status, body) = h
        .patch(
            "/api/v1/memories/01ARZ3NDEKTSV4RRFFQ69G5FAV",
            serde_json::json!({ "pinned": true }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "resource.not_found");
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn patch_cross_user_id_is_404_not_a_cross_user_edit(pool: PgPool) {
    let h = harness(pool).await;
    let theirs = memory_for(
        "01ARZ3NDEKTSV4RRFFQ69G5FB0",
        &other_user(),
        "likes soy milk",
        false,
        at(T0),
    );
    h.store
        .create(&theirs, &audit(&theirs, "memory.created"))
        .await
        .unwrap();

    let (status, body) = h
        .patch(
            "/api/v1/memories/01ARZ3NDEKTSV4RRFFQ69G5FB0",
            serde_json::json!({ "text": "hijacked" }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "resource.not_found");

    let untouched = h
        .store
        .get(&other_user(), &mid("01ARZ3NDEKTSV4RRFFQ69G5FB0"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        untouched.text, "likes soy milk",
        "the other user's memory was not edited"
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn patch_rejects_secret_shaped_text_with_its_own_code(pool: PgPool) {
    let h = harness(pool).await;
    let item = memory_for(
        "01ARZ3NDEKTSV4RRFFQ69G5FB0",
        &h.user_id,
        "likes tea",
        false,
        at(T0),
    );
    h.store
        .create(&item, &audit(&item, "memory.created"))
        .await
        .unwrap();

    let (status, body) = h
        .patch(
            "/api/v1/memories/01ARZ3NDEKTSV4RRFFQ69G5FB0",
            serde_json::json!({ "text": "my password is hunter2" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "memory.secret_rejected");

    // Rejected content never landed.
    let fetched = h
        .store
        .get(&h.user_id, &mid("01ARZ3NDEKTSV4RRFFQ69G5FB0"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.text, "likes tea");
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn patch_malformed_id_is_400_not_500(pool: PgPool) {
    let h = harness(pool).await;
    let (status, body) = h
        .patch(
            "/api/v1/memories/not-a-ulid",
            serde_json::json!({ "pinned": true }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "validation.failed");
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn forgetting_an_existing_memory_is_204_and_it_is_genuinely_gone(pool: PgPool) {
    let h = harness(pool).await;
    let item = memory_for(
        "01ARZ3NDEKTSV4RRFFQ69G5FB0",
        &h.user_id,
        "the wifi network is HomeNet",
        false,
        at(T0),
    );
    h.store
        .create_embedded(&item, &embedding(0.1), &audit(&item, "memory.created"))
        .await
        .unwrap();
    let embedding_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memory.embeddings WHERE memory_id = $1")
            .bind(item.id.as_str())
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(
        embedding_count, 1,
        "setup sanity: the embedding exists before forget"
    );

    let (status, _) = h
        .delete("/api/v1/memories/01ARZ3NDEKTSV4RRFFQ69G5FB0")
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Gone from the store directly...
    assert_eq!(
        h.store
            .get(&h.user_id, &mid("01ARZ3NDEKTSV4RRFFQ69G5FB0"))
            .await
            .unwrap(),
        None
    );
    // ...and from the list a real client would see...
    let (_, listed) = h.get("/api/v1/memories").await;
    assert!(listed["memories"].as_array().unwrap().is_empty());
    // ...and the cascaded embedding row is gone too (docs/02 §7).
    let embedding_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memory.embeddings WHERE memory_id = $1")
            .bind(item.id.as_str())
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(
        embedding_count, 0,
        "forget cascades to the derived embedding"
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn forgetting_an_unknown_or_already_absent_id_is_404_never_a_silent_success(pool: PgPool) {
    let h = harness(pool).await;

    let (status, body) = h
        .delete("/api/v1/memories/01ARZ3NDEKTSV4RRFFQ69G5FAV")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "resource.not_found");

    // Create then forget then forget again — the second call must not be a
    // silent 204.
    let item = memory_for(
        "01ARZ3NDEKTSV4RRFFQ69G5FB0",
        &h.user_id,
        "likes tea",
        false,
        at(T0),
    );
    h.store
        .create(&item, &audit(&item, "memory.created"))
        .await
        .unwrap();
    let (status, _) = h
        .delete("/api/v1/memories/01ARZ3NDEKTSV4RRFFQ69G5FB0")
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = h
        .delete("/api/v1/memories/01ARZ3NDEKTSV4RRFFQ69G5FB0")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "resource.not_found");
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn forgetting_another_users_memory_is_404_and_never_deletes_it(pool: PgPool) {
    let h = harness(pool).await;
    let theirs = memory_for(
        "01ARZ3NDEKTSV4RRFFQ69G5FB0",
        &other_user(),
        "likes soy milk",
        false,
        at(T0),
    );
    h.store
        .create(&theirs, &audit(&theirs, "memory.created"))
        .await
        .unwrap();

    let (status, body) = h
        .delete("/api/v1/memories/01ARZ3NDEKTSV4RRFFQ69G5FB0")
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cross-user forget must not leak existence or succeed: {body}"
    );
    assert_eq!(body["code"], "resource.not_found");

    // The other user's memory is untouched.
    assert_eq!(
        h.store
            .get(&other_user(), &mid("01ARZ3NDEKTSV4RRFFQ69G5FB0"))
            .await
            .unwrap(),
        Some(theirs),
        "a cross-user forget attempt must not delete the memory"
    );
}
