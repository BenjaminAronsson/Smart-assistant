use super::shared::RepositoryError;
use jarvis_domain::audit::AuditEvent;

/// Storage for automations and their execution history (FR-17, F8.6).
///
/// Note what this port does **not** offer: any way to store or read a policy
/// decision. An automation is a stored intention, and its authority is resolved
/// at fire time from its creator's current scopes — see
/// [`crate::automations::decide_at_fire_time`].
#[async_trait::async_trait]
pub trait AutomationStore: Send + Sync {
    /// Persist a new automation together with its audit row (invariant 6).
    async fn create(
        &self,
        automation: &jarvis_domain::automations::Automation,
        audit: &jarvis_domain::audit::AuditEvent,
    ) -> Result<(), RepositoryError>;

    /// Every enabled automation — what the scheduler sweeps.
    async fn list_enabled(
        &self,
    ) -> Result<Vec<jarvis_domain::automations::Automation>, RepositoryError>;

    /// Every automation, enabled or not — what the settings surface lists.
    async fn list_all(
        &self,
    ) -> Result<Vec<jarvis_domain::automations::Automation>, RepositoryError>;

    /// Arm or disarm an automation, with its audit row in the same transaction
    /// (invariant 6).
    ///
    /// Enabling is the act that arms an unattended actor holding its creator's
    /// authority; disabling is how a household silences one. Neither may happen
    /// unrecorded — `create` audits, and these are the same kind of act.
    async fn set_enabled(
        &self,
        id: &jarvis_domain::ids::AutomationId,
        enabled: bool,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError>;

    async fn delete(
        &self,
        id: &jarvis_domain::ids::AutomationId,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError>;

    /// Record one firing and stamp `last_fired_at`, in one transaction.
    ///
    /// Together, because a firing that is rate-limited but not recorded — or
    /// recorded but not rate-limited — is a bug that only shows up as a
    /// flapping sensor turning the lights on forty times.
    /// Record one firing, stamp `last_fired_at`, and append the audit event —
    /// all in one transaction.
    ///
    /// The audit row is the third thing that must not drift from the other two:
    /// an automation is the one surface in the system that acts on the world
    /// with nobody watching, so "it ran" has to be answerable from the
    /// append-only trail and not only from a table the automation module owns.
    async fn record_execution(
        &self,
        execution: &jarvis_domain::automations::AutomationExecution,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError>;

    /// Most recent firings first — the history FR-17 asks for.
    async fn history(
        &self,
        id: &jarvis_domain::ids::AutomationId,
        limit: i64,
    ) -> Result<Vec<jarvis_domain::automations::AutomationExecution>, RepositoryError>;

    /// Stamp "the daemon was alive at this instant" (M8b).
    ///
    /// Written on every sweep tick, so the resolution of the restart report is
    /// the sweep interval. Deliberately not tied to graceful shutdown: the
    /// downtime worth reporting is the one nobody planned, and a daemon that
    /// was killed is exactly the case a shutdown hook does not cover.
    async fn record_heartbeat(&self, at: std::time::SystemTime) -> Result<(), RepositoryError>;

    /// When the daemon was last known to be running, if ever.
    ///
    /// `None` on a first start — there is no downtime to report when there was
    /// no uptime before it.
    async fn last_heartbeat(&self) -> Result<Option<std::time::SystemTime>, RepositoryError>;
}
