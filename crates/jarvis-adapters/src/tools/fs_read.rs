//! `fs.read` — the real R0 native tool (F2.6, exit evidence #1). Reads a project
//! file within an allowlisted root and nothing else: read-only, no egress, and
//! **path-traversal denied**. It is the concrete proof that a real native tool
//! flows end-to-end through `policy::evaluate` (invariant #1) — R0 still routes
//! through the auto path and emits an audit event; there is no read-only
//! shortcut.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::tools::required_str;

/// Cap on bytes read from a single file. Bounds the memory a single call can
/// force (resource DoS, docs/06 §5); a file larger than this is read up to the
/// cap and the result is marked `truncated`. Content-level sanitization (control
/// chars, injection shaping) is the orchestrator's job, not this executor's
/// (CF-3) — here we only bound the resource.
const MAX_READ_BYTES: u64 = 64 * 1024;

/// Reads a file within a fixed, allowlisted root. `root` is the **canonicalized**
/// directory outside which no read is ever permitted; [`FsReadTool::new`]
/// canonicalizes it once so symlink resolution at call time can be compared
/// against a stable, real path.
pub struct FsReadTool {
    root: PathBuf,
}

impl FsReadTool {
    /// Construct against an allowlisted root, canonicalizing it up front. Fails
    /// if the root does not exist or cannot be resolved — the host must supply a
    /// real directory (docs/09 §1).
    pub fn new(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let root = std::fs::canonicalize(root)?;
        Ok(Self { root })
    }

    /// The stable tool identifier.
    pub fn id() -> ToolId {
        "fs.read".parse().expect("static tool id is valid")
    }

    /// Host-owned policy: R0, read-only, reversible (a read mutates nothing), no
    /// egress, gated behind the `files:read` scope (the domain's scope
    /// vocabulary, `policy::Scope`). R0 still passes through `evaluate`; the scope
    /// requirement means an unscoped run cannot read.
    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R0,
            is_reversible: true,
            requires_user_presence: false,
            timeout: Duration::from_secs(5),
            required_scopes: [Scope::new("files:read").expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::None,
        }
    }

    /// Registerable descriptor (id + version + policy + executor).
    pub fn descriptor(root: impl AsRef<Path>) -> std::io::Result<ToolDescriptor> {
        Ok(ToolDescriptor {
            id: Self::id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: std::sync::Arc::new(Self::new(root)?),
        })
    }

    /// Resolve a requested relative path against the root, refusing anything that
    /// could escape it. Two independent defences: (1) a lexical check that
    /// rejects absolute paths and any `..`/root component *before* touching the
    /// filesystem, and (2) `canonicalize` (which resolves symlinks) followed by a
    /// `starts_with(root)` check — so a symlink *inside* the root pointing out is
    /// also caught. Both must pass.
    ///
    /// Known limits (docs/06 §5 confused-deputy), out of scope for the
    /// model-supplied-path threat this R0 read defends: a TOCTOU directory-swap
    /// between `canonicalize` and `open`, and a hardlink inside the root to an
    /// outside file — both require a *concurrent local writer*, and this tier
    /// ships no write tool that could plant one. Hardening (`openat2`
    /// `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS`) is a carry-forward for when a write
    /// tool or the live ToolStack lands.
    ///
    /// Async because `canonicalize` is a blocking syscall: it must not run on a
    /// runtime worker thread (use `tokio::fs`, not `std::fs`, off startup).
    async fn resolve(&self, requested: &str) -> Result<PathBuf, ToolError> {
        let requested = Path::new(requested);
        let escapes = requested.is_absolute()
            || requested.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            });
        if escapes {
            return Err(ToolError::Denied(
                "path escapes the allowlisted root".to_owned(),
            ));
        }
        let candidate = self.root.join(requested);
        // Resolves symlinks; errors (e.g. not found) map to a non-sensitive
        // message that never echoes the requested path (invariant #5).
        let canonical = tokio::fs::canonicalize(&candidate)
            .await
            .map_err(|_| ToolError::ExecutionFailed("file not found or unreadable".to_owned()))?;
        if !canonical.starts_with(&self.root) {
            return Err(ToolError::Denied(
                "path escapes the allowlisted root".to_owned(),
            ));
        }
        Ok(canonical)
    }
}

