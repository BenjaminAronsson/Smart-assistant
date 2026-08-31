//! Node pairing: challenge-response with per-device keys (F7.2, FR-19,
//! **ADR-031**; docs/05 §6.5, docs/06 §5 "remote node impersonation").
//!
//! Three steps, and the split is the security design:
//!
//! 1. **The owner opens a window** (`POST /api/v1/devices/pairing-window`,
//!    `ui` scope) and reads the one-time code out to the node. Not
//!    `jarvisd pair --new`: a separate CLI process cannot mutate the running
//!    daemon's in-flight state, so that shape would need the window persisted
//!    — giving an offline secret a durable home for no gain (ADR-031 §5).
//! 2. **The node presents its public key and the code** and gets a challenge.
//! 3. **The node returns a signature** over that challenge and gets its token.
//!
//! Why not just accept the code (what the bootstrap does)? Because on a LAN
//! the code and the resulting token are both observable in a way loopback
//! never was. Step 3 proves the node *holds the private key* it registered, so
//! the token is bound to something the network cannot replay, and every later
//! reconnect can be re-anchored to that same key.
//!
//! What a node may become is **not** its own decision: it *requests* a class
//! and the server *assigns* one. `owner-ui` is refused outright rather than
//! silently downgraded — a client that believes it asked for more than it got
//! is a client that will act on the wrong assumption.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, VerifyingKey};
use jarvis_application::ports::NodePairOutcome;
use jarvis_contracts::auth::PairResponse;
use jarvis_contracts::errors::ErrorCode;
use jarvis_contracts::pairing::{
    NodePairChallengeDto, NodePairCompleteRequest, NodePairStartRequest, PairingWindowDto,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::identity::{ClassScope, Device, DeviceClass};
use rand::RngCore;

use crate::auth::{AuthState, DeviceContext};
use crate::problem::problem;
use crate::time::rfc3339;

/// How long an opened window stays open. Long enough to walk to the satellite
/// and type, short enough that a forgotten window is not a standing door.
pub const WINDOW_TTL: Duration = Duration::from_secs(5 * 60);

/// How long a challenge stays signable. This is a machine round trip, not a
/// human one.
pub const CHALLENGE_TTL: Duration = Duration::from_secs(60);

/// In-flight challenges tolerated at once. Each is ~100 bytes, but an
/// unbounded map reachable from an unauthenticated route is a memory-growth
/// primitive, so it is capped and the cap is enforced by refusing new starts
/// rather than by evicting — evicting would let an attacker push out the
/// legitimate node's challenge.
const MAX_OPEN_CHALLENGES: usize = 8;

/// Longest device name a node may propose (mirrors the bootstrap bound).
const MAX_DEVICE_NAME_CHARS: usize = 80;

const ED25519_PUBLIC_KEY_BYTES: usize = 32;
const ED25519_SIGNATURE_BYTES: usize = 64;

/// One issued, not-yet-spent challenge, bound to everything it was issued for.
/// Binding the class and name here — not re-reading them from the completing
/// request — is what stops a caller from signing a cheap challenge and then
/// completing as a different class.
#[derive(Clone)]
struct OpenChallenge {
    nonce: [u8; 32],
    public_key: String,
    device_name: String,
    class: DeviceClass,
    expires_at: SystemTime,
}

/// The node-pairing window and the challenges issued under it.
///
/// In-memory by design (ADR-031 §5): the window is a short-lived secret the
/// owner is holding in their head while they walk to the device, and a restart
/// legitimately closes it.
#[derive(Clone, Default)]
pub struct PairingState {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Default)]
struct Inner {
    window: Option<Window>,
    challenges: HashMap<String, OpenChallenge>,
}

struct Window {
    code: String,
    expires_at: SystemTime,
    failed_attempts: u32,
}

/// Wrong codes tolerated before the window closes. Same reasoning as the
/// bootstrap window: a 6-digit code is ~20 bits, and a LAN attacker must not
/// get 10^6 tries at it.
const MAX_FAILED_ATTEMPTS: u32 = 5;

