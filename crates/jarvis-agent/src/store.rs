//! Where a node's credentials live (ADR-031 §1: "keyring where available, else
//! a 0600 file").
//!
//! One bundle, one entry, saved atomically — a node with a token but no private
//! key, or a fingerprint from a previous daemon, is worse than a node with
//! nothing, because it fails at connect time instead of at pair time.
//!
//! Nothing here is ever logged. [`Credentials`] deliberately does not derive
//! `Debug`; the only way to see a secret is to name the field.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The keyring service name and entry, and the basename of the file fallback.
const SERVICE: &str = "jarvis-agent";
const ENTRY: &str = "node-credentials";

/// Everything a paired node needs to reconnect without the owner present.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    /// Base URL the node paired against, e.g. `https://jarvis.lan:8741`.
    pub server_url: String,
    /// Base64 of the Ed25519 **private** seed (32 bytes). Never leaves this
    /// node; never crosses the wire in either direction (ADR-031 §1).
    pub private_key: String,
    /// The opaque device bearer token (ADR-031 §3).
    pub device_token: String,
    pub device_id: String,
    /// The class the server *assigned*. A node is told its authority, it never
    /// infers it (docs/05 §6.3) — so this is read, not chosen.
    pub device_class: String,
    /// `sha256` of the daemon certificate's DER, lowercase hex. `None` only for
    /// a plaintext loopback daemon, where there is no certificate to pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_fingerprint: Option<String>,
}

impl Credentials {
    /// Whether this node must speak TLS and pin.
    pub fn is_tls(&self) -> bool {
        self.server_url.starts_with("https://")
    }
}

/// Load/save/clear for the credential bundle.
///
/// A trait so the tests can exercise the pairing flow end to end without an
/// OS keyring — CI has no secret service, and a test that skipped itself there
/// would be no evidence at all.
pub trait CredentialStore: Send + Sync {
    fn load(&self) -> Result<Option<Credentials>>;
    fn save(&self, credentials: &Credentials) -> Result<()>;
    fn clear(&self) -> Result<()>;
    /// Human-readable name of the backend actually in use, for the startup log.
    fn backend(&self) -> &str;
}

/// The OS keyring, with a 0600 file fallback (ADR-031 §1).
///
/// The fallback is chosen once, at construction, and named in the startup log:
/// an owner who thinks their key is in the keyring when it is in a file has
/// been told something false about their own threat model.
#[derive(Clone)]
pub struct KeyringStore {
    fallback: Option<PathBuf>,
}

impl KeyringStore {
    /// Probes the keyring once. If it cannot be reached — a headless node with
    /// no session bus is the ordinary case, not an exotic one — falls back to a
    /// 0600 file under `$JARVIS_AGENT_STATE_DIR`, else `$XDG_DATA_HOME`, else
    /// `~/.local/share`.
    pub fn open() -> Result<Self> {
        match keyring_entry().and_then(|entry| probe(&entry)) {
            Ok(()) => Ok(Self { fallback: None }),
            Err(e) => {
                let path = fallback_path()?;
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "OS keyring unavailable; falling back to a 0600 file (ADR-031 §1)"
                );
                Ok(Self {
                    fallback: Some(path),
                })
            }
        }
    }

    /// A store pinned to a file, for tests and for nodes whose owner prefers it.
    pub fn with_file(path: PathBuf) -> Self {
        Self {
            fallback: Some(path),
        }
    }
}

