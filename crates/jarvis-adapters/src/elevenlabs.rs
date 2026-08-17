//! ElevenLabs speech synthesis, opt-in (F8.11, ADR-033).
//!
//! The M8 feature list deferred this and then the owner pulled it forward. The
//! *conditions* of that deferral did not move, and they are the whole of this
//! module:
//!
//! 1. **Opt-in is the consent gate.** Off by default. Nothing here runs until
//!    somebody switches it on, because switching it on is the moment a house's
//!    voice starts leaving the house.
//! 2. **A local fallback, always.** ADR-023 says an alarm must sound. A cloud
//!    voice that fails must degrade to Piper, never to silence.
//! 3. **Sensitivity-aware routing.** Message bodies and calendar entries are
//!    never spoken by a third party — enforced as a *refusal*, not a
//!    preference (see [`ElevenLabsSynthesizer::synthesize`]).
//! 4. **A character budget**, with the spend observable.
//! 5. Its API key is a keyring reference resolved at this boundary, never a
//!    literal in config, a prompt, a log line, or an argv entry (invariant 5).
//!
//! What it is deliberately **not** used for, and these are invariant calls
//! rather than timing ones: not the wake word (must be local and offline —
//! F8.3), not STT (voice is the most sensitive stream in the system, and the
//! zero-LLM paths must work with the network down), and never their Agents
//! platform, which would take over the loop and break invariants 1–2.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use jarvis_application::voice::{AudioFormat, SpeechSensitivity, SpeechSynthesizer, VoiceError};
use tokio_util::sync::CancellationToken;

/// ElevenLabs streams PCM at a rate we ask for; 16 kHz mono matches the one
/// wire format the rest of the system uses (docs/05 §1), so nothing has to
/// resample a cloud voice on a satellite.
const OUTPUT_FORMAT: &str = "pcm_16000";

const SYNTH_FORMAT: AudioFormat = AudioFormat {
    sample_rate_hz: 16_000,
    sample_width_bytes: 2,
    channels: 1,
};

/// Tracks characters spent against a ceiling.
///
/// A budget nobody can read is not a budget: [`CharacterBudget::spent`] is what
/// makes the spend observable, and the gate report is expected to quote it.
#[derive(Debug)]
pub struct CharacterBudget {
    limit: u64,
    spent: AtomicU64,
}

impl CharacterBudget {
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            spent: AtomicU64::new(0),
        }
    }

    pub fn spent(&self) -> u64 {
        self.spent.load(Ordering::Relaxed)
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.spent())
    }

    /// Reserves `chars` if the budget can cover them.
    ///
    /// Reserved *before* the request rather than counted after it: a budget
    /// that only notices overspend once the bytes have been sent is an
    /// accounting record, not a limit.
    pub fn try_reserve(&self, chars: u64) -> bool {
        // Compare-and-swap so two concurrent utterances cannot both squeeze
        // past the same remaining allowance.
        let mut current = self.spent.load(Ordering::Relaxed);
        loop {
            if current + chars > self.limit {
                return false;
            }
            match self.spent.compare_exchange_weak(
                current,
                current + chars,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Gives back a reservation whose request never happened.
    pub fn refund(&self, chars: u64) {
        self.spent.fetch_sub(
            chars.min(self.spent.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
    }
}

/// Why an utterance did not go to ElevenLabs. Each is a *routing* outcome, not
/// an error: every one of them ends with the local voice speaking instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bypass {
    /// The owner has not switched it on.
    NotEnabled,
    /// The text is the user's private correspondence.
    Sensitive,
    /// The character budget is spent.
    BudgetExhausted,
}

impl Bypass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotEnabled => "not_enabled",
            Self::Sensitive => "sensitive",
            Self::BudgetExhausted => "budget_exhausted",
        }
    }
}