impl PairingState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open (or replace) the window and return the code. Replacing rather than
    /// refusing is deliberate: an owner who opens a second window has decided
    /// the first attempt failed, and leaving the old code live would mean two
    /// valid codes for one intent.
    /// Open a window without going through the owner-authenticated route —
    /// for tests that need to isolate a *later* step's refusal (e.g. "a node
    /// cannot pair before an owner exists", where no owner can open one).
    #[doc(hidden)]
    pub fn open_window_for_test(&self, now: SystemTime) -> (String, SystemTime) {
        self.open_window(now)
    }

    fn open_window(&self, now: SystemTime) -> (String, SystemTime) {
        let code = generate_pairing_code();
        let expires_at = now + WINDOW_TTL;
        let mut inner = self.write();
        inner.window = Some(Window {
            code: code.clone(),
            expires_at,
            failed_attempts: 0,
        });
        // A new window invalidates challenges issued under the old one.
        inner.challenges.clear();
        (code, expires_at)
    }

    /// Check a presented code against the open window **without** consuming
    /// it: the window is spent by a completed pairing, not by starting one, so
    /// a node that fumbles the signature can retry without the owner
    /// re-opening. Wrong codes still count toward the lockout.
    fn check_code(&self, presented: &str, now: SystemTime) -> bool {
        let mut inner = self.write();
        let Some(window) = &mut inner.window else {
            return false;
        };
        if now >= window.expires_at {
            inner.window = None;
            return false;
        }
        // Digest comparison so length/content timing reveals nothing.
        if sha256_hex(window.code.as_bytes()) == sha256_hex(presented.as_bytes()) {
            return true;
        }
        window.failed_attempts += 1;
        if window.failed_attempts >= MAX_FAILED_ATTEMPTS {
            tracing::warn!("node pairing window closed after repeated wrong codes");
            inner.window = None;
        }
        false
    }

    fn issue(&self, challenge: OpenChallenge, now: SystemTime) -> Option<String> {
        let mut inner = self.write();
        inner.challenges.retain(|_, c| c.expires_at > now);
        if inner.challenges.len() >= MAX_OPEN_CHALLENGES {
            return None;
        }
        let id = hex::encode(random_bytes::<16>());
        inner.challenges.insert(id.clone(), challenge);
        Some(id)
    }

    /// Take the challenge out of the map. Single-use: whether the signature
    /// then verifies or not, this exact challenge can never be presented
    /// again, which is what makes a captured signature worthless.
    fn take(&self, id: &str) -> Option<OpenChallenge> {
        self.write().challenges.remove(id)
    }

    /// Close the window and drop every outstanding challenge — called once a
    /// node has actually paired.
    fn close(&self) {
        let mut inner = self.write();
        inner.window = None;
        inner.challenges.clear();
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// `POST /api/v1/devices/pairing-window` — owner-only (`ui`, enforced by the
/// class gate on the protected router *and* here).
#[tracing::instrument(skip_all, fields(device_id = %caller.device_id))]
pub async fn open_window(
    State(state): State<PairingApi>,
    Extension(caller): Extension<DeviceContext>,
) -> Result<Json<PairingWindowDto>, Response> {
    if !caller.holds(ClassScope::Ui.as_str()) {
        return Err(problem(
            StatusCode::FORBIDDEN,
            ErrorCode::AuthScopeMissing,
            "opening a pairing window requires the `ui` scope",
            None,
        ));
    }
    let now = SystemTime::now();
    let (code, expires_at) = state.pairing.open_window(now);
    // Opening the door to new enrolment is an authority-relevant owner action;
    // it gets a durable record, not just a log line (M7 gate S-2).
    state
        .auth
        .record_refusal(AuditEvent {
            occurred_at: now,
            actor: format!("device:{}", caller.device_id),
            event_type: "device.pairing_window_opened".to_owned(),
            target: "identity:pairing-window".to_owned(),
            correlation_id: None,
            payload_json: serde_json::json!({
                "ttlSeconds": WINDOW_TTL.as_secs(),
            })
            .to_string(),
        })
        .await;
    // Deliberate: the owner asked for this code and is looking at the reply.
    // It is not logged.
    tracing::info!(
        expires_in_s = WINDOW_TTL.as_secs(),
        "node pairing window opened"
    );
    Ok(Json(PairingWindowDto {
        pairing_code: code,
        expires_at: rfc3339(expires_at),
    }))
}

/// `POST /api/v1/devices/pair` — step one. Unauthenticated: the node has no
/// token yet, which is the entire point of pairing. Everything it can do here
/// is bounded by the window, the lockout, and [`MAX_OPEN_CHALLENGES`].
#[tracing::instrument(skip_all)]
pub async fn start(
    State(state): State<PairingApi>,
    Json(request): Json<NodePairStartRequest>,
) -> Result<Json<NodePairChallengeDto>, Response> {
    let device_name = request.device_name.trim();
    if device_name.is_empty()
        || device_name.chars().count() > MAX_DEVICE_NAME_CHARS
        || device_name.chars().any(char::is_control)
    {
        return Err(problem(
            StatusCode::BAD_REQUEST,
            ErrorCode::ValidationFailed,
            "deviceName must be non-empty, bounded, and free of control characters",
            None,
        ));
    }

    // Parse the key before checking the code, so a malformed key is a
    // validation error rather than a wasted pairing attempt — and so the
    // stored key is known-good before anything is issued against it.
    let public_key = parse_public_key(&request.public_key).ok_or_else(invalid_public_key)?;

    let Some(class) = requested_class(&request.requested_class) else {
        state
            .auth
            .record_refusal(pairing_refusal(
                "device.pairing_refused",
                "requested a class it may not have",
            ))
            .await;
        return Err(class_not_grantable());
    };

    let now = SystemTime::now();
    if !state.pairing.check_code(&request.pairing_code, now) {
        // A wrong or expired code on an unauthenticated LAN route is the
        // docs/06 §5 "remote node impersonation" signal, and logs are not the
        // append-only record that threat deserves (M7 gate S-2).
        state
            .auth
            .record_refusal(pairing_refusal("device.pairing_refused", "code rejected"))
            .await;
        // Same answer whether the window is closed, expired, or the code is
        // wrong — no oracle for which.
        return Err(problem(
            StatusCode::FORBIDDEN,
            ErrorCode::AuthPairingInvalid,
            "pairing failed",
            Some("no open pairing window matches the presented code".into()),
        ));
    }

    let nonce = random_bytes::<32>();
    let expires_at = now + CHALLENGE_TTL;
    let challenge = OpenChallenge {
        nonce,
        public_key: BASE64.encode(public_key.as_bytes()),
        device_name: device_name.to_owned(),
        class,
        expires_at,
    };
    let Some(challenge_id) = state.pairing.issue(challenge, now) else {
        return Err(problem(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::ValidationFailed,
            "too many pairing attempts in flight",
            None,
        ));
    };

    tracing::info!(class = %class, "node pairing challenge issued");
    Ok(Json(NodePairChallengeDto {
        challenge_id,
        challenge: BASE64.encode(nonce),
        expires_at: rfc3339(expires_at),
    }))
}

/// `POST /api/v1/devices/pair/complete` — step two: prove possession, receive
/// a token whose authority the *class* decides.
#[tracing::instrument(skip_all)]
pub async fn complete(
    State(state): State<PairingApi>,
    Json(request): Json<NodePairCompleteRequest>,
) -> Result<Json<PairResponse>, Response> {
    let now = SystemTime::now();
    // Taken unconditionally: a challenge is spent by being *presented*, so a
    // wrong signature burns it rather than allowing a retry loop against one
    // nonce.
    let Some(challenge) = state.pairing.take(&request.challenge_id) else {
        return Err(challenge_rejected());
    };
    if now >= challenge.expires_at {
        return Err(challenge_rejected());
    }

    let signature_bytes = decode_fixed::<ED25519_SIGNATURE_BYTES>(&request.signature)
        .ok_or_else(challenge_rejected)?;
    let key_bytes =
        decode_fixed::<ED25519_PUBLIC_KEY_BYTES>(&challenge.public_key).ok_or_else(|| {
            tracing::error!("stored challenge key is unparseable");
            challenge_rejected()
        })?;
    let verifying_key = VerifyingKey::from_bytes(&key_bytes).map_err(|_| challenge_rejected())?;
    if verifying_key
        .verify_strict(&challenge.nonce, &Signature::from_bytes(&signature_bytes))
        .is_err()
    {
        tracing::warn!("node pairing signature did not verify");
        // The sharpest signal on this surface: someone holds a valid challenge
        // and cannot prove the key it was issued to (M7 gate S-2).
        state
            .auth
            .record_refusal(pairing_refusal(
                "device.pairing_refused",
                "signature did not verify",
            ))
            .await;
        return Err(challenge_rejected());
    }

    // Verified. The class was fixed when the challenge was issued.
    let token = generate_token();
    let device = Device {
        id: crate::auth::fresh_id(),
        // Overwritten by the store, which attaches the node to the owner's
        // existing user; a node never mints a user.
        user_id: crate::auth::fresh_id(),
        name: challenge.device_name.clone(),
        token_hash: sha256_hex(token.as_bytes()),
        public_key: Some(challenge.public_key.clone()),
        class: challenge.class,
        created_at: now,
        last_seen_at: None,
        revoked_at: None,
        revoked_reason: None,
    };
    let audit = AuditEvent {
        occurred_at: now,
        actor: "system".into(),
        event_type: "device.paired".into(),
        target: format!("device:{}", device.id),
        correlation_id: None,
        payload_json: serde_json::json!({
            "deviceName": device.name,
            "deviceClass": device.class.as_str(),
            "scopes": device.effective_scopes(),
            // The key itself is public, but a fingerprint is what an operator
            // can actually compare against the node's own display.
            "keyFingerprint": key_fingerprint(&challenge.public_key),
            "method": "challenge-response",
        })
        .to_string(),
    };

    let outcome = state
        .auth
        .identity()
        .pair_node_device(&device, &audit)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "node pairing persistence failed");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "pairing could not be persisted",
                None,
            )
        })?;

    match outcome {
        NodePairOutcome::Paired => {}
        NodePairOutcome::NoOwner => {
            return Err(problem(
                StatusCode::CONFLICT,
                ErrorCode::AuthPairingInvalid,
                "no owner device exists to pair against",
                Some("pair the owner's own client first (docs/05 §6.1)".into()),
            ));
        }
        NodePairOutcome::KeyAlreadyPaired => {
            return Err(problem(
                StatusCode::CONFLICT,
                ErrorCode::AuthPairingInvalid,
                "this key is already paired",
                Some("revoke the existing device before pairing the key again".into()),
            ));
        }
    }

    // One window, one node.
    state.pairing.close();
    tracing::info!(device_id = %device.id, class = %device.class, "node paired");

    Ok(Json(PairResponse {
        device_id: device.id.clone(),
        device_token: token,
        device_class: device.class.as_str().to_owned(),
        scopes: device.effective_scopes(),
        server_fingerprint: state.server_fingerprint.clone(),
    }))
}

