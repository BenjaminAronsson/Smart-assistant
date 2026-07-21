//! Host-applied execution-timeout decorator (CF-11, docs/06 §5). Wraps any
//! [`ToolExecutor`] so a tool that hangs — a FIFO/special file, a slow external
//! trickle that stays under the read cap — fails with [`ToolError::Timeout`] at
//! its policy deadline instead of blocking until the user cancels.
//!
//! The bound is **host-owned**: the host reads [`ToolPolicy::timeout`] at
//! registration and wraps each executor with [`TimeoutExecutor::wrap`], so the
//! deadline is never tool-self-declared (invariant #1; a tool cannot lengthen its
//! own leash). It lives in the adapter layer, not the orchestrator: the timer
//! (`tokio::time`) belongs where the runtime already is, keeping the pure
//! `jarvis-application` crate runtime-neutral (invariant #3). The orchestrator
//! still races the whole `execute` future against cancellation (`run_or_cancel`);
//! this deadline is composed underneath that — cancellation still wins first, and
//! a timeout surfaces to `tool_step` as an ordinary `Err`, audited as
//! `tool.failed` (CF-4) and failing the run.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jarvis_application::policy::ToolExecutor;
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::tools::{CanonicalValue, ToolError, ToolInvocation, ToolResult};
use tokio_util::sync::CancellationToken;

/// Bounds an inner executor's `execute` by a fixed policy deadline.
pub struct TimeoutExecutor {
    inner: Arc<dyn ToolExecutor>,
    timeout: Duration,
}

impl TimeoutExecutor {
    /// Wrap `inner` so its execution is abandoned after `timeout`, returning
    /// [`ToolError::Timeout`]. The returned executor is a drop-in for the inner
    /// one — same port, same `validate_args`.
    pub fn wrap(inner: Arc<dyn ToolExecutor>, timeout: Duration) -> Arc<dyn ToolExecutor> {
        Arc::new(Self { inner, timeout })
    }
}

#[async_trait]
impl ToolExecutor for TimeoutExecutor {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        grant: Option<ExecutionGrant>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        // On elapse, the inner future is dropped (cancelled) — the same abandon
        // semantics the orchestrator relies on for cancellation (invariant #4).
        match tokio::time::timeout(self.timeout, self.inner.execute(invocation, grant, cancel))
            .await
        {
            Ok(result) => result,
            Err(_elapsed) => Err(ToolError::Timeout(self.timeout)),
        }
    }

    /// Argument-schema validation is not an execution and is not timed — forward
    /// it verbatim so wrapping does not change a tool's pre-mint CF-9 checks.
    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        self.inner.validate_args(arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::tools::{ToolId, ToolVersion};

    fn invocation() -> ToolInvocation {
        ToolInvocation {
            tool_id: "test.tool".parse::<ToolId>().unwrap(),
            tool_version: ToolVersion::new(1, 0, 0),
            arguments: CanonicalValue::obj([]),
        }
    }

    /// A tool that never completes on its own — only a timeout or cancellation
    /// can end it. Stands in for a FIFO/special-file read that blocks forever.
    struct HangingTool;

    #[async_trait]
    impl ToolExecutor for HangingTool {
        async fn execute(
            &self,
            _invocation: ToolInvocation,
            _grant: Option<ExecutionGrant>,
            _cancel: CancellationToken,
        ) -> Result<ToolResult, ToolError> {
            std::future::pending().await
        }

        fn validate_args(&self, _arguments: &CanonicalValue) -> Result<(), ToolError> {
            Err(ToolError::SchemaInvalid("nope".to_owned()))
        }
    }

    /// A tool that returns immediately — proves the decorator is transparent when
    /// the inner call finishes inside the deadline.
    struct InstantTool;

    #[async_trait]
    impl ToolExecutor for InstantTool {
        async fn execute(
            &self,
            _invocation: ToolInvocation,
            _grant: Option<ExecutionGrant>,
            _cancel: CancellationToken,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult {
                content: "done".to_owned(),
                truncated: false,
                compensation: None,
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_tool_times_out_at_its_deadline() {
        let tool = TimeoutExecutor::wrap(Arc::new(HangingTool), Duration::from_secs(5));
        let err = tool
            .execute(invocation(), None, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Timeout(d) if d == Duration::from_secs(5)),
            "got {err:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_prompt_tool_is_unaffected() {
        let tool = TimeoutExecutor::wrap(Arc::new(InstantTool), Duration::from_secs(5));
        let result = tool
            .execute(invocation(), None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.content, "done");
    }

    #[test]
    fn validate_args_is_forwarded_to_the_inner_tool() {
        let tool = TimeoutExecutor::wrap(Arc::new(HangingTool), Duration::from_secs(5));
        // The inner tool rejects everything; the wrapper must not mask that.
        let err = tool.validate_args(&CanonicalValue::obj([])).unwrap_err();
        assert!(matches!(err, ToolError::SchemaInvalid(_)), "got {err:?}");
    }
}
