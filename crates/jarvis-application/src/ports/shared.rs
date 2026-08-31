use jarvis_domain::grants::Sha256;

/// `sha256(canonical_form(arguments))` — the same normalization and hash the
/// grant minter binds (docs/06 §4).
///
/// A port rather than a function because the application layer computes no
/// crypto (invariant 3; `sha2` lives in infra). It exists to close **D-M5-4**:
/// through M5 a `tool.executed` audit row named only the tool, so the
/// append-only trail could say *that* `home.set_light` ran and never *which
/// light*. Binding the argument hash makes an executed effect answerable after
/// the fact without storing the arguments themselves, which may be sensitive
/// (invariant 5) — the same trade the grant table already makes.
pub trait ArgumentDigest: Send + Sync {
    fn digest(&self, arguments: &jarvis_domain::tools::CanonicalValue) -> Sha256;
}

#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error("conflict: {0}")]
    Conflict(String),
    /// Same idempotency key, different payload (docs/05 §7
    /// `idempotency.conflict`).
    #[error("idempotency key reused with a different payload")]
    IdempotencyConflict,
    #[error("storage failure: {0}")]
    Storage(String),
}
