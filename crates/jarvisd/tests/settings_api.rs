//! F8.8's voice section and F8.11's spend, through the production router with
//! the real bearer middleware in front of it.
//!
//! The assertions that matter are the negative ones, for the same reason they
//! are in `devices_api.rs`: a room satellite is a *paired, authenticated*
//! device, and everything it must not be able to do is something an
//! authenticated request could otherwise do. Here that includes reading the
//! household's spend and withdrawing — or granting — its consent to a
//! third-party egress path.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use jarvis_application::ports::{RepositoryError, SettingsStore, SpendLedger, VoiceOverrides};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::identity::DeviceClass;
use jarvis_domain::ids::DeviceId;
use jarvisd::api::{AppState, Wiring, router_with};
use jarvisd::auth::AuthState;
use jarvisd::settings::{SettingsApi, VoiceCapabilities};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

mod identity_fixture;
use identity_fixture::{InMemoryIdentityStore, device};

const OWNER_TOKEN: &str = "owner-token";
const NODE_TOKEN: &str = "node-token";

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// An in-memory override layer that also remembers what it was asked to audit
/// — invariant 6 says the audit row is written with the change, so a test that
/// never looks at it cannot claim the change was recorded.
#[derive(Default)]
struct MemSettings {
    overrides: std::sync::Mutex<VoiceOverrides>,
    audited: std::sync::Mutex<Vec<AuditEvent>>,
}

#[async_trait::async_trait]
impl SettingsStore for MemSettings {
    async fn voice_overrides(&self) -> Result<VoiceOverrides, RepositoryError> {
        Ok(self.overrides.lock().expect("lock").clone())
    }

    async fn set_voice_overrides(
        &self,
        incoming: &VoiceOverrides,
        _by_device: &DeviceId,
        _at: std::time::SystemTime,
        audit: &AuditEvent,
    ) -> Result<VoiceOverrides, RepositoryError> {
        let mut stored = self.overrides.lock().expect("lock");
        if incoming.wake_word.is_some() {
            stored.wake_word = incoming.wake_word.clone();
        }
        if incoming.elevenlabs_enabled.is_some() {
            stored.elevenlabs_enabled = incoming.elevenlabs_enabled;
        }
        self.audited.lock().expect("lock").push(audit.clone());
        Ok(stored.clone())
    }
}

struct FixedSpend(u64);

#[async_trait::async_trait]
impl SpendLedger for FixedSpend {
    async fn reserve(&self, _characters: u64) -> Result<u64, RepositoryError> {
        Ok(self.0)
    }
    async fn refund(&self, _characters: u64) -> Result<(), RepositoryError> {
        Ok(())
    }
    async fn spent(&self) -> Result<u64, RepositoryError> {
        Ok(self.0)
    }
}

struct Harness {
    router: axum::Router,
    settings: Arc<MemSettings>,
    consent: Arc<AtomicBool>,
}

/// `configured`/`local_fallback` are the two conditions ADR-033 §3 makes
/// enabling depend on, so every test names them explicitly.
async fn harness(elevenlabs_configured: bool, local_fallback: Option<&str>) -> Harness {
    let store = Arc::new(
        InMemoryIdentityStore::new()
            .with_device(device(
                "owner shell",
                DeviceClass::OwnerUi,
                &token_hash(OWNER_TOKEN),
            ))
            .with_device(device(
                "kitchen node",
                DeviceClass::RoomNode,
                &token_hash(NODE_TOKEN),
            )),
    );
    let settings = Arc::new(MemSettings::default());
    let consent = Arc::new(AtomicBool::new(false));

    let api = SettingsApi::new(
        settings.clone(),
        VoiceCapabilities {
            // Deliberately a word with no provisioned model. The shipped
            // default is now `hey jarvis`, which HAS one (ADR-032 §1, amended
            // 2026-08-17) — but an owner can still configure a word that does
            // not, and that state must be reported rather than hidden, so the
            // fixture keeps exercising it.
            configured_wake_word: "andy".to_owned(),
            available_wake_words: vec!["alexa".to_owned(), "hey jarvis".to_owned()],
            elevenlabs_configured,
            local_fallback: local_fallback.map(str::to_owned),
            character_budget: 100_000,
        },
    )
    .with_ledger(Arc::new(FixedSpend(12_480)))
    .with_consent(consent.clone());

    let auth = AuthState::bootstrap(store).await;
    let state = AppState::new().with_auth(auth);
    let router = router_with(
        state,
        Wiring {
            settings: Some(api),
            ..Wiring::default()
        },
    );

    Harness {
        router,
        settings,
        consent,
    }
}

async fn send(router: &axum::Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request")
}

