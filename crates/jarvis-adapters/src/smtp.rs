//! Bounded outbound SMTP mail (M4, FR-36, ADR-026).
//!
//! The host supplies [`SmtpConfig`] with an already-resolved password. This
//! adapter never resolves secrets, logs them, or includes provider errors in a
//! [`ToolError`]. The application policy/grant path remains the authority for
//! this R2 external mutation; the executor only validates the final arguments
//! and performs the bounded transport operation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{
    CanonicalValue, ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion, canonical_form,
};
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use sha2::{Digest, Sha256 as Sha2};
use tokio_util::sync::CancellationToken;

const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RECIPIENT_BYTES: usize = 320;
const MAX_SUBJECT_BYTES: usize = 998;
const MAX_BODY_BYTES: usize = 64 * 1024;

const INVALID_ARGUMENTS: &str = "invalid SMTP message arguments";
const INVALID_CONFIGURATION: &str = "SMTP configuration is invalid";
const SEND_FAILED: &str = "SMTP delivery failed";
const SEND_IN_PROGRESS: &str = "SMTP send already in progress";
const IDEMPOTENCY_CONFLICT: &str = "SMTP idempotency key was reused for different arguments";

/// The transport boundary keeps SMTP delivery fakeable without weakening the
/// production STARTTLS transport. Implementations must not expose provider
/// errors to the tool caller.
#[async_trait]
pub trait SmtpSender: Send + Sync {
    async fn send(&self, message: Message) -> Result<(), ()>;
}

/// Durable-seam for the send record. A database-backed implementation can make
/// `claim` an atomic insert and retain `Sent` records across process restarts;
/// the default is deliberately bounded to one process until that store exists.
#[async_trait]
pub trait SmtpIdempotencyStore: Send + Sync {
    async fn claim(
        &self,
        key: &str,
        fingerprint: &jarvis_domain::grants::Sha256,
    ) -> Result<ClaimOutcome, ()>;
    async fn mark_sent(
        &self,
        key: &str,
        fingerprint: &jarvis_domain::grants::Sha256,
    ) -> Result<(), ()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    Acquired,
    AlreadySent,
    InProgress,
}

#[derive(Default)]
pub struct InMemorySmtpIdempotencyStore {
    records: Mutex<BTreeMap<String, (jarvis_domain::grants::Sha256, bool)>>,
}

#[async_trait]
impl SmtpIdempotencyStore for InMemorySmtpIdempotencyStore {
    async fn claim(
        &self,
        key: &str,
        fingerprint: &jarvis_domain::grants::Sha256,
    ) -> Result<ClaimOutcome, ()> {
        let mut records = self.records.lock().map_err(|_| ())?;
        match records.get(key) {
            Some((existing, true)) if existing == fingerprint => Ok(ClaimOutcome::AlreadySent),
            Some((existing, false)) if existing == fingerprint => Ok(ClaimOutcome::InProgress),
            Some(_) => Err(()),
            None => {
                records.insert(key.to_owned(), (*fingerprint, false));
                Ok(ClaimOutcome::Acquired)
            }
        }
    }

    async fn mark_sent(
        &self,
        key: &str,
        fingerprint: &jarvis_domain::grants::Sha256,
    ) -> Result<(), ()> {
        let mut records = self.records.lock().map_err(|_| ())?;
        match records.get_mut(key) {
            Some((existing, sent)) if existing == fingerprint => {
                *sent = true;
                Ok(())
            }
            _ => Err(()),
        }
    }
}

struct LettreSmtpSender {
    config: SmtpConfig,
}

#[async_trait]
impl SmtpSender for LettreSmtpSender {
    async fn send(&self, email: Message) -> Result<(), ()> {
        let transport = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.config.host)
            .map_err(|_| ())?
            .port(self.config.port)
            .credentials(Credentials::new(
                self.config.username.clone(),
                self.config.password.clone(),
            ))
            .build();
        transport.send(email).await.map(|_| ()).map_err(|_| ())
    }
}

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
    sender: Arc<dyn SmtpSender>,
    idempotency: Arc<dyn SmtpIdempotencyStore>,
}

impl SmtpTool {
    pub fn new(config: SmtpConfig) -> Self {
        let sender_config = SmtpConfig::new(
            config.host.clone(),
            config.port,
            config.username.clone(),
            config.from.clone(),
            config.password.clone(),
        );
        Self::with_dependencies(
            config,
            Arc::new(LettreSmtpSender {
                config: sender_config,
            }),
            Arc::new(InMemorySmtpIdempotencyStore::default()),
        )
    }

