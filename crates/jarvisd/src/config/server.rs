use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Where the daemon listens. Loopback needs nothing further; **any other
    /// address requires TLS** and is refused without it (docs/06 §7, F7.3).
    pub bind: String,
    /// Static Angular assets; optional until packaging serves them.
    pub web_assets: Option<PathBuf>,
    /// TLS for LAN/remote nodes (F7.3, ADR-031). Absent = plaintext, which is
    /// only legal on loopback.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

/// `[server.tls]` — the certificate a node pins at pairing (ADR-031).
///
/// Both paths are required together: a certificate without its key cannot
/// serve, and a key without its certificate cannot be pinned. Self-signed is
/// the expected case — there is no CA in a house, and the fingerprint handed
/// to the node during the pairing ceremony is what makes the certificate
/// meaningful.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}