fn patch(token: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri("/api/v1/settings/voice")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

#[tokio::test]
async fn the_owner_sees_the_word_the_spend_and_the_fallback() {
    let h = harness(true, Some("wyoming-tts")).await;
    let (status, body) = send(&h.router, get("/api/v1/settings/voice", OWNER_TOKEN)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["wakeWord"], "andy");
    assert_eq!(body["elevenlabs"]["spentCharacters"], 12_480);
    assert_eq!(body["elevenlabs"]["characterBudget"], 100_000);
    assert_eq!(body["elevenlabs"]["localFallback"], "wyoming-tts");
    assert_eq!(body["elevenlabs"]["enabled"], false, "off by default");
    // The "Andy" case, surfaced rather than swallowed: a node configured for a
    // word with no model answers to nothing while looking perfectly healthy.
    assert!(
        body["wakeWordWarning"]
            .as_str()
            .is_some_and(|w| w.contains("andy")),
        "the missing model must be named, got {:?}",
        body["wakeWordWarning"]
    );
}

/// The negative that matters most: a satellite is authenticated, and must
/// still not be able to read the household's spend or touch its consent.
#[tokio::test]
async fn a_room_node_can_neither_read_nor_change_the_voice_settings() {
    let h = harness(true, Some("wyoming-tts")).await;

    let (status, _) = send(&h.router, get("/api/v1/settings/voice", NODE_TOKEN)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = send(
        &h.router,
        patch(NODE_TOKEN, serde_json::json!({ "elevenlabsEnabled": true })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        !h.consent.load(Ordering::Relaxed),
        "a refused request must not have moved the gate"
    );
}

/// A word with no provisioned model cannot be chosen. Free text here would let
/// one typo take every node in the house deaf, silently.
#[tokio::test]
async fn a_wake_word_with_no_model_is_refused_and_says_what_there_is() {
    let h = harness(true, Some("wyoming-tts")).await;
    let (status, body) = send(
        &h.router,
        patch(OWNER_TOKEN, serde_json::json!({ "wakeWord": "jeeves" })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let detail = body["detail"].as_str().unwrap_or_default().to_owned()
        + body["title"].as_str().unwrap_or_default();
    assert!(
        detail.contains("alexa") || detail.contains("hey jarvis"),
        "the refusal must say what IS available, got {body}"
    );
}

#[tokio::test]
async fn a_provisioned_wake_word_is_accepted_and_clears_the_warning() {
    let h = harness(true, Some("wyoming-tts")).await;
    let (status, body) = send(
        &h.router,
        patch(OWNER_TOKEN, serde_json::json!({ "wakeWord": "hey jarvis" })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["wakeWord"], "hey jarvis");
    assert_eq!(
        body["wakeWordWarning"],
        serde_json::Value::Null,
        "a word with a model needs no warning"
    );
}

/// ADR-033 §2: consenting to something that cannot work is consent to nothing.
#[tokio::test]
async fn enabling_elevenlabs_is_refused_when_it_is_not_configured() {
    let h = harness(false, Some("wyoming-tts")).await;
    let (status, _) = send(
        &h.router,
        patch(
            OWNER_TOKEN,
            serde_json::json!({ "elevenlabsEnabled": true }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(!h.consent.load(Ordering::Relaxed));
}

/// ADR-033 §3: without a local voice, an outage would be a mute house — the
/// same condition `main.rs` refuses to start under.
#[tokio::test]
async fn enabling_elevenlabs_is_refused_without_a_local_fallback() {
    let h = harness(true, None).await;
    let (status, body) = send(
        &h.router,
        patch(
            OWNER_TOKEN,
            serde_json::json!({ "elevenlabsEnabled": true }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(!h.consent.load(Ordering::Relaxed));
    let text = body.to_string();
    assert!(
        text.contains("local voice") || text.contains("wyoming_tts"),
        "the refusal must name the missing fallback, got {body}"
    );
}

/// The whole point of the live gate: consent takes effect on the next
/// sentence, not the next restart.
#[tokio::test]
async fn consent_moves_the_live_gate_and_is_audited() {
    let h = harness(true, Some("wyoming-tts")).await;

    let (status, body) = send(
        &h.router,
        patch(
            OWNER_TOKEN,
            serde_json::json!({ "elevenlabsEnabled": true }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["elevenlabs"]["enabled"], true);
    assert!(
        h.consent.load(Ordering::Relaxed),
        "the synthesiser reads this gate per utterance; it must be open now"
    );

    // Withdrawing is the direction that matters more, and it must be just as
    // immediate: a house that keeps talking to a third party until someone
    // finds a terminal has not honoured the switch.
    let (status, body) = send(
        &h.router,
        patch(
            OWNER_TOKEN,
            serde_json::json!({ "elevenlabsEnabled": false }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["elevenlabs"]["enabled"], false);
    assert!(!h.consent.load(Ordering::Relaxed));

    // Invariant 6: both changes are recorded, and neither payload carries a
    // credential — the API key is a keyring reference and never reaches here.
    let audited = h.settings.audited.lock().expect("lock");
    assert_eq!(audited.len(), 2);
    for event in audited.iter() {
        assert_eq!(event.event_type, "settings.voice.updated");
        assert!(event.actor.starts_with("device:"));
        assert!(
            !event.payload_json.contains("api_key") && !event.payload_json.contains("keyring:"),
            "no credential may appear in an audit payload"
        );
    }
}

/// A node reads the one setting it is *about*, and nothing else.
#[tokio::test]
async fn a_node_reads_the_wake_word_and_learns_nothing_else() {
    let h = harness(true, Some("wyoming-tts")).await;
    let (status, body) = send(&h.router, get("/api/v1/settings/node-voice", NODE_TOKEN)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["wakeWord"], "andy");
    // Not the spend, not the consent state, not what else could be configured.
    assert_eq!(body.as_object().expect("an object").len(), 1);
}

#[tokio::test]
async fn the_settings_surface_needs_a_token_at_all() {
    let h = harness(true, Some("wyoming-tts")).await;
    let request = Request::builder()
        .uri("/api/v1/settings/voice")
        .body(Body::empty())
        .expect("request");
    let (status, _) = send(&h.router, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