    pub fn with_dependencies(
        config: SmtpConfig,
        sender: Arc<dyn SmtpSender>,
        idempotency: Arc<dyn SmtpIdempotencyStore>,
    ) -> Self {
        Self {
            config,
            sender,
            idempotency,
        }
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
        grant: Option<ExecutionGrant>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let Some(grant) = grant else {
            return Err(ToolError::Denied(
                "message.send requires an execution grant".to_owned(),
            ));
        };

        let (to, subject, body) = message_arguments(&invocation.arguments)?;
        let fingerprint = arguments_fingerprint(&invocation.arguments);
        if grant.tool_id != invocation.tool_id
            || grant.tool_version != invocation.tool_version
            || !grant.single_use
            || grant.normalized_args_sha256 != fingerprint
        {
            return Err(ToolError::Denied(
                "execution grant does not match message.send".to_owned(),
            ));
        }
        let email = self.message(to, subject, body)?;
        let key = grant.grant_id.to_string();

        match self.idempotency.claim(&key, &fingerprint).await {
            Ok(ClaimOutcome::AlreadySent) => {
                return Ok(ToolResult {
                    content: "Message already sent".to_owned(),
                    truncated: false,
                    compensation: None,
                });
            }
            Ok(ClaimOutcome::InProgress) => {
                return Err(ToolError::ExecutionFailed(SEND_IN_PROGRESS.to_owned()));
            }
            Ok(ClaimOutcome::Acquired) => {}
            Err(_) => return Err(ToolError::ExecutionFailed(IDEMPOTENCY_CONFLICT.to_owned())),
        }

        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let send = self.sender.send(email);
        tokio::pin!(send);
        tokio::select! {
            _ = cancel.cancelled() => Err(ToolError::Cancelled),
            result = tokio::time::timeout(SEND_TIMEOUT, &mut send) => {
                match result {
                    Ok(Ok(())) => match self.idempotency.mark_sent(&key, &fingerprint).await {
                        Ok(()) => Ok(ToolResult {
                            content: "Message sent".to_owned(),
                            truncated: false,
                            compensation: None,
                        }),
                        Err(_) => Err(ToolError::ExecutionFailed(SEND_FAILED.to_owned())),
                    },
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

fn arguments_fingerprint(arguments: &CanonicalValue) -> jarvis_domain::grants::Sha256 {
    let mut hasher = Sha2::new();
    hasher.update(canonical_form(arguments));
    jarvis_domain::grants::Sha256::from_bytes(hasher.finalize().into())
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use jarvis_domain::grants::{ExecutionGrant, GrantId};
    use jarvis_domain::ids::{DeviceId, RunId, UserId};
    use jarvis_domain::policy::ResourcePattern;

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

    fn grant(args: &CanonicalValue, byte: u8) -> ExecutionGrant {
        ExecutionGrant {
            grant_id: GrantId::from_bytes([byte; 32]),
            user_id: "00000000000000000000000001".parse::<UserId>().unwrap(),
            device_id: "00000000000000000000000002".parse::<DeviceId>().unwrap(),
            run_id: "00000000000000000000000003".parse::<RunId>().unwrap(),
            tool_id: SmtpTool::id(),
            tool_version: ToolVersion::new(1, 0, 0),
            normalized_args_sha256: arguments_fingerprint(args),
            target_resource: "smtp:*".parse::<ResourcePattern>().unwrap(),
            expires_at: std::time::SystemTime::now() + Duration::from_secs(60),
            single_use: true,
        }
    }

    struct FakeSender {
        sends: AtomicUsize,
    }

    #[async_trait]
    impl SmtpSender for FakeSender {
        async fn send(&self, _message: Message) -> Result<(), ()> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            Ok(())
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
    async fn direct_invocation_without_grant_is_denied_before_transport() {
        let tool = SmtpTool::new(config());
        let error = tool
            .execute(
                invocation(arguments("person@example.test", "Subject", "Body")),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied(_)));
    }

    #[tokio::test]
    async fn approved_grant_sends_once_and_duplicate_is_idempotent() {
        let sender = Arc::new(FakeSender {
            sends: AtomicUsize::new(0),
        });
        let tool = SmtpTool::with_dependencies(
            config(),
            sender.clone(),
            Arc::new(InMemorySmtpIdempotencyStore::default()),
        );
        let args = arguments("person@example.test", "Subject", "Body");
        let approved = grant(&args, 7);

        let first = tool
            .execute(
                invocation(args.clone()),
                Some(approved.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let replay = tool
            .execute(invocation(args), Some(approved), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(first.content, "Message sent");
        assert_eq!(replay.content, "Message already sent");
        assert_eq!(sender.sends.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn mismatched_grant_is_denied_before_fake_transport() {
        let sender = Arc::new(FakeSender {
            sends: AtomicUsize::new(0),
        });
        let tool = SmtpTool::with_dependencies(
            config(),
            sender.clone(),
            Arc::new(InMemorySmtpIdempotencyStore::default()),
        );
        let approved_args = arguments("person@example.test", "Subject", "Body");
        let changed_args = arguments("other@example.test", "Subject", "Body");
        let error = tool
            .execute(
                invocation(changed_args),
                Some(grant(&approved_args, 8)),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, ToolError::Denied(_)));
        assert_eq!(sender.sends.load(Ordering::SeqCst), 0);
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
