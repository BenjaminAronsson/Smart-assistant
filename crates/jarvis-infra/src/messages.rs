//! Postgres-backed `MessageStore` (docs/04 §2-§3, FR-01). Messages are
//! immutable; `append` writes the row and its `message.created` outbox event in
//! one transaction (docs/02 §2). M1 content is a single text block; the JSONB
//! column already holds the discriminated block array (docs/05 §2) so richer
//! blocks are additive without a migration.

use jarvis_application::ports::{MessageStore, RepositoryError};
use jarvis_domain::conversations::{Message, MessageRole};
use jarvis_domain::ids::SessionId;
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub struct PgMessageStore {
    pool: sqlx::PgPool,
}

impl PgMessageStore {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl MessageStore for PgMessageStore {
    async fn append(&self, message: &Message) -> Result<(), RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let content = json!([{ "type": "text", "text": message.text }]);
        let created_at = OffsetDateTime::from(message.created_at);

        sqlx::query!(
            r#"
            INSERT INTO conversation.messages (id, session_id, role, content, created_at)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            message.id.as_str(),
            message.session_id.as_str(),
            role_str(message.role),
            content,
            created_at,
        )
        .execute(&mut *tx)
        .await
        .map_err(storage)?;

        // `message.created` payload matches the wire `MessageDto` (docs/05 §2)
        // so the host forwards it verbatim.
        let payload = json!({ "message": {
            "id": message.id.as_str(),
            "sessionId": message.session_id.as_str(),
            "role": role_str(message.role),
            "content": content,
            "createdAt": created_at.format(&Rfc3339).map_err(|e| RepositoryError::Storage(e.to_string()))?,
        }});
        sqlx::query!(
            "INSERT INTO outbox.outbox_events (event_type, payload) VALUES ($1, $2)",
            "message.created",
            payload,
        )
        .execute(&mut *tx)
        .await
        .map_err(storage)?;

        tx.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn list_by_session(
        &self,
        session_id: &SessionId,
        limit: u32,
    ) -> Result<Vec<Message>, RepositoryError> {
        let rows = sqlx::query!(
            r#"
            SELECT id, role, content, created_at
            FROM conversation.messages
            WHERE session_id = $1
            ORDER BY created_at ASC
            LIMIT $2
            "#,
            session_id.as_str(),
            i64::from(limit),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;

        rows.into_iter()
            .map(|r| {
                Ok(Message {
                    id: r.id.parse().map_err(|e| {
                        RepositoryError::Storage(format!("stored message id invalid: {e}"))
                    })?,
                    session_id: session_id.clone(),
                    role: role_from(&r.role)?,
                    text: text_of(&r.content),
                    created_at: r.created_at.into(),
                })
            })
            .collect()
    }
}

fn storage(e: sqlx::Error) -> RepositoryError {
    RepositoryError::Storage(e.to_string())
}

fn role_str(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
    }
}

fn role_from(role: &str) -> Result<MessageRole, RepositoryError> {
    Ok(match role {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "system" => MessageRole::System,
        other => {
            return Err(RepositoryError::Storage(format!(
                "stored message role invalid: {other:?}"
            )));
        }
    })
}

/// Concatenate the text of every `text` block (M1 messages carry exactly one).
fn text_of(content: &serde_json::Value) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                block.get("text").and_then(|t| t.as_str())
            } else {
                None
            }
        })
        .collect()
}