impl CredentialStore for KeyringStore {
    fn load(&self) -> Result<Option<Credentials>> {
        let raw = match &self.fallback {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(raw) => raw,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e).context("reading the credential file"),
            },
            None => match keyring_entry()?.get_password() {
                Ok(raw) => raw,
                Err(keyring::Error::NoEntry) => return Ok(None),
                Err(e) => return Err(anyhow::anyhow!("reading the keyring entry: {e}")),
            },
        };
        // A corrupt bundle is a hard error, never a silent re-pair: losing a
        // key quietly is how a node ends up with two device rows.
        let credentials =
            serde_json::from_str(&raw).context("stored credentials are not readable")?;
        Ok(Some(credentials))
    }

    fn save(&self, credentials: &Credentials) -> Result<()> {
        let raw = serde_json::to_string(credentials).context("encoding credentials")?;
        match &self.fallback {
            Some(path) => write_private(path, &raw),
            None => keyring_entry()?
                .set_password(&raw)
                .map_err(|e| anyhow::anyhow!("writing the keyring entry: {e}")),
        }
    }

    fn clear(&self) -> Result<()> {
        match &self.fallback {
            Some(path) => match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e).context("removing the credential file"),
            },
            None => match keyring_entry()?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(anyhow::anyhow!("deleting the keyring entry: {e}")),
            },
        }
    }

    fn backend(&self) -> &str {
        match &self.fallback {
            Some(_) => "0600 file",
            None => "OS keyring",
        }
    }
}

fn keyring_entry() -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, ENTRY).map_err(|e| anyhow::anyhow!("opening the keyring: {e}"))
}

/// Round-trips a marker to prove the keyring is actually usable.
///
/// `Entry::new` succeeds against a backend that will fail on first use, so
/// construction alone proves nothing.
fn probe(entry: &keyring::Entry) -> Result<()> {
    match entry.get_password() {
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

fn fallback_path() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("JARVIS_AGENT_STATE_DIR") {
        return Ok(PathBuf::from(dir).join("credentials.json"));
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        .context("neither JARVIS_AGENT_STATE_DIR, XDG_DATA_HOME nor HOME is set")?;
    Ok(base.join("jarvis-agent").join("credentials.json"))
}

/// Writes 0600, creating the directory 0700, and *never* widening an existing
/// file's mode by writing through it.
fn write_private(path: &std::path::Path, contents: &str) -> Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating the credential directory")?;
        set_mode(parent, 0o700)?;
    }
    // Write to a fresh temporary file with the right mode from the start, then
    // rename: a reader never sees a half-written bundle, and the secret is
    // never briefly world-readable.
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).context("creating the credential file")?;
    file.write_all(contents.as_bytes())
        .context("writing credentials")?;
    file.sync_all().context("flushing credentials")?;
    drop(file);
    std::fs::rename(&tmp, path).context("installing the credential file")?;
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .context("tightening permissions")
}

#[cfg(not(unix))]
fn set_mode(_path: &std::path::Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Credentials {
        Credentials {
            server_url: "https://jarvis.lan:8741".into(),
            private_key: "cHJpdmF0ZQ==".into(),
            device_token: "a-secret-token".into(),
            device_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            device_class: "room-node".into(),
            server_fingerprint: Some("ab".repeat(32)),
        }
    }

    #[test]
    fn a_bundle_round_trips_through_the_file_store() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = KeyringStore::with_file(dir.path().join("nested").join("credentials.json"));

        assert!(store.load().expect("empty load").is_none());
        store.save(&sample()).expect("save");

        let loaded = store.load().expect("load").expect("present");
        assert_eq!(loaded.device_token, "a-secret-token");
        assert_eq!(loaded.device_class, "room-node");
        assert_eq!(
            loaded.server_fingerprint.as_deref(),
            Some(&*"ab".repeat(32))
        );

        store.clear().expect("clear");
        assert!(store.load().expect("after clear").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn the_credential_file_is_0600_and_its_directory_0700() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("state").join("credentials.json");
        KeyringStore::with_file(path.clone())
            .save(&sample())
            .expect("save");

        let file_mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(file_mode & 0o777, 0o600, "credential file must be 0600");
        let dir_mode = std::fs::metadata(path.parent().expect("parent"))
            .expect("stat dir")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "credential directory must be 0700");
    }

    #[test]
    fn saving_twice_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("credentials.json");
        let store = KeyringStore::with_file(path.clone());
        store.save(&sample()).expect("first save");
        store.save(&sample()).expect("second save");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left {leftovers:?} behind");
    }

    #[test]
    fn a_corrupt_bundle_is_an_error_not_a_silent_repair() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("credentials.json");
        std::fs::write(&path, "{not json").expect("write");
        assert!(KeyringStore::with_file(path).load().is_err());
    }
}
