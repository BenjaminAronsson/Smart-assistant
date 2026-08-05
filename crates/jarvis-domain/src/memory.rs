//! Durable memory value types and safety rules (FR-16, docs/02 §7).
//!
//! Raw conversation history is not memory. A memory is an explicit, bounded
//! record with provenance, sensitivity, confidence, and retention metadata.
//! The domain rejects credential-shaped content before an adapter can persist
//! or embed it: memory is not a secret store.

use std::time::SystemTime;

use crate::ids::{MemoryId, MessageId, RunId, SessionId, UserId};
use crate::location::Sensitivity;

pub const MAX_MEMORY_TEXT_BYTES: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryLayer {
    Working,
    Episodic,
    Semantic,
    Procedural,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemorySource {
    Explicit,
    Message(MessageId),
    Run(RunId),
}

impl MemorySource {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Message(_) => "message",
            Self::Run(_) => "run",
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Explicit => None,
            Self::Message(id) => Some(id.as_str()),
            Self::Run(id) => Some(id.as_str()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryScope {
    User,
    Session(SessionId),
    Project(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionRule {
    UntilForgotten,
    ExpiresAt(SystemTime),
    Session,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub id: MemoryId,
    pub user_id: UserId,
    pub layer: MemoryLayer,
    pub text: String,
    pub source: MemorySource,
    pub scope: MemoryScope,
    pub retention: RetentionRule,
    pub confidence: f32,
    pub sensitivity: Sensitivity,
    pub pinned: bool,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MemoryError {
    #[error("memory text must not be empty")]
    EmptyText,
    #[error("memory text is too long")]
    TooLong,
    #[error("credential-shaped content cannot be stored as memory")]
    SecretLike,
    #[error("memory confidence must be between 0 and 1")]
    InvalidConfidence,
}

impl Memory {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: MemoryId,
        user_id: UserId,
        layer: MemoryLayer,
        text: String,
        source: MemorySource,
        scope: MemoryScope,
        retention: RetentionRule,
        confidence: f32,
        sensitivity: Sensitivity,
        pinned: bool,
        now: SystemTime,
    ) -> Result<Self, MemoryError> {
        let text = normalize_text(&text).ok_or(MemoryError::EmptyText)?;
        if text.len() > MAX_MEMORY_TEXT_BYTES {
            return Err(MemoryError::TooLong);
        }
        if looks_like_secret(&text) {
            return Err(MemoryError::SecretLike);
        }
        if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
            return Err(MemoryError::InvalidConfidence);
        }
        Ok(Self {
            id,
            user_id,
            layer,
            text,
            source,
            scope,
            retention,
            confidence,
            sensitivity,
            pinned,
            created_at: now,
            updated_at: now,
        })
    }
}

fn normalize_text(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len().min(MAX_MEMORY_TEXT_BYTES));
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_control()
            || matches!(ch, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
        {
            continue;
        }
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    (!out.is_empty()).then_some(out)
}

/// Conservative detector for common credential forms. It intentionally errs
/// toward refusing storage; users can store a non-secret explanation instead.
pub fn looks_like_secret(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let labels = [
        "password",
        "passphrase",
        "secret",
        "api key",
        "apikey",
        "token",
        "private key",
        "recovery code",
        "otp",
        "one-time code",
        "wifi key",
        "wireless key",
    ];
    if labels.iter().any(|label| lower.contains(label)) {
        return true;
    }
    lower.contains("-----begin ") || lower.contains("ghp_") || lower.contains("sk-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    const USER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const MEMORY: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";

    fn memory(text: &str) -> Result<Memory, MemoryError> {
        Memory::new(
            MemoryId::from_str(MEMORY).unwrap(),
            UserId::from_str(USER).unwrap(),
            MemoryLayer::Semantic,
            text.to_owned(),
            MemorySource::Explicit,
            MemoryScope::User,
            RetentionRule::UntilForgotten,
            1.0,
            Sensitivity::Normal,
            false,
            SystemTime::UNIX_EPOCH,
        )
    }

    #[test]
    fn explicit_memory_normalizes_text_and_keeps_provenance() {
        let item = memory("  Benjamin\n likes  tea. ").unwrap();
        assert_eq!(item.text, "Benjamin likes tea.");
        assert_eq!(item.source.kind(), "explicit");
        assert_eq!(item.source.id(), None);
    }

    #[test]
    fn credential_shaped_content_is_refused_before_storage() {
        assert_eq!(
            memory("Remember my Wi-Fi password is hunter2"),
            Err(MemoryError::SecretLike)
        );
        assert_eq!(
            memory("-----BEGIN PRIVATE KEY-----"),
            Err(MemoryError::SecretLike)
        );
        assert!(!looks_like_secret(
            "The credential policy requires twelve characters"
        ));
    }

    #[test]
    fn bounds_and_confidence_are_fail_closed() {
        assert_eq!(
            memory(&"x".repeat(MAX_MEMORY_TEXT_BYTES + 1)),
            Err(MemoryError::TooLong)
        );
        let invalid = Memory::new(
            MemoryId::from_str(MEMORY).unwrap(),
            UserId::from_str(USER).unwrap(),
            MemoryLayer::Semantic,
            "fact".to_owned(),
            MemorySource::Explicit,
            MemoryScope::User,
            RetentionRule::UntilForgotten,
            f32::NAN,
            Sensitivity::Normal,
            false,
            SystemTime::UNIX_EPOCH,
        );
        assert_eq!(invalid, Err(MemoryError::InvalidConfidence));
    }
}
