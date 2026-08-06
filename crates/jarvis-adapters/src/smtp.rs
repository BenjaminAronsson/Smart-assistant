//! Bounded outbound SMTP mail (M4, FR-36, ADR-026).
//!
//! The host supplies [`SmtpConfig`] with an already-resolved password. This
//! adapter never resolves secrets, logs them, or includes provider errors in a
//! [`ToolError`]. The application policy/grant path remains the authority for
//! this R2 external mutation; the executor only validates the final arguments
//! and performs the bounded transport operation.

use std::collections::BTreeSet;
use std::time::Duration;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{
    CanonicalValue, ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion,
};
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use tokio_util::sync::CancellationToken;

const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RECIPIENT_BYTES: usize = 320;
const MAX_SUBJECT_BYTES: usize = 998;
const MAX_BODY_BYTES: usize = 64 * 1024;

const INVALID_ARGUMENTS: &str = "invalid SMTP message arguments";
const INVALID_CONFIGURATION: &str = "SMTP configuration is invalid";
const SEND_FAILED: &str = "SMTP delivery failed";

/// SMTP connection settings. `password` is expected to be an already-resolved
/// secret from the host's secret store, never model- or user-supplied text.
/// This type intentionally does not implement `Debug` to avoid accidental
/// credential exposure in diagnostics.
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub from: String,
    pub password: String,
}

impl SmtpConfig {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        from: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            from: from.into(),
            password: password.into(),
        }
    }
}

pub struct SmtpTool {
    config: SmtpConfig,
}

impl SmtpTool {
    pub fn new(config: SmtpConfig) -> Self {
        Self { config }
    }

    pub fn id() -> ToolId {
        "message.send".parse().expect("static tool id is valid")
    }

    /// Host-owned policy: sending mail is an irreversible external mutation.
    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R2,
            is_reversible: false,
            requires_user_presence: true,
            timeout: SEND_TIMEOUT,
            required_scopes: [Scope::new("message:send").expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::External,
        }
    }

    pub fn descriptor(config: SmtpConfig) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: std::sync::Arc::new(Self::new(config)),
        }
    }

    fn message(&self, to: &str, subject: &str, body: &str) -> Result<Message, ToolError> {
        let from = parse_mailbox(&self.config.from).map_err(|_| invalid_configuration())?;
        let recipient = parse_mailbox(to).map_err(|_| invalid_arguments())?;

        Message::builder()
            .from(from)
            .to(recipient)
            .subject(subject)
            .body(body.to_owned())
            .map_err(|_| invalid_arguments())
    }
}

#[async_trait]
impl ToolExecutor for SmtpTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let (to, subject, body) = message_arguments(&invocation.arguments)?;
        let email = self.message(to, subject, body)?;
        let transport = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.config.host)
            .map_err(|_| invalid_configuration())?
            .port(self.config.port)
            .credentials(Credentials::new(
                self.config.username.clone(),
                self.config.password.clone(),
            ))
            .build();

        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let send = transport.send(email);
        tokio::pin!(send);
        tokio::select! {
            _ = cancel.cancelled() => Err(ToolError::Cancelled),
            result = tokio::time::timeout(SEND_TIMEOUT, &mut send) => {
                match result {
                    Ok(Ok(_)) => Ok(ToolResult {
                        content: "Message sent".to_owned(),
                        truncated: false,
                        compensation: None,
                    }),
                    Ok(Err(_)) => Err(ToolError::ExecutionFailed(SEND_FAILED.to_owned())),
                    Err(_) => Err(ToolError::Timeout(SEND_TIMEOUT)),
                }
            }
        }
    }

    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        let (to, subject, body) = message_arguments(arguments)?;
        validate_header_value(to, MAX_RECIPIENT_BYTES, "recipient")?;
        validate_header_value(subject, MAX_SUBJECT_BYTES, "subject")?;
        if body.is_empty() || body.len() > MAX_BODY_BYTES {
            return Err(invalid_arguments());
        }
        parse_mailbox(to).map_err(|_| invalid_arguments())?;
        parse_mailbox(&self.config.from).map_err(|_| invalid_configuration())?;
        Ok(())
    }
}

