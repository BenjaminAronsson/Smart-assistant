//! Owner-tunable runtime settings (F8.8's voice section, F8.11's spend).
//!
//! The narrow slice of configuration the shell may read and change. Everything
//! security-relevant — secret references, bind address, TLS, allowlists — stays
//! in `jarvisd.toml` and is not represented here at all: a DTO that cannot
//! express a setting is a surface that cannot change it.
//!
//! Nothing in this module is or carries a credential. The ElevenLabs API key
//! remains a keyring reference in the config file (invariant 5); what is
//! exchanged here is whether the owner has **consented** to using it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What the shell shows in Settings → Voice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VoiceSettingsDto {
    /// The word a node answers to (ADR-032 §1).
    pub wake_word: String,
    /// The words this installation actually has a model for.
    ///
    /// Sent rather than hardcoded in the client because it depends on what the
    /// installer provisioned (ADR-032 consequence 3), and because offering a
    /// word with no model would be offering a node that goes deaf.
    pub available_wake_words: Vec<String>,
    /// Empty when every offered word has a model. Names the configured word
    /// when it has none — the "Andy" case (ADR-032 §1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_word_warning: Option<String>,
    pub elevenlabs: ElevenLabsSettingsDto,
}

/// The third-party speech synthesiser's consent gate and spend (ADR-033).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ElevenLabsSettingsDto {
    /// Whether this daemon is configured for it at all — an API key reference
    /// and a voice. The toggle is refused when this is false, because consent
    /// to use something that is not configured would be consent to nothing.
    pub configured: bool,
    /// ADR-033 §2's opt-in gate. Off by default, always.
    pub enabled: bool,
    /// Characters spent this month, and the ceiling. Durable across restarts —
    /// a budget that resets whenever the daemon does is not a monthly budget.
    pub spent_characters: u64,
    pub character_budget: u64,
    /// `YYYY-MM`, UTC — the period `spent_characters` covers.
    pub period: String,
    /// The voice that speaks when this is off, the network is down, or the
    /// budget is spent. Never absent (ADR-033 §3): an alarm must still ring.
    pub local_fallback: String,
}

/// A change to the voice settings.
///
/// Every field optional and absent-means-unchanged, so the shell can send one
/// toggle without restating the rest and racing another tab.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateVoiceSettingsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_word: Option<String>,
    /// Turning this **on** is consent to send spoken text to a third party
    /// (ADR-033). It is refused unless the daemon is configured for it and a
    /// local fallback exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevenlabs_enabled: Option<bool>,
}

/// What a node asks for at startup so it answers to the configured word
/// (ADR-032 §4 — the word is configuration, so it cannot live only in the
/// node's own environment).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeVoiceSettingsDto {
    pub wake_word: String,
}
