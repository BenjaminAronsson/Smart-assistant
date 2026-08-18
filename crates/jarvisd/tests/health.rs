//! F0.5: `GET /api/v1/diagnostics/health` (docs/05 §1; docs/02 §12 startup
//! order: "UI + agent connect; health page shows adapter states"; docs/02
//! §14 diagnostics). Unauthenticated, loopback-only (docs/06 §7) — this
//! test only covers the route/serde contract, not auth (arrives later).

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use jarvis_contracts::health::{HealthResponse, ServiceStatus, UiSettingsDto};
use jarvisd::api::{AppState, router};
use tower::ServiceExt;

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .expect("request must build")
}

// Happy path: default (F0.5, no adapters registered) state answers 200 with
// a well-formed HealthResponse.
#[tokio::test]
async fn health_returns_200_ok() {
    let app = router(AppState::new());
    let response = app
        .oneshot(get("/api/v1/diagnostics/health"))
        .await
        .expect("router must not error");
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn health_content_type_is_json() {
    let app = router(AppState::new());
    let response = app
        .oneshot(get("/api/v1/diagnostics/health"))
        .await
        .expect("router must not error");
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .expect("content-type header must be present")
        .to_str()
        .expect("content-type must be valid ascii");
    assert!(
        content_type.starts_with("application/json"),
        "expected application/json, got {content_type}"
    );
}

// Malformed-response guard, inverted: the body must actually deserialize as
// the documented contract DTO, not just "look json-ish".
#[tokio::test]
async fn health_body_deserializes_as_health_response() {
    let app = router(AppState::new());
    let response = app
        .oneshot(get("/api/v1/diagnostics/health"))
        .await
        .expect("router must not error");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must be readable")
        .to_bytes();

    let health: HealthResponse =
        serde_json::from_slice(&bytes).expect("body must deserialize as HealthResponse");

    assert_eq!(health.status, ServiceStatus::Ok);
    assert_eq!(health.version, env!("CARGO_PKG_VERSION"));
    assert!(
        health.adapters.is_empty(),
        "F0.5 has no adapter joins yet (arrives F0.6); adapters map must be empty, got: {:?}",
        health.adapters
    );
}

#[tokio::test]
async fn health_exposes_non_sensitive_ui_settings_when_configured() {
    let app = router(AppState::new().with_ui_settings(UiSettingsDto {
        background: "abstract".to_owned(),
        panel_ttl_hours: 2,
        motion: "auto".to_owned(),
    }));
    let response = app
        .oneshot(get("/api/v1/diagnostics/health"))
        .await
        .expect("router must not error");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must be readable")
        .to_bytes();
    let health: HealthResponse =
        serde_json::from_slice(&bytes).expect("body must deserialize as HealthResponse");
    assert_eq!(
        health.ui.as_ref().map(|ui| ui.background.as_str()),
        Some("abstract")
    );
    assert_eq!(health.ui.as_ref().map(|ui| ui.panel_ttl_hours), Some(2));
}

// docs/05 §1 / conventional REST: unmapped routes are 404, not swallowed
// into the health handler or a 200.
#[tokio::test]
async fn unmapped_route_returns_404() {
    let app = router(AppState::new());
    let response = app
        .oneshot(get("/api/v1/nonexistent"))
        .await
        .expect("router must not error");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// `infra/install/first-run.sh` gates its "a paired device" step on this exact
// field, and the health endpoint is the only unauthenticated surface (docs/05
// §6.2), so it is the one honest signal an install script can read without a
// token.
//
// This test exists because the field did not: the script shipped in F8.9
// grepping for `"paired":true`, `HealthResponse` never carried it, and the
// check therefore failed on every machine no matter how correctly installed.
// Nobody noticed because nobody had run the script end to end.
#[tokio::test]
async fn health_reports_whether_an_owner_is_paired() {
    let response = jarvisd::api::router(jarvisd::api::AppState::new())
        .oneshot(get("/api/v1/diagnostics/health"))
        .await
        .expect("router responds");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");

    // Present and typed, which is what the script's grep depends on.
    assert!(
        body.get("paired")
            .is_some_and(serde_json::Value::is_boolean),
        "health must carry a boolean `paired`, got: {body}"
    );
    // Fails closed: with no identity store wired there is no owner to claim.
    assert_eq!(body["paired"], false);
    // And it stays a bare boolean — not a device count, not a list.
    assert!(body.get("devices").is_none() && body.get("deviceCount").is_none());
}