fn message_arguments(arguments: &CanonicalValue) -> Result<(&str, &str, &str), ToolError> {
    let CanonicalValue::Object(map) = arguments else {
        return Err(invalid_arguments());
    };

    let expected: BTreeSet<&str> = ["to", "subject", "body"].into_iter().collect();
    let actual: BTreeSet<&str> = map.keys().map(String::as_str).collect();
    if actual != expected {
        return Err(invalid_arguments());
    }

    let Some(CanonicalValue::Str(to)) = map.get("to") else {
        return Err(invalid_arguments());
    };
    let Some(CanonicalValue::Str(subject)) = map.get("subject") else {
        return Err(invalid_arguments());
    };
    let Some(CanonicalValue::Str(body)) = map.get("body") else {
        return Err(invalid_arguments());
    };
    Ok((to, subject, body))
}

fn validate_header_value(value: &str, max_bytes: usize, _field: &str) -> Result<(), ToolError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.contains('\r')
        || value.contains('\n')
        || value.chars().any(char::is_control)
    {
        return Err(invalid_arguments());
    }
    Ok(())
}

fn parse_mailbox(value: &str) -> Result<Mailbox, ()> {
    if value.contains('\r') || value.contains('\n') {
        return Err(());
    }
    value.parse().map_err(|_| ())
}

fn invalid_arguments() -> ToolError {
    ToolError::SchemaInvalid(INVALID_ARGUMENTS.to_owned())
}

fn invalid_configuration() -> ToolError {
    ToolError::ExecutionFailed(INVALID_CONFIGURATION.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SmtpConfig {
        SmtpConfig::new(
            "smtp.example.test",
            587,
            "jarvis",
            "jarvis@example.test",
            "secret",
        )
    }

    fn arguments(to: &str, subject: &str, body: &str) -> CanonicalValue {
        CanonicalValue::obj([
            ("to", CanonicalValue::str(to)),
            ("subject", CanonicalValue::str(subject)),
            ("body", CanonicalValue::str(body)),
        ])
    }

    fn invocation(args: CanonicalValue) -> ToolInvocation {
        ToolInvocation {
            tool_id: SmtpTool::id(),
            tool_version: ToolVersion::new(1, 0, 0),
            arguments: args,
        }
    }

    #[test]
    fn policy_is_r2_external_and_requires_presence() {
        let policy = SmtpTool::policy();
        assert_eq!(policy.risk, RiskLevel::R2);
        assert_eq!(policy.egress, DataEgress::External);
        assert!(policy.requires_grant());
        assert!(policy.requires_user_presence);
        assert!(!policy.is_reversible);
    }

    #[test]
    fn validation_requires_exact_message_shape() {
        let tool = SmtpTool::new(config());
        assert!(
            tool.validate_args(&arguments("person@example.test", "Subject", "Body"))
                .is_ok()
        );
        let extra = CanonicalValue::obj([
            ("to", CanonicalValue::str("person@example.test")),
            ("subject", CanonicalValue::str("Subject")),
            ("body", CanonicalValue::str("Body")),
            ("extra", CanonicalValue::str("ignored")),
        ]);
        assert!(matches!(
            tool.validate_args(&extra),
            Err(ToolError::SchemaInvalid(_))
        ));
    }

    #[test]
    fn validation_rejects_crlf_injection_and_invalid_addresses() {
        let tool = SmtpTool::new(config());
        for args in [
            arguments(
                "person@example.test\r\nBcc: victim@example.test",
                "Subject",
                "Body",
            ),
            arguments("person@example.test", "Subject\nInjected: yes", "Body"),
            arguments("not-an-address", "Subject", "Body"),
        ] {
            assert!(matches!(
                tool.validate_args(&args),
                Err(ToolError::SchemaInvalid(_))
            ));
        }
    }

    #[tokio::test]
    async fn cancelled_before_send_does_not_attempt_connection() {
        let tool = SmtpTool::new(config());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = tool
            .execute(
                invocation(arguments("person@example.test", "Subject", "Body")),
                None,
                cancel,
            )
            .await
            .unwrap_err();
        assert_eq!(error, ToolError::Cancelled);
    }

    #[test]
    fn configuration_errors_do_not_expose_secret_or_provider_details() {
        let tool = SmtpTool::new(SmtpConfig::new(
            "smtp.example.test",
            587,
            "user",
            "bad\nfrom@example.test",
            "super-secret-password",
        ));
        let error = tool
            .validate_args(&arguments("person@example.test", "Subject", "Body"))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "tool execution failed: SMTP configuration is invalid"
        );
        assert!(!error.to_string().contains("super-secret-password"));
    }
}
