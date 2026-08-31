use jarvis_domain::secrecy::Redacted;

pub(crate) fn validate_secret_ref_named(field: &str, reference: &str) -> anyhow::Result<()> {
    // NEVER echo the rejected value: the failing case is precisely "someone
    // pasted a literal secret", and this error reaches stderr/journald.
    anyhow::ensure!(
        reference.starts_with("env:") || reference.starts_with("keyring:"),
        "{field} (scheme {:?}) is not a secret reference — secrets must be \
         `env:VAR` or `keyring:service/entry` references, never literal values \
         (invariant 5); the rejected value is withheld from this message",
        scheme_of(reference)
    );
    Ok(())
}

pub(crate) fn validate_secret_ref(reference: &str) -> anyhow::Result<()> {
    validate_secret_ref_named("database.url_secret", reference)
}

/// The scheme part of a reference — safe to echo; never the remainder.
///
/// A value with **no** `:` at all has no scheme, and returning "everything
/// before the first `:`" would then return the whole value. That is precisely
/// the case this function exists to protect (someone pasted a raw token or
/// password), so it must not be echoed: an opaque marker goes out instead.
///
/// A `:` alone is not enough either: a pasted literal password may well contain
/// one (`Summer:2026!`), and echoing "everything before the first colon" then
/// leaks a usable prefix of the secret into stderr/journald — the same leak in
/// smaller print (invariant 5).
///
/// Shape alone does not separate the two cases: `Summer` is a perfectly
/// well-formed RFC 3986 scheme. What separates them is *provenance* — so the
/// echo is drawn from a fixed, code-owned list of scheme names rather than from
/// the rejected value. A candidate that matches one of those names carries no
/// information the operator did not already read in this file; anything else is
/// withheld wholesale. The message stays actionable regardless, because the two
/// facts an operator needs — which field, and which schemes are accepted — are
/// always spelled out and never come from the value.
const WITHHELD_SCHEME: &str = "<withheld>";

/// Scheme names safe to repeat, because they *are* these literals: the two
/// Jarvis accepts (echoed when only the shape after the colon is wrong), plus
/// the near-misses an operator plausibly types into a `*_secret` field. A scheme
/// outside this list is not "unknown but harmless" — it may be the first half of
/// a password, so it is withheld.
const ECHOABLE_SCHEMES: &[&str] = &[
    "env",
    "keyring",
    "file",
    "vault",
    "secret",
    "op",
    "http",
    "https",
    "postgres",
    "postgresql",
];

fn scheme_of(reference: &str) -> &str {
    match reference.split_once(':') {
        Some((scheme, _)) => ECHOABLE_SCHEMES
            .iter()
            .find(|known| scheme.eq_ignore_ascii_case(known))
            .copied()
            .unwrap_or(WITHHELD_SCHEME),
        None => WITHHELD_SCHEME,
    }
}

/// Resolve a secret reference at the adapter boundary. The value comes back
/// [`Redacted`] so it cannot reach logs or serialization by accident.
pub fn resolve_secret_ref(reference: &str) -> anyhow::Result<Redacted<String>> {
    resolve_secret_ref_with(reference, |var| std::env::var(var).ok())
}

/// Resolve a secret without blocking an async runtime worker.
///
/// keyring's pure-Rust Secret Service backend presents the crate's synchronous
/// [`keyring::Entry`] facade by driving async D-Bus work internally. Its own
/// contract requires those calls to run on a separate thread when the caller
/// already owns a Tokio runtime; doing otherwise can deadlock during startup.
/// The task is awaited, so startup neither detaches work nor outlives a lookup.
pub async fn resolve_secret_ref_async(reference: &str) -> anyhow::Result<Redacted<String>> {
    let reference = reference.to_owned();
    tokio::task::spawn_blocking(move || resolve_secret_ref(&reference))
        .await
        .map_err(|_| anyhow::anyhow!("secret resolution task failed"))?
}

/// Injectable-lookup variant so tests never mutate process-global env
/// (`std::env::set_var` is `unsafe` in Rust 2024 and stays banned here).
pub fn resolve_secret_ref_with(
    reference: &str,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<Redacted<String>> {
    if let Some(var) = reference.strip_prefix("env:") {
        let value = env_lookup(var).ok_or_else(|| {
            anyhow::anyhow!("secret reference {reference:?}: environment variable {var} is not set")
        })?;
        Ok(Redacted::new(value))
    } else if reference.starts_with("keyring:") {
        let key = reference
            .strip_prefix("keyring:")
            .and_then(|value| value.split_once('/'))
            .filter(|(service, entry)| !service.is_empty() && !entry.is_empty())
            .ok_or_else(|| anyhow::anyhow!("keyring reference has invalid shape"))?;
        let entry = keyring::Entry::new(key.0, key.1)
            .map_err(|_| anyhow::anyhow!("keyring reference could not be opened"))?;
        let value = entry
            .get_password()
            .map_err(|_| anyhow::anyhow!("keyring reference could not be resolved"))?;
        Ok(Redacted::new(value))
    } else {
        // Same rule as validate_secret_ref: the value may BE a secret.
        anyhow::bail!(
            "secret reference with scheme {:?} is not supported (env: or keyring:); \
             the value is withheld from this message",
            scheme_of(reference)
        )
    }
}
