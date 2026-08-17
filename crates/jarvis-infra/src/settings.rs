//! Runtime settings persistence (F8.8, F8.11, migration 0020).
//!
//! The override layer only. Nothing security-relevant is stored here and no
//! credential ever passes through it — the ElevenLabs API key stays a keyring
//! reference in `jarvisd.toml` (invariant 5); what this records is whether the
//! owner consented to using it.

use async_trait::async_trait;
use jarvis_application::ports::{RepositoryError, SettingsStore, VoiceOverrides};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::ids::DeviceId;
use sqlx::PgPool;
use std::time::SystemTime;
use time::OffsetDateTime;

pub struct PgSettingsStore {
    pool: PgPool,
}

impl PgSettingsStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn storage(e: sqlx::Error) -> RepositoryError {
    RepositoryError::Storage(e.to_string())
}

/// `YYYY-MM` in UTC for an instant — the key the spend table is bucketed by.
///
/// Computed here rather than taken from the caller so that two callers cannot
/// disagree about which month a spend belongs to.
pub fn period_of(at: SystemTime) -> String {
    let at = OffsetDateTime::from(at);
    format!("{:04}-{:02}", at.year(), u8::from(at.month()))
}

#[async_trait]
impl SettingsStore for PgSettingsStore {
    async fn voice_overrides(&self) -> Result<VoiceOverrides, RepositoryError> {
        let row = sqlx::query!(
            r#"SELECT wake_word, elevenlabs_enabled FROM settings.voice WHERE only_row"#
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;

        Ok(
            row.map_or_else(VoiceOverrides::default, |row| VoiceOverrides {
                wake_word: row.wake_word,
                elevenlabs_enabled: row.elevenlabs_enabled,
            }),
        )
    }

    async fn set_voice_overrides(
        &self,
        overrides: &VoiceOverrides,
        by_device: &DeviceId,
        at: SystemTime,
        audit: &AuditEvent,
    ) -> Result<VoiceOverrides, RepositoryError> {
        // Row and audit in one transaction (invariant 6): consenting to a
        // third-party egress path must not be possible to do unrecorded.
        let mut tx = self.pool.begin().await.map_err(storage)?;

        // COALESCE on the *excluded* value so an absent field leaves the stored
        // one alone: the request says what to change, not what everything is,
        // which is what keeps two shell tabs from overwriting each other.
        let row = sqlx::query!(
            r#"
            INSERT INTO settings.voice
                (only_row, wake_word, elevenlabs_enabled, updated_at, updated_by_device_id)
            VALUES (TRUE, $1, $2, $3, $4)
            ON CONFLICT (only_row) DO UPDATE SET
                wake_word = COALESCE($1, settings.voice.wake_word),
                elevenlabs_enabled = COALESCE($2, settings.voice.elevenlabs_enabled),
                updated_at = EXCLUDED.updated_at,
                updated_by_device_id = EXCLUDED.updated_by_device_id
            RETURNING wake_word, elevenlabs_enabled
            "#,
            overrides.wake_word.as_deref(),
            overrides.elevenlabs_enabled,
            OffsetDateTime::from(at),
            by_device.as_str(),
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;

        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(format!("voice settings: audit: {e}")))?;
        tx.commit().await.map_err(storage)?;

        Ok(VoiceOverrides {
            wake_word: row.wake_word,
            elevenlabs_enabled: row.elevenlabs_enabled,
        })
    }
}

/// The durable character budget (F8.11, ADR-033 §5).
///
/// Separate from [`PgSettingsStore`] because the speech adapter holds this and
/// must not, through it, be able to rewrite the consent gate that governs it.
pub struct PgSpendLedger {
    pool: PgPool,
    /// Injectable so a test can pin the month instead of waiting for one.
    clock: Box<dyn Fn() -> SystemTime + Send + Sync>,
}

impl PgSpendLedger {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            clock: Box::new(SystemTime::now),
        }
    }

    pub fn with_clock(
        pool: PgPool,
        clock: impl Fn() -> SystemTime + Send + Sync + 'static,
    ) -> Self {
        Self {
            pool,
            clock: Box::new(clock),
        }
    }

    fn period(&self) -> String {
        period_of((self.clock)())
    }
}

#[async_trait]
impl jarvis_application::ports::SpendLedger for PgSpendLedger {
    async fn reserve(&self, characters: u64) -> Result<u64, RepositoryError> {
        let row = sqlx::query!(
            r#"
            INSERT INTO settings.elevenlabs_spend (period, spent_characters)
            VALUES ($1, $2)
            ON CONFLICT (period) DO UPDATE SET
                spent_characters =
                    settings.elevenlabs_spend.spent_characters + EXCLUDED.spent_characters
            RETURNING spent_characters
            "#,
            self.period(),
            i64::try_from(characters).unwrap_or(i64::MAX),
        )
        .fetch_one(&self.pool)
        .await
        .map_err(storage)?;

        Ok(row.spent_characters.max(0) as u64)
    }

    async fn refund(&self, characters: u64) -> Result<(), RepositoryError> {
        // GREATEST rather than a bare subtraction: the column's CHECK would
        // reject a negative, and a refund arriving after a period rollover
        // must not be able to fail the request that is giving budget *back*.
        sqlx::query!(
            r#"
            UPDATE settings.elevenlabs_spend
            SET spent_characters = GREATEST(0, spent_characters - $2)
            WHERE period = $1
            "#,
            self.period(),
            i64::try_from(characters).unwrap_or(i64::MAX),
        )
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn spent(&self) -> Result<u64, RepositoryError> {
        let row = sqlx::query!(
            r#"SELECT spent_characters FROM settings.elevenlabs_spend WHERE period = $1"#,
            self.period(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;

        Ok(row.map_or(0, |row| row.spent_characters.max(0) as u64))
    }
}
