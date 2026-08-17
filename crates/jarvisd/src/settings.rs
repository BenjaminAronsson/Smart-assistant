//! The owner-tunable settings surface (F8.8's voice section, F8.11's spend).
//!
//! The narrow slice of configuration the shell may change, and it is narrow by
//! construction rather than by convention: the request DTO has two fields, the
//! table has two columns, and neither can express a bind address, a secret
//! reference, an allowlist, or a filesystem path. A settings API that could
//! write arbitrary configuration would be a way to reconfigure the daemon
//! through the same surface that serves untrusted content.
//!
//! Two things here are security decisions rather than plumbing:
//!
//! 1. **Enabling ElevenLabs is consent to a third-party egress path**
//!    (ADR-033 §2). It is gated on `ui` scope, audited in the same transaction
//!    as the change, and **refused** unless the daemon is actually configured
//!    for it *and* a local voice exists to fall back to — consenting to
//!    something that cannot work would be consent to nothing (ADR-033 §3).
//! 2. **The wake word is offered from what is provisioned**, never free text.
//!    A word with no model is a node that has gone deaf, and the shell should
//!    not be able to cause that by typing (ADR-032 §4, consequence 3).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use jarvis_application::ports::{SettingsStore, SpendLedger, VoiceOverrides};
use jarvis_contracts::errors::ErrorCode;
use jarvis_contracts::settings::{
    ElevenLabsSettingsDto, NodeVoiceSettingsDto, UpdateVoiceSettingsRequest, VoiceSettingsDto,
};
use jarvis_domain::audit::AuditEvent;

use crate::auth::DeviceContext;
use crate::problem::problem;

/// What the daemon was configured with, and therefore what the shell is
/// allowed to choose between. Read from `jarvisd.toml` at startup; the
/// override layer can only pick among these, never add to them.
#[derive(Clone)]
pub struct VoiceCapabilities {
    /// The word the config file names, used when there is no override.
    pub configured_wake_word: String,
    /// Words this installation has a provisioned model for (ADR-032).
    pub available_wake_words: Vec<String>,
    /// Whether ElevenLabs has an API key reference and a voice.
    pub elevenlabs_configured: bool,
    /// Whether a local voice exists to fall back to (ADR-033 §3).
    pub local_fallback: Option<String>,
    pub character_budget: u64,
}

#[derive(Clone)]
pub struct SettingsApi {
    store: Arc<dyn SettingsStore>,
    ledger: Option<Arc<dyn SpendLedger>>,
    /// The live gate the synthesiser reads at speaking time, so withdrawing
    /// consent takes effect on the next sentence rather than the next restart.
    consent: Option<Arc<AtomicBool>>,
    capabilities: VoiceCapabilities,
}

impl SettingsApi {
    pub fn new(store: Arc<dyn SettingsStore>, capabilities: VoiceCapabilities) -> Self {
        Self {
            store,
            ledger: None,
            consent: None,
            capabilities,
        }
    }

    pub fn with_ledger(mut self, ledger: Arc<dyn SpendLedger>) -> Self {
        self.ledger = Some(ledger);
        self
    }

    pub fn with_consent(mut self, consent: Arc<AtomicBool>) -> Self {
        self.consent = Some(consent);
        self
    }

    /// The wake word in force: the override if there is one, else the config.
    fn wake_word(&self, overrides: &VoiceOverrides) -> String {
        overrides
            .wake_word
            .clone()
            .unwrap_or_else(|| self.capabilities.configured_wake_word.clone())
    }

    fn elevenlabs_enabled(&self, overrides: &VoiceOverrides) -> bool {
        // The live gate is the truth when one is wired — it is what the
        // synthesiser actually reads. The stored override is what survives a
        // restart, and the two are set together.
        match (&self.consent, overrides.elevenlabs_enabled) {
            (Some(consent), _) => consent.load(Ordering::Relaxed),
            (None, Some(stored)) => stored,
            (None, None) => false,
        }
    }
}

fn storage_problem(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "settings storage failed");
    problem(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::ProviderUnavailable,
        "settings are unavailable",
        None,
    )
}

fn holds_ui(caller: &DeviceContext) -> bool {
    caller.holds(jarvis_domain::identity::ClassScope::Ui.as_str())
}

fn ui_required() -> Response {
    problem(
        StatusCode::FORBIDDEN,
        ErrorCode::PolicyDenied,
        "settings are administered from the shell",
        Some("this device does not hold the `ui` scope".to_owned()),
    )
}

async fn view(api: &SettingsApi, overrides: &VoiceOverrides) -> Result<VoiceSettingsDto, Response> {
    let spent = match api.ledger.as_ref() {
        Some(ledger) => ledger.spent().await.map_err(storage_problem)?,
        None => 0,
    };
    let wake_word = api.wake_word(overrides);
    // Named rather than silently tolerated: the configured word having no
    // model is exactly the "Andy" case (ADR-032 §1), and a node in that state
    // answers to nothing while looking perfectly healthy.
    let wake_word_warning = (!api
        .capabilities
        .available_wake_words
        .iter()
        .any(|w| w == &wake_word))
    .then(|| {
        format!(
            "no wake-word model is provisioned for {wake_word:?}; nodes fall back to \
             push-to-talk until one is installed"
        )
    });

    Ok(VoiceSettingsDto {
        wake_word,
        available_wake_words: api.capabilities.available_wake_words.clone(),
        wake_word_warning,
        elevenlabs: ElevenLabsSettingsDto {
            configured: api.capabilities.elevenlabs_configured,
            enabled: api.elevenlabs_enabled(overrides),
            spent_characters: spent,
            character_budget: api.capabilities.character_budget,
            period: jarvis_infra::settings::period_of(SystemTime::now()),
            local_fallback: api
                .capabilities
                .local_fallback
                .clone()
                .unwrap_or_else(|| "none".to_owned()),
        },
    })
}