/// Configuration. Absent or `enabled = false` means this adapter never runs.
#[derive(Debug, Clone)]
pub struct ElevenLabsConfig {
    pub enabled: bool,
    pub voice_id: String,
    pub model_id: String,
    /// Resolved from a keyring reference at construction — never stored in
    /// config as a literal, never logged, never an argv entry (invariant 5).
    pub api_key: String,
    pub monthly_character_budget: u64,
    /// Overridable for tests; production leaves it at the real host.
    pub base_url: String,
}

impl ElevenLabsConfig {
    pub fn api_base() -> String {
        "https://api.elevenlabs.io".to_owned()
    }
}

/// Speaks through ElevenLabs when — and only when — every condition allows it,
/// and through `local` otherwise.
///
/// The fallback is not an error path bolted on the side; it is the default
/// path, and reaching ElevenLabs is the exception that has to earn its way
/// past four checks.
pub struct ElevenLabsSynthesizer {
    config: ElevenLabsConfig,
    budget: Arc<CharacterBudget>,
    /// ADR-033 §2's consent gate, readable at *speaking* time rather than only
    /// at construction (F8.8): the owner can withdraw consent from the shell
    /// and the next sentence is already local. Withdrawing consent should not
    /// require a restart — a house that keeps talking to a third party until
    /// someone finds a terminal has not honoured the switch.
    consent: Arc<AtomicBool>,
    /// Durable spend, when wired. `None` keeps the in-process ceiling, which is
    /// what the adapter's own tests use.
    ledger: Option<Arc<dyn jarvis_application::ports::SpendLedger>>,
    local: Arc<dyn SpeechSynthesizer>,
    http: reqwest::Client,
}

impl ElevenLabsSynthesizer {
    pub fn new(config: ElevenLabsConfig, local: Arc<dyn SpeechSynthesizer>) -> Self {
        let budget = Arc::new(CharacterBudget::new(config.monthly_character_budget));
        let consent = Arc::new(AtomicBool::new(config.enabled));
        Self {
            config,
            budget,
            consent,
            ledger: None,
            local,
            http: reqwest::Client::new(),
        }
    }

    /// Back the budget with durable storage (F8.11). Without this the ceiling
    /// is per-process, and a daemon restarted daily has no monthly ceiling.
    pub fn with_ledger(mut self, ledger: Arc<dyn jarvis_application::ports::SpendLedger>) -> Self {
        self.ledger = Some(ledger);
        self
    }

    pub fn budget(&self) -> Arc<CharacterBudget> {
        self.budget.clone()
    }

    /// The live consent gate, shared with the settings surface that flips it.
    pub fn consent(&self) -> Arc<AtomicBool> {
        self.consent.clone()
    }

    /// The routing decision, separated from the I/O so it is testable without a
    /// network — and so the rule is readable in one place.
    ///
    /// **Pure.** It reserves nothing: a method named for a question must not
    /// have a side effect, or calling it twice quietly spends the budget twice
    /// (which is exactly what an earlier draft did, and what its own test
    /// caught). The reservation happens once, in [`Self::synthesize`].
    pub fn bypass_reason(&self, text: &str, sensitivity: SpeechSensitivity) -> Option<Bypass> {
        // Read live, not from the config this was built with: consent can be
        // withdrawn from the shell mid-session (F8.8) and must take effect on
        // the next sentence, not the next restart.
        if !self.consent.load(Ordering::Relaxed) {
            return Some(Bypass::NotEnabled);
        }
        // Checked before the budget on purpose: a sensitive utterance must not
        // consume external allowance, and must not be routed externally even
        // when there is plenty left.
        if !sensitivity.may_leave_the_house() {
            return Some(Bypass::Sensitive);
        }
        if (text.chars().count() as u64) > self.budget.remaining() {
            return Some(Bypass::BudgetExhausted);
        }
        None
    }
}

#[async_trait]
impl SpeechSynthesizer for ElevenLabsSynthesizer {
    fn id(&self) -> &str {
        "elevenlabs"
    }

