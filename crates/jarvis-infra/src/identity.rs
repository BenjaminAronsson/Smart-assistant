//! Postgres-backed `IdentityStore` (docs/05 §6, docs/04 §3). Token VALUES
//! never reach this module — callers hash first; the identity schema stores
//! hashes only.

use jarvis_application::ports::{IdentityStore, RepositoryError};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::identity::Device;
use sqlx::PgPool;
use time::OffsetDateTime;

pub struct PgIdentityStore {
    pool: PgPool,
}

impl PgIdentityStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IdentityStore for PgIdentityStore {
    async fn device_count(&self) -> Result<u64, RepositoryError> {
        let count: i64 = sqlx::query_scalar!("SELECT count(*) FROM identity.devices")
            .fetch_one(&self.pool)
            .await
            .map_err(storage)?
            .unwrap_or(0);
        Ok(u64::try_from(count).unwrap_or(0))
    }

    async fn pair_device(
        &self,
        owner_name: &str,
        device: &Device,
        audit: &AuditEvent,
    ) -> Result<(), RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;

        sqlx::query!(
            "INSERT INTO identity.users (id, name, created_at) VALUES ($1, $2, $3)",
            device.user_id.as_str(),
            owner_name,
            OffsetDateTime::from(device.created_at),
        )
        .execute(&mut *tx)
        .await
        .map_err(storage)?;

        sqlx::query!(
            r#"
            INSERT INTO identity.devices (id, user_id, name, token_hash, scopes, created_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
            device.id.as_str(),
            device.user_id.as_str(),
            device.name,
            device.token_hash,
            &device.scopes,
            OffsetDateTime::from(device.created_at),
        )
        .execute(&mut *tx)
        .await
        .map_err(storage)?;

        // Same transaction as the identity change (invariant 6).
        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(e.to_string()))?;

        tx.commit().await.map_err(storage)
    }

    async fn find_active_device_by_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<Device>, RepositoryError> {
        let row = sqlx::query!(
            r#"
            SELECT id, user_id, name, token_hash, scopes, created_at, revoked_at
            FROM identity.devices
            WHERE token_hash = $1 AND revoked_at IS NULL
            "#,
            token_hash,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;

        row.map(|r| {
            Ok(Device {
                id: r
                    .id
                    .parse()
                    .map_err(|e| RepositoryError::Storage(format!("stored device id: {e}")))?,
                user_id: r
                    .user_id
                    .parse()
                    .map_err(|e| RepositoryError::Storage(format!("stored user id: {e}")))?,
                name: r.name,
                token_hash: r.token_hash,
                scopes: r.scopes,
                created_at: r.created_at.into(),
                revoked_at: r.revoked_at.map(Into::into),
            })
        })
        .transpose()
    }
}

fn storage(e: sqlx::Error) -> RepositoryError {
    RepositoryError::Storage(e.to_string())
}