/// `GET /api/v1/settings/voice`
pub async fn get_voice(
    State(api): State<SettingsApi>,
    axum::Extension(caller): axum::Extension<DeviceContext>,
) -> Result<Json<VoiceSettingsDto>, Response> {
    if !holds_ui(&caller) {
        return Err(ui_required());
    }
    let overrides = api.store.voice_overrides().await.map_err(storage_problem)?;
    Ok(Json(view(&api, &overrides).await?))
}

/// `PATCH /api/v1/settings/voice`
pub async fn update_voice(
    State(api): State<SettingsApi>,
    axum::Extension(caller): axum::Extension<DeviceContext>,
    Json(request): Json<UpdateVoiceSettingsRequest>,
) -> Result<Json<VoiceSettingsDto>, Response> {
    if !holds_ui(&caller) {
        return Err(ui_required());
    }

    // The wake word must be one this installation can actually answer to.
    // Validated against provisioned models rather than accepted as free text:
    // a typo would otherwise take every node in the house deaf, silently.
    if let Some(word) = request.wake_word.as_deref() {
        let word = word.trim();
        if !api
            .capabilities
            .available_wake_words
            .iter()
            .any(|w| w == word)
        {
            return Err(problem(
                StatusCode::BAD_REQUEST,
                ErrorCode::ValidationFailed,
                "no wake-word model is provisioned for that word",
                Some(format!(
                    "available: {}",
                    api.capabilities.available_wake_words.join(", ")
                )),
            ));
        }
    }

    // Consent to a third-party egress path (ADR-033 §2) is refused unless it
    // could actually be honoured. Both halves matter: without a key there is
    // nothing to consent *to*, and without a local voice an outage would mean
    // a mute house — which is the condition ADR-033 §3 exists to prevent, and
    // the same one `main.rs` refuses to start under.
    if request.elevenlabs_enabled == Some(true) {
        if !api.capabilities.elevenlabs_configured {
            return Err(problem(
                StatusCode::CONFLICT,
                ErrorCode::ValidationFailed,
                "ElevenLabs is not configured on this daemon",
                Some(
                    "set [voice.elevenlabs] api_key_ref and voice_id in jarvisd.toml first \
                     (ADR-033)"
                        .to_owned(),
                ),
            ));
        }
        if api.capabilities.local_fallback.is_none() {
            return Err(problem(
                StatusCode::CONFLICT,
                ErrorCode::ValidationFailed,
                "there is no local voice to fall back to",
                Some(
                    "set [voice].wyoming_tts as well; an alarm must still ring with the \
                     network down (ADR-033 §3)"
                        .to_owned(),
                ),
            ));
        }
    }

    let now = SystemTime::now();
    let overrides = VoiceOverrides {
        wake_word: request.wake_word.map(|w| w.trim().to_owned()),
        elevenlabs_enabled: request.elevenlabs_enabled,
    };

    // The audit row names what changed, never a credential — the API key is a
    // keyring reference and never passes through this surface at all. Both
    // values are closed vocabulary: the wake word was just checked against the
    // provisioned list, so no free text reaches a hashed audit payload.
    let audit = AuditEvent {
        occurred_at: now,
        actor: format!("device:{}", caller.device_id),
        event_type: "settings.voice.updated".into(),
        target: "settings:voice".to_owned(),
        correlation_id: None,
        payload_json: serde_json::json!({
            "wakeWord": overrides.wake_word,
            "elevenlabsEnabled": overrides.elevenlabs_enabled,
        })
        .to_string(),
    };

    let stored = api
        .store
        .set_voice_overrides(&overrides, &caller.device_id, now, &audit)
        .await
        .map_err(storage_problem)?;

    // Only after the change is durably recorded: a live gate that opened on a
    // write that then failed would be consent nobody agreed to and nothing
    // recorded.
    if let (Some(consent), Some(enabled)) = (&api.consent, stored.elevenlabs_enabled) {
        consent.store(enabled, Ordering::Relaxed);
        tracing::info!(
            enabled,
            actor = %caller.device_id,
            "ElevenLabs consent changed from the settings surface"
        );
    }

    Ok(Json(view(&api, &stored).await?))
}

/// `GET /api/v1/settings/node-voice` — what a node needs to answer to its name.
///
/// Available to any paired device rather than `ui` only: a satellite has to be
/// able to read this, and it is the one setting that is *about* the satellite.
/// It carries nothing else, which is why it is a separate route rather than a
/// scope exception on the one above — a node should not receive the spend, the
/// consent state, or the list of what else could be configured.
pub async fn get_node_voice(
    State(api): State<SettingsApi>,
) -> Result<Json<NodeVoiceSettingsDto>, Response> {
    let overrides = api.store.voice_overrides().await.map_err(storage_problem)?;
    Ok(Json(NodeVoiceSettingsDto {
        wake_word: api.wake_word(&overrides),
    }))
}