#[async_trait]
impl ToolExecutor for FsReadTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R0: auto-authorized, never carries a grant.
        // Cancellation is enforced one level up: the orchestrator races the whole
        // `execute` future against the token (`run_or_cancel` in `tool_step`) and
        // drops it on cancel. The read here is bounded (`MAX_READ_BYTES`), so no
        // internal race is needed to stay promptly cancellable (invariant #4).
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let requested = required_str(&invocation.arguments, "path")?;
        let path = self.resolve(requested).await?;

        let file = tokio::fs::File::open(&path)
            .await
            .map_err(|_| ToolError::ExecutionFailed("cannot open file".to_owned()))?;
        let mut bytes = Vec::new();
        // Read one byte past the cap so we can detect (and mark) truncation.
        file.take(MAX_READ_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| ToolError::ExecutionFailed("read failed".to_owned()))?;

        let truncated = bytes.len() as u64 > MAX_READ_BYTES;
        let slice = if truncated {
            &bytes[..MAX_READ_BYTES as usize]
        } else {
            &bytes[..]
        };
        // Lossy decode: a binary or partially-read file yields replacement
        // characters rather than an error — the orchestrator sanitizer (CF-3)
        // strips control content before any of this reaches a model prompt.
        let content = String::from_utf8_lossy(slice).into_owned();

        Ok(ToolResult {
            content,
            truncated,
            compensation: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::tools::CanonicalValue;

    fn invocation(path: &str) -> ToolInvocation {
        ToolInvocation {
            tool_id: FsReadTool::id(),
            tool_version: ToolVersion::new(1, 0, 0),
            arguments: CanonicalValue::obj([("path", CanonicalValue::str(path))]),
        }
    }

    fn temp_root() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("jarvis-fsread-{}", uid()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn uid() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn policy_is_r0_read_only_no_egress() {
        let policy = FsReadTool::policy();
        assert_eq!(policy.risk, RiskLevel::R0);
        assert!(policy.is_reversible);
        assert_eq!(policy.egress, DataEgress::None);
        assert!(!policy.requires_grant());
        assert!(
            policy
                .required_scopes
                .contains(&Scope::new("files:read").unwrap())
        );
    }

    #[tokio::test]
    async fn reads_a_file_within_the_root() {
        let root = temp_root();
        std::fs::write(root.join("notes.txt"), "hello jarvis").unwrap();
        let tool = FsReadTool::new(&root).unwrap();

        let result = tool
            .execute(invocation("notes.txt"), None, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result.content, "hello jarvis");
        assert!(!result.truncated);
        assert!(result.compensation.is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn denies_parent_directory_traversal() {
        let root = temp_root();
        let outside = root.parent().unwrap().join(format!("secret-{}", uid()));
        std::fs::write(&outside, "top secret").unwrap();
        let tool = FsReadTool::new(&root).unwrap();

        // `../secret` is rejected lexically, before any filesystem access.
        let escape = format!("../{}", outside.file_name().unwrap().to_str().unwrap());
        let err = tool
            .execute(invocation(&escape), None, CancellationToken::new())
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::Denied(_)), "got {err:?}");
        std::fs::remove_file(&outside).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn denies_absolute_paths() {
        let root = temp_root();
        let tool = FsReadTool::new(&root).unwrap();

        let err = tool
            .execute(invocation("/etc/passwd"), None, CancellationToken::new())
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::Denied(_)), "got {err:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn denies_a_symlink_that_escapes_the_root() {
        let root = temp_root();
        let outside = root.parent().unwrap().join(format!("target-{}", uid()));
        std::fs::write(&outside, "escaped").unwrap();
        // A symlink *inside* the root pointing outside it: passes the lexical
        // check, caught by canonicalize + starts_with.
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let tool = FsReadTool::new(&root).unwrap();
        let err = tool
            .execute(invocation("link.txt"), None, CancellationToken::new())
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::Denied(_)), "got {err:?}");
        std::fs::remove_file(&outside).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn truncates_a_file_larger_than_the_cap() {
        let root = temp_root();
        let big = "x".repeat((MAX_READ_BYTES as usize) + 100);
        std::fs::write(root.join("big.txt"), &big).unwrap();
        let tool = FsReadTool::new(&root).unwrap();

        let result = tool
            .execute(invocation("big.txt"), None, CancellationToken::new())
            .await
            .unwrap();

        assert!(result.truncated);
        assert_eq!(result.content.len(), MAX_READ_BYTES as usize);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn missing_path_argument_is_an_execution_error() {
        let root = temp_root();
        let tool = FsReadTool::new(&root).unwrap();
        let invocation = ToolInvocation {
            tool_id: FsReadTool::id(),
            tool_version: ToolVersion::new(1, 0, 0),
            arguments: CanonicalValue::obj([]),
        };

        let err = tool
            .execute(invocation, None, CancellationToken::new())
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::ExecutionFailed(_)), "got {err:?}");
        std::fs::remove_dir_all(&root).ok();
    }
}