/// Which classes a node may ask to be. `owner-ui` is refused rather than
/// downgraded (docs/05 §6.3).
///
/// Returns `Option` rather than `Result<_, Response>` for the reason
/// [`crate::lists::IdFault`] exists: an axum `Response` is large, and putting
/// one in a helper's `Err` makes every caller's result enormous (clippy
/// `result_large_err`).
fn requested_class(raw: &str) -> Option<DeviceClass> {
    match raw.parse().ok()? {
        DeviceClass::DisplayNode => Some(DeviceClass::DisplayNode),
        DeviceClass::VoiceNode => Some(DeviceClass::VoiceNode),
        DeviceClass::RoomNode => Some(DeviceClass::RoomNode),
        // The one class that carries tool authority is not on the menu.
        DeviceClass::OwnerUi => None,
    }
}

/// An append-only record of a refused pairing attempt. The actor is
/// unauthenticated by construction — that is what pairing is — so the event
/// names the surface rather than a device, and carries no attacker-controlled
/// text.
fn pairing_refusal(event_type: &str, reason: &'static str) -> AuditEvent {
    AuditEvent {
        occurred_at: SystemTime::now(),
        actor: "unauthenticated".to_owned(),
        event_type: event_type.to_owned(),
        target: "identity:pairing".to_owned(),
        correlation_id: None,
        payload_json: serde_json::json!({ "reason": reason }).to_string(),
    }
}

