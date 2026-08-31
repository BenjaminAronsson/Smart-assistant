use serde::{Deserialize, Serialize};

/// `[voice]` (docs/09 §1, FR-13). Voice is opt-in: the browser may render its
/// push-to-talk affordance without this service-side pipeline, but no daemon
/// connection to an external speech service is created until enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_wyoming_stt")]
    pub wyoming_stt: String,
    /// Piper (or any Wyoming TTS) endpoint for the spoken response leg (F5.2).
    /// **Absent means no TTS**, deliberately: the round trip still works — the
    /// transcript starts a run and the answer streams as text — it is simply not
    /// spoken. The stricter default, matching every other outbound capability in
    /// this config (media, web search, MCP): opt in by naming the service.
    #[serde(default)]
    pub wyoming_tts: Option<String>,
    #[serde(default)]
    pub audio: VoiceAudioConfig,
    /// The word nodes answer to (ADR-032 §1/§4). Configuration rather than
    /// code; the shell may change it, but only to one of
    /// [`Self::wake_words_available`].
    #[serde(default = "default_wake_word")]
    pub wake_word: String,
    /// The words this household has **provisioned models for**.
    ///
    /// Declared here rather than discovered, because the models live on the
    /// satellites and the daemon cannot see their filesystems. It is what the
    /// settings surface offers, so a word absent from this list cannot be
    /// chosen — the failure it prevents is a house that silently goes deaf
    /// because somebody picked a word nothing has a model for.
    ///
    /// Defaults to what `infra/install/fetch-wake-assets.sh` installs, which
    /// includes the default word — so a fresh install answers to its name
    /// rather than reporting a missing model (ADR-032 §1, amended 2026-08-17).
    /// A word outside this list is a legitimate owner choice that costs a model
    /// training run; the settings surface will not offer one.
    #[serde(default = "default_wake_words_available")]
    pub wake_words_available: Vec<String>,
    /// `[voice.elevenlabs]` (F8.11, ADR-033). Absent means never.
    #[serde(default)]
    pub elevenlabs: ElevenLabsConfig,
}

fn default_wake_word() -> String {
    // Must stay in step with `jarvis_agent::wake::DEFAULT_WAKE_WORD` — the
    // daemon serves this to nodes, so a disagreement would have a node
    // answering to one word while the shell reported another.
    "hey jarvis".to_owned()
}

fn default_wake_words_available() -> Vec<String> {
    ["alexa", "hey jarvis", "hey mycroft"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// `[voice.elevenlabs]` — a third-party voice, off unless switched on.
///
/// **Switching this on is the consent** (ADR-033 §2): it is the moment the
/// house's spoken output starts leaving the house, so it is one deliberate,
/// reversible act rather than a per-utterance prompt. Everything the local
/// voice does keeps working when it is off, and when it fails.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElevenLabsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// A **keyring reference** (`keyring:service/entry`), never a literal key
    /// (invariant 5). Resolved at the adapter boundary.
    #[serde(default)]
    pub api_key_ref: Option<String>,
    #[serde(default)]
    pub voice_id: Option<String>,
    #[serde(default = "default_elevenlabs_model")]
    pub model_id: String,
    /// Characters per process lifetime. A ceiling that makes runaway spend
    /// impossible; exhaustion falls back to the local voice rather than
    /// failing a turn (ADR-033 §5).
    #[serde(default = "default_elevenlabs_budget")]
    pub character_budget: u64,
}

/// Written by hand rather than derived: `Config::from_figment` serializes the
/// defaults in as a base layer, so a derived `Default` would put an explicit
/// `character_budget = 0` underneath the `serde(default = …)` and win — which
/// showed up as a fully-configured file being refused for having no budget.
impl Default for ElevenLabsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key_ref: None,
            voice_id: None,
            model_id: default_elevenlabs_model(),
            character_budget: default_elevenlabs_budget(),
        }
    }
}

fn default_elevenlabs_model() -> String {
    "eleven_flash_v2_5".to_owned()
}

fn default_elevenlabs_budget() -> u64 {
    100_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceAudioConfig {
    #[serde(default = "default_voice_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_voice_channels")]
    pub channels: u16,
    #[serde(default = "default_voice_format")]
    pub format: String,
}

fn default_wyoming_stt() -> String {
    "tcp://127.0.0.1:10300".to_owned()
}

fn default_voice_sample_rate() -> u32 {
    16_000
}

fn default_voice_channels() -> u16 {
    1
}

fn default_voice_format() -> String {
    "s16le".to_owned()
}

impl Default for VoiceAudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: default_voice_sample_rate(),
            channels: default_voice_channels(),
            format: default_voice_format(),
        }
    }
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            wyoming_stt: default_wyoming_stt(),
            wyoming_tts: None,
            audio: VoiceAudioConfig::default(),
            // Hand-written to match the `serde(default = …)` above, for the
            // reason recorded on `ElevenLabsConfig::default`: `from_figment`
            // serializes these defaults in as a base layer, so a field left out
            // here would put an empty list *under* the annotation and win.
            wake_word: default_wake_word(),
            wake_words_available: default_wake_words_available(),
            elevenlabs: ElevenLabsConfig::default(),
        }
    }
}