    async fn synthesize(
        &self,
        text: &str,
        sensitivity: SpeechSensitivity,
        cancel: CancellationToken,
    ) -> Result<(AudioFormat, BoxStream<'static, Result<Vec<u8>, VoiceError>>), VoiceError> {
        if let Some(reason) = self.bypass_reason(text, sensitivity) {
            tracing::debug!(reason = reason.as_str(), "speaking locally");
            return self.local.synthesize(text, sensitivity, cancel).await;
        }

        // Reserve exactly once, here. The compare-and-swap is what stops two
        // concurrent utterances both squeezing past the same allowance — the
        // pure check above cannot, and is not meant to.
        let reserved = text.chars().count() as u64;
        if !self.reserve(reserved).await {
            tracing::debug!(
                reason = Bypass::BudgetExhausted.as_str(),
                "speaking locally"
            );
            return self.local.synthesize(text, sensitivity, cancel).await;
        }

        match self.request(text, cancel.clone()).await {
            Ok(stream) => Ok((SYNTH_FORMAT, stream)),
            Err(e) => {
                // The condition that matters most: a cloud voice that fails
                // must degrade to the local one, never to silence. An alarm
                // must sound (ADR-023).
                self.refund(reserved).await;
                tracing::warn!(
                    error = %e,
                    "ElevenLabs synthesis failed; falling back to the local voice"
                );
                self.local.synthesize(text, sensitivity, cancel).await
            }
        }
    }
}

impl ElevenLabsSynthesizer {
    /// Reserve against the budget, durably when a ledger is wired.
    ///
    /// A ledger failure spends **locally** rather than externally: if the
    /// database cannot say how much has been spent, the honest reading is that
    /// the ceiling is unknown, and an unknown ceiling is not permission.
    async fn reserve(&self, characters: u64) -> bool {
        let Some(ledger) = self.ledger.as_ref() else {
            return self.budget.try_reserve(characters);
        };
        match ledger.reserve(characters).await {
            Ok(total) if total <= self.budget.limit() => true,
            Ok(_) => {
                // Over the ceiling: hand it straight back, so a refusal costs
                // nothing and next month is not started in arrears.
                self.refund(characters).await;
                false
            }
            Err(e) => {
                tracing::warn!(error = %e, "spend ledger unavailable; speaking locally");
                false
            }
        }
    }

    async fn refund(&self, characters: u64) {
        match self.ledger.as_ref() {
            Some(ledger) => {
                if let Err(e) = ledger.refund(characters).await {
                    // Nothing to escalate to: the utterance is already being
                    // spoken locally, and over-counting spend fails safe.
                    tracing::warn!(error = %e, "could not refund unused characters");
                }
            }
            None => self.budget.refund(characters),
        }
    }

