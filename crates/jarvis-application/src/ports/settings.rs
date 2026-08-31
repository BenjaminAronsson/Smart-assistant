use super::shared::RepositoryError;
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::DeviceId;
use std::time::SystemTime;

/// The owner's overrides for the handful of settings the shell may change
/// (F8.8's voice section, F8.11's spend).
///
/// An *override* layer, not the configuration: `jarvisd.toml` supplies the
/// defaults and everything security-relevant, and `None` here means "whatever
/// the file says". That is why each field is an `Option` rather than a value —
/// clearing an override and choosing a value are different acts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoiceOverrides {
    pub wake_word: Option<String>,
    pub elevenlabs_enabled: Option<bool>,
}

/// Persistence for [`VoiceOverrides`] and the durable character budget.
///
/// Mutations co-transact their audit event (invariant 6): consenting to a
/// third-party egress path is exactly the kind of change that must not be
/// possible to make unrecorded.
#[async_trait::async_trait]
pub trait SettingsStore: Send + Sync {
    async fn voice_overrides(&self) -> Result<VoiceOverrides, RepositoryError>;

    /// Apply only the fields that are `Some`, leaving the rest as they were.
    async fn set_voice_overrides(
        &self,
        overrides: &VoiceOverrides,
        by_device: &DeviceId,
        at: SystemTime,
        audit: &AuditEvent,
    ) -> Result<VoiceOverrides, RepositoryError>;
}

/// The durable half of ADR-033's character budget (F8.11).
///
/// A port of its own, and narrow on purpose: the speech adapter needs to spend
/// against the budget, and nothing else. Handing it the whole
/// [`SettingsStore`] would hand a synthesiser the ability to rewrite the
/// consent gate that governs it.
///
/// Through F8.11 the budget was an in-process `AtomicU64`, which made "monthly
/// budget" untrue in the direction that matters: a daemon restarted daily had
/// no ceiling at all. The period is computed by the implementation so two
/// callers cannot disagree about which month a spend belongs to, and the
/// rollover needs no scheduled job.
#[async_trait::async_trait]
pub trait SpendLedger: Send + Sync {
    /// Add to this period's spend and return the **running total**.
    ///
    /// The total comes back from storage rather than from the caller's last
    /// read, so two concurrent utterances cannot both squeeze past the same
    /// remaining allowance.
    async fn reserve(&self, characters: u64) -> Result<u64, RepositoryError>;

    /// Give back a reservation whose request never happened. Never takes the
    /// period below zero.
    async fn refund(&self, characters: u64) -> Result<(), RepositoryError>;

    /// Characters spent so far this period.
    async fn spent(&self) -> Result<u64, RepositoryError>;
}