fn class_not_grantable() -> Response {
    problem(
        StatusCode::FORBIDDEN,
        ErrorCode::IdentityClassNotGrantable,
        "that device class cannot be requested",
        Some("a node may request display-node, voice-node, or room-node".into()),
    )
}

/// Rejects non-canonical and small-order points, so a node cannot register a
/// key whose signatures anyone could forge. `Option` for the same
/// `result_large_err` reason as [`requested_class`].
fn parse_public_key(raw: &str) -> Option<VerifyingKey> {
    VerifyingKey::from_bytes(&decode_fixed::<ED25519_PUBLIC_KEY_BYTES>(raw)?).ok()
}

fn invalid_public_key() -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        ErrorCode::ValidationFailed,
        "publicKey must be a base64 Ed25519 public key",
        None,
    )
}

fn decode_fixed<const N: usize>(raw: &str) -> Option<[u8; N]> {
    let bytes = BASE64.decode(raw).ok()?;
    bytes.try_into().ok()
}

fn challenge_rejected() -> Response {
    problem(
        StatusCode::FORBIDDEN,
        ErrorCode::IdentityChallengeRejected,
        "pairing challenge rejected",
        None,
    )
}

/// First 16 hex characters of sha256(public key) — enough to compare against
/// what a node prints, short enough to read aloud.
fn key_fingerprint(public_key_b64: &str) -> String {
    sha256_hex(public_key_b64.as_bytes())[..16].to_owned()
}

fn generate_pairing_code() -> String {
    let bytes = random_bytes::<4>();
    let n = u32::from_be_bytes(bytes) % 1_000_000;
    format!("{:03}-{:03}", n / 1_000, n % 1_000)
}

fn generate_token() -> String {
    hex::encode(random_bytes::<32>())
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

fn sha256_hex(input: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(input))
}

/// State the pairing routes carry: the identity store (through `AuthState`)
/// and the in-memory window/challenge map.
#[derive(Clone)]
pub struct PairingApi {
    pub auth: AuthState,
    pub pairing: PairingState,
    /// The listener's certificate fingerprint, handed to the node so it can
    /// pin us (F7.3). `None` on plaintext loopback.
    pub server_fingerprint: Option<String>,
}