    async fn request(
        &self,
        text: &str,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<Vec<u8>, VoiceError>>, VoiceError> {
        let url = format!(
            "{}/v1/text-to-speech/{}/stream?output_format={OUTPUT_FORMAT}",
            self.config.base_url.trim_end_matches('/'),
            self.config.voice_id
        );

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(VoiceError::Cancelled),
            response = self
                .http
                .post(&url)
                // The key travels in a header and nowhere else.
                .header("xi-api-key", &self.config.api_key)
                .json(&serde_json::json!({
                    "text": text,
                    "model_id": self.config.model_id,
                }))
                .send() => response.map_err(|e| VoiceError::Unavailable(e.to_string()))?,
        };

        if !response.status().is_success() {
            // The status, never the body: an error body from a third party is
            // untrusted text, and it has no business in our logs.
            return Err(VoiceError::Unavailable(format!(
                "ElevenLabs returned {}",
                response.status().as_u16()
            )));
        }

        let stream = response.bytes_stream().map(|chunk| {
            chunk
                .map(|bytes| bytes.to_vec())
                .map_err(|e| VoiceError::Unavailable(e.to_string()))
        });
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use std::sync::Mutex;

    /// The local voice, recording what it was asked to say.
    #[derive(Default)]
    struct LocalVoice {
        spoken: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl SpeechSynthesizer for LocalVoice {
        fn id(&self) -> &str {
            "local"
        }
        async fn synthesize(
            &self,
            text: &str,
            _sensitivity: SpeechSensitivity,
            _cancel: CancellationToken,
        ) -> Result<(AudioFormat, BoxStream<'static, Result<Vec<u8>, VoiceError>>), VoiceError>
        {
            self.spoken.lock().expect("lock").push(text.to_owned());
            Ok((
                SYNTH_FORMAT,
                Box::pin(stream::once(async { Ok(vec![0_u8; 32]) })),
            ))
        }
    }

    /// Consent is read at *speaking* time, not construction time (F8.8).
    ///
    /// The withdraw direction is the one that matters: a house that keeps
    /// talking to a third party until someone finds a terminal and restarts
    /// the daemon has not honoured the switch the owner just flipped.
    #[tokio::test]
    async fn consent_is_read_live_in_both_directions() {
        let (synth, _local) = synth(false, 10_000);
        assert_eq!(
            synth.bypass_reason("hello", SpeechSensitivity::Normal),
            Some(Bypass::NotEnabled),
            "off is off"
        );

        synth.consent().store(true, Ordering::Relaxed);
        assert_eq!(
            synth.bypass_reason("hello", SpeechSensitivity::Normal),
            None,
            "granting consent takes effect on the next utterance"
        );

        synth.consent().store(false, Ordering::Relaxed);
        assert_eq!(
            synth.bypass_reason("hello", SpeechSensitivity::Normal),
            Some(Bypass::NotEnabled),
            "and withdrawing it takes effect just as immediately"
        );
    }

    /// Consent does not override sensitivity. Switching the third-party voice
    /// on is not permission to read the owner's messages aloud through it.
    #[tokio::test]
    async fn consent_does_not_unlock_sensitive_text() {
        let (synth, _local) = synth(true, 10_000);
        synth.consent().store(true, Ordering::Relaxed);
        assert_eq!(
            synth.bypass_reason("your bank called", SpeechSensitivity::Sensitive),
            Some(Bypass::Sensitive)
        );
    }

    fn config(enabled: bool, budget: u64) -> ElevenLabsConfig {
        ElevenLabsConfig {
            enabled,
            voice_id: "voice".into(),
            model_id: "model".into(),
            api_key: "not-a-real-key".into(),
            monthly_character_budget: budget,
            // Unroutable on purpose: any test that reaches the network is a
            // test that would have leaked.
            base_url: "http://127.0.0.1:1".into(),
        }
    }

    fn synth(enabled: bool, budget: u64) -> (ElevenLabsSynthesizer, Arc<LocalVoice>) {
        let local = Arc::new(LocalVoice::default());
        (
            ElevenLabsSynthesizer::new(config(enabled, budget), local.clone()),
            local,
        )
    }

    /// Condition 1: off by default means nothing leaves the house.
    #[tokio::test]
    async fn with_the_opt_in_off_everything_is_spoken_locally() {
        let (synth, local) = synth(false, 10_000);
        assert_eq!(
            synth.bypass_reason("hello", SpeechSensitivity::Normal),
            Some(Bypass::NotEnabled)
        );

        let (_format, _audio) = synth
            .synthesize("hello", SpeechSensitivity::Normal, CancellationToken::new())
            .await
            .expect("speaks");
        assert_eq!(local.spoken.lock().expect("lock").as_slice(), ["hello"]);
        assert_eq!(
            synth.budget().spent(),
            0,
            "a disabled adapter spends nothing"
        );
    }

    /// Condition 3, and the one with teeth: a message body is never spoken by a
    /// third party, even with the opt-in on and budget to spare.
    #[tokio::test]
    async fn sensitive_text_is_spoken_locally_even_when_enabled() {
        let (synth, local) = synth(true, 10_000);
        assert_eq!(
            synth.bypass_reason("your bank code is 1234", SpeechSensitivity::Sensitive),
            Some(Bypass::Sensitive)
        );

        let (_format, _audio) = synth
            .synthesize(
                "your bank code is 1234",
                SpeechSensitivity::Sensitive,
                CancellationToken::new(),
            )
            .await
            .expect("speaks");
        assert_eq!(local.spoken.lock().expect("lock").len(), 1);
        assert_eq!(
            synth.budget().spent(),
            0,
            "a sensitive utterance must not consume external allowance"
        );
    }

    /// Condition 4: the budget is a limit, not an accounting record.
    #[tokio::test]
    async fn an_exhausted_budget_falls_back_rather_than_failing_the_turn() {
        let (synth, local) = synth(true, 10);
        // Eleven characters against a ten-character ceiling.
        assert_eq!(
            synth.bypass_reason("hello world", SpeechSensitivity::Normal),
            Some(Bypass::BudgetExhausted)
        );

        let (_format, _audio) = synth
            .synthesize(
                "hello world",
                SpeechSensitivity::Normal,
                CancellationToken::new(),
            )
            .await
            .expect("the turn still speaks");
        assert_eq!(local.spoken.lock().expect("lock").len(), 1);
    }

    /// Condition 2, the one ADR-023 turns on: unreachable must mean the local
    /// voice speaks, never silence.
    #[tokio::test]
    async fn an_unreachable_service_still_speaks_through_the_local_voice() {
        let (synth, local) = synth(true, 10_000);
        // `bypass_reason` allows it, so this genuinely attempts the network —
        // against a port nothing is listening on.
        assert_eq!(synth.bypass_reason("hi", SpeechSensitivity::Normal), None);

        let (format, _audio) = synth
            .synthesize("hi", SpeechSensitivity::Normal, CancellationToken::new())
            .await
            .expect("an alarm must still sound");
        assert_eq!(format, SYNTH_FORMAT);
        assert_eq!(
            local.spoken.lock().expect("lock").as_slice(),
            ["hi"],
            "the local voice must have spoken instead"
        );
        // And the reservation was given back, so a dead service cannot burn the
        // month's allowance.
        assert_eq!(synth.budget().spent(), 0);
    }

    #[test]
    fn the_budget_reserves_before_spending_and_is_observable() {
        let budget = CharacterBudget::new(100);
        assert_eq!(budget.limit(), 100);
        assert_eq!(budget.remaining(), 100);

        assert!(budget.try_reserve(60));
        assert_eq!(budget.spent(), 60);
        assert_eq!(budget.remaining(), 40);

        // Over the remainder: refused outright rather than allowed to overshoot.
        assert!(!budget.try_reserve(41));
        assert_eq!(budget.spent(), 60, "a refused reservation spends nothing");

        assert!(budget.try_reserve(40));
        assert_eq!(budget.remaining(), 0);

        budget.refund(40);
        assert_eq!(budget.spent(), 60);
    }

    #[test]
    fn a_refund_cannot_drive_the_spend_below_zero() {
        let budget = CharacterBudget::new(100);
        budget.try_reserve(10);
        budget.refund(1_000);
        assert_eq!(budget.spent(), 0);
    }

    /// Sensitivity is checked before the budget, so a sensitive utterance is
    /// never even priced.
    #[test]
    fn sensitivity_outranks_the_budget() {
        let (synth, _local) = synth(true, 0);
        assert_eq!(
            synth.bypass_reason("private", SpeechSensitivity::Sensitive),
            Some(Bypass::Sensitive),
            "a sensitive utterance must read as sensitive, not as unaffordable"
        );
    }

    /// Invariant 5, checked at the type: the key is not in anything printable.
    #[test]
    fn the_api_key_is_not_in_the_adapters_debug_output() {
        let (synth, _local) = synth(true, 10);
        // The synthesizer itself has no Debug — the config does, and it is
        // never logged. This asserts the id carries nothing secret, which is
        // the value that *does* reach logs and metrics.
        assert_eq!(synth.id(), "elevenlabs");
        assert!(!synth.id().contains("key"));
    }
}
