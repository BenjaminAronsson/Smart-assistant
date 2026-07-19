//! v1 authentication (docs/05 §6): one-time pairing bootstrap + bearer-token
//! middleware. Loopback-first by design; upgraded at M7.
//!
//! Token lifecycle: 256-bit random value → returned ONCE in the pair
//! response → stored sha256-hashed. The value never touches logs, the
//! database, or any struct with a serde derive (invariant 5).

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use jarvis_application::ports::IdentityStore;
use jarvis_contracts::auth::{PairRequest, PairResponse};
use jarvis_contracts::errors::ErrorCode;
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::identity::Device;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use crate::problem::problem;

/// Scopes granted to the first paired device (docs/05 §6.3). Scope
/// differentiation (display-agent, voice-capture) arrives with those clients.
const FIRST_DEVICE_SCOPES: &[&str] = &["ui"];

/// Wrong guesses tolerated before the window closes (restart reopens it).
/// A 6-digit code is ~20 bits; loopback-only, but a local brute force must
/// not get 10^6 attempts (docs/06 §5 adversarial thinking).
const MAX_FAILED_PAIR_ATTEMPTS: u32 = 5;

#[derive(Clone)]
pub struct AuthState {
    identity: Arc<dyn IdentityStore>,
    /// One-time pairing code; consumed on successful pair. None = no pairing
    /// window open (all further devices need `jarvisd pair --new`, post-M0).
    pairing_code: Arc<RwLock<Option<String>>>,
    failed_attempts: Arc<RwLock<u32>>,
}

impl AuthState {
    /// First-run bootstrap (docs/05 §6): with no paired devices, open a
    /// pairing window and surface the code in the journal + health page
    /// (loopback only). With devices present, no window opens. An
    /// unreachable database must NOT abort startup (degraded start,
    /// docs/02 §12) — no window opens and a restart re-runs the bootstrap.
    pub async fn bootstrap(identity: Arc<dyn IdentityStore>) -> Self {
        let code = match identity.device_count().await {
            Ok(0) => {
                let code = generate_pairing_code();
                // Deliberate journal output (docs/05 §6 step 1) — the pairing
                // code is the bootstrap secret shown to the local owner only.
                tracing::info!(pairing_code = %code, "no paired devices — pairing window open");
                Some(code)
            }
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error = %e, "pairing bootstrap deferred — database unreachable");
                None
            }
        };
        Self {
            identity,
            pairing_code: Arc::new(RwLock::new(code)),
            failed_attempts: Arc::new(RwLock::new(0)),
        }
    }

    pub fn current_pairing_code(&self) -> Option<String> {
        self.pairing_code
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn consume_code_if_matches(&self, presented: &str) -> bool {
        let mut slot = self
            .pairing_code
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Compare digests so length/content timing reveals nothing useful.
        let matches = match slot.as_deref() {
            Some(expected) => sha256_hex(expected.as_bytes()) == sha256_hex(presented.as_bytes()),
            None => false,
        };
        if matches {
            *slot = None; // single-use
            return true;
        }
        // Brute-force lockout: repeated wrong guesses close the window; a
        // restart (with still zero devices) reopens it with a fresh code.
        if slot.is_some() {
            let mut failed = self
                .failed_attempts
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *failed += 1;
            if *failed >= MAX_FAILED_PAIR_ATTEMPTS {
                *slot = None;
                tracing::warn!(
                    "pairing window closed after {MAX_FAILED_PAIR_ATTEMPTS} failed attempts — \
                     restart jarvisd to reopen"
                );
            }
        }
        false
    }
}

/// Identity attached to authenticated requests by the middleware.
#[derive(Debug, Clone)]
pub struct DeviceContext {
    pub device_id: jarvis_domain::ids::DeviceId,
    pub user_id: jarvis_domain::ids::UserId,
    pub scopes: Vec<String>,
}

/// POST /api/v1/auth/pair — exchange the one-time code for a device token.
pub async fn pair(
    State(auth): State<AuthState>,
    Json(request): Json<PairRequest>,
) -> Result<Json<PairResponse>, Response> {
    if request.device_name.trim().is_empty() {
        return Err(problem(
            StatusCode::BAD_REQUEST,
            ErrorCode::ValidationFailed,
            "deviceName must not be empty",
            None,
        ));
    }
    if !auth.consume_code_if_matches(&request.pairing_code) {
        // Same response whether the window is closed or the code is wrong —
        // no oracle for which it was.
        return Err(problem(
            StatusCode::FORBIDDEN,
            ErrorCode::AuthPairingInvalid,
            "pairing failed",
            Some("no open pairing window matches the presented code".into()),
        ));
    }

    let now = SystemTime::now();
    let token = generate_token();
    let device = Device {
        id: fresh_id(),
        user_id: fresh_id(),
        name: request.device_name.clone(),
        token_hash: sha256_hex(token.as_bytes()),
        scopes: FIRST_DEVICE_SCOPES.iter().map(|s| s.to_string()).collect(),
        created_at: now,
        revoked_at: None,
    };
    let audit = AuditEvent {
        occurred_at: now,
        actor: "system".into(),
        event_type: "device.paired".into(),
        target: format!("device:{}", device.id),
        correlation_id: None,
        payload_json: serde_json::json!({
            "deviceName": device.name,
            "scopes": device.scopes,
        })
        .to_string(),
    };

    auth.identity
        .pair_device("owner", &device, &audit)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "pairing persistence failed");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "pairing could not be persisted",
                None,
            )
        })?;

    Ok(Json(PairResponse {
        device_id: device.id,
        device_token: token,
        scopes: device.scopes,
    }))
}

/// Bearer middleware: every route behind it requires a valid, unrevoked
/// device token; fails closed with 401 auth.invalid_token (docs/05 §6).
pub async fn require_device(
    State(auth): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(token) = token else {
        return unauthorized();
    };
    match auth
        .identity
        .find_active_device_by_token_hash(&sha256_hex(token.as_bytes()))
        .await
    {
        Ok(Some(device)) => {
            request.extensions_mut().insert(DeviceContext {
                device_id: device.id,
                user_id: device.user_id,
                scopes: device.scopes,
            });
            next.run(request).await
        }
        Ok(None) => unauthorized(),
        Err(e) => {
            tracing::error!(error = %e, "device lookup failed");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "identity store unavailable",
                None,
            )
        }
    }
}

fn unauthorized() -> Response {
    problem(
        StatusCode::UNAUTHORIZED,
        ErrorCode::AuthInvalidToken,
        "missing, invalid, or revoked device token",
        None,
    )
}

fn generate_pairing_code() -> String {
    let mut bytes = [0u8; 4];
    rand::rng().fill_bytes(&mut bytes);
    let n = u32::from_be_bytes(bytes) % 1_000_000;
    format!("{:03}-{:03}", n / 1_000, n % 1_000)
}

/// Opaque 256-bit bearer token, hex-encoded (docs/05 §6).
fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn sha256_hex(input: &[u8]) -> String {
    hex::encode(Sha256::digest(input))
}

/// Generate a fresh ULID-backed id at the gateway edge (infra owns
/// randomness; the domain only validates).
pub fn fresh_id<T: std::str::FromStr>() -> T
where
    T::Err: std::fmt::Debug,
{
    ulid::Ulid::new()
        .to_string()
        .parse()
        .expect("generated ULID is canonical")
}
