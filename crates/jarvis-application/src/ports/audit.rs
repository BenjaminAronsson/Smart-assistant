use super::shared::RepositoryError;
use jarvis_domain::audit::AuditEvent;

/// A standalone append-only audit write (invariant 6). Unlike the store ports
/// above — which co-transact their audit with a domain row — a display
/// placement (FR-09/10) has no other row to write: issuing the directive *is*
/// the change, so its audit event is recorded on its own. Implementations open
/// a transaction, append to the hash chain, and commit; a placement that cannot
/// be audited must not be dispatched.
#[async_trait::async_trait]
pub trait AuditLog: Send + Sync {
    async fn record(&self, audit: &AuditEvent) -> Result<(), RepositoryError>;
}
