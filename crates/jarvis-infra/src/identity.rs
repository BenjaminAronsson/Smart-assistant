//! Postgres-backed `IdentityStore` (docs/05 §6, docs/04 §3). Token VALUES
//! never reach this module — callers hash first; the identity schema stores
//! hashes only.
//!
//! Authority is read from `device_class`, never from the stored `scopes`
//! column (F7.1): the column is the pairing-time snapshot kept for audit, and
//! reading it back would mean a tampered row could widen what a device may do.

use jarvis_application::ports::{
    IdentityStore, NodePairOutcome, RepositoryError, RevocationOutcome,
};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::identity::{Device, DeviceClass};
use jarvis_domain::ids::DeviceId;
use sqlx::PgPool;
use std::str::FromStr;
use std::time::SystemTime;
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
            INSERT INTO identity.devices
                (id, user_id, name, token_hash, scopes, device_class, public_key, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
            device.id.as_str(),
            device.user_id.as_str(),
            device.name,
            device.token_hash,
            // Snapshot only — `device_class` on the next line is what
            // authorization reads back (see the module docs).
            &device.effective_scopes(),
            device.class.as_str(),
            device.public_key,
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
            SELECT id, user_id, name, token_hash, device_class, public_key, created_at,
                   last_seen_at, revoked_at, revoked_reason
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
                id: parse_id(&r.id, "device id")?,
                user_id: parse_id(&r.user_id, "user id")?,
                name: r.name,
                token_hash: r.token_hash,
                public_key: r.public_key,
                class: parse_class(&r.device_class)?,
                created_at: r.created_at.into(),
                last_seen_at: r.last_seen_at.map(Into::into),
                revoked_at: r.revoked_at.map(Into::into),
                revoked_reason: r.revoked_reason,
            })
        })
        .transpose()
    }

    async fn pair_node_device(
        &self,
        device: &Device,
        audit: &AuditEvent,
    ) -> Result<NodePairOutcome, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;

        // Attach to the owner who already exists. Deliberately keyed off an
        // ACTIVE owner device rather than `identity.users`: a house whose only
        // owner device has been revoked should not accept new satellites.
        let owner_user_id: Option<String> = sqlx::query_scalar!(
            r#"
            SELECT user_id FROM identity.devices
            WHERE device_class = $1 AND revoked_at IS NULL
            ORDER BY created_at, id
            LIMIT 1
            "#,
            DeviceClass::OwnerUi.as_str(),
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some(owner_user_id) = owner_user_id else {
            tx.rollback().await.map_err(storage)?;
            return Ok(NodePairOutcome::NoOwner);
        };

        let insert = sqlx::query!(
            r#"
            INSERT INTO identity.devices
                (id, user_id, name, token_hash, scopes, device_class, public_key, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT DO NOTHING
            "#,
            device.id.as_str(),
            owner_user_id,
            device.name,
            device.token_hash,
            &device.effective_scopes(),
            device.class.as_str(),
            device.public_key,
            OffsetDateTime::from(device.created_at),
        )
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        if insert.rows_affected() == 0 {
            // The partial unique index on `public_key` is the only conflict
            // reachable here (ids are freshly minted ULIDs).
            tx.rollback().await.map_err(storage)?;
            return Ok(NodePairOutcome::KeyAlreadyPaired);
        }

        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(e.to_string()))?;
        tx.commit().await.map_err(storage)?;
        Ok(NodePairOutcome::Paired)
    }

    async fn touch_last_seen(
        &self,
        device_id: &DeviceId,
        at: SystemTime,
    ) -> Result<(), RepositoryError> {
        // Never resurrects a revoked device's presence: a revoked row is not
        // "here", it is gone.
        sqlx::query!(
            "UPDATE identity.devices SET last_seen_at = $2 WHERE id = $1 AND revoked_at IS NULL",
            device_id.as_str(),
            OffsetDateTime::from(at),
        )
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn is_device_active(&self, device_id: &DeviceId) -> Result<bool, RepositoryError> {
        let active: Option<bool> = sqlx::query_scalar!(
            "SELECT revoked_at IS NULL FROM identity.devices WHERE id = $1",
            device_id.as_str(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?
        .flatten();
        // Unknown device ⇒ not active (fail closed).
        Ok(active.unwrap_or(false))
    }

    async fn list_devices(&self) -> Result<Vec<Device>, RepositoryError> {
        let rows = sqlx::query!(
            r#"
            SELECT id, user_id, name, token_hash, device_class, public_key, created_at,
                   last_seen_at, revoked_at, revoked_reason
            FROM identity.devices
            ORDER BY created_at, id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;

        rows.into_iter()
            .map(|r| {
                Ok(Device {
                    id: parse_id(&r.id, "device id")?,
                    user_id: parse_id(&r.user_id, "user id")?,
                    name: r.name,
                    token_hash: r.token_hash,
                    public_key: r.public_key,
                    class: parse_class(&r.device_class)?,
                    created_at: r.created_at.into(),
                    last_seen_at: r.last_seen_at.map(Into::into),
                    revoked_at: r.revoked_at.map(Into::into),
                    revoked_reason: r.revoked_reason,
                })
            })
            .collect()
    }

    async fn revoke_device(
        &self,
        device_id: &DeviceId,
        reason: Option<&str>,
        revoked_at: SystemTime,
        audit: &AuditEvent,
    ) -> Result<RevocationOutcome, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;

        // Lock every active owner device FIRST, in a deterministic order, so
        // two concurrent revocations serialize on an overlapping row set
        // instead of each observing the other as "still there" and both
        // committing. Locking only the target row would let the last two
        // owner devices be revoked simultaneously.
        // A pathological lock wait must become a 503, not an axum handler
        // parked forever behind another transaction (this port carries no
        // CancellationToken).
        sqlx::query("SET LOCAL lock_timeout = '5s'")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;

        let active_owner_ids: Vec<String> = sqlx::query_scalar!(
            r#"
            SELECT id FROM identity.devices
            WHERE device_class = $1 AND revoked_at IS NULL
            ORDER BY id
            FOR UPDATE
            "#,
            DeviceClass::OwnerUi.as_str(),
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;

        let Some(target) = sqlx::query!(
            r#"
            SELECT device_class, revoked_at FROM identity.devices
            WHERE id = $1
            FOR UPDATE
            "#,
            device_id.as_str(),
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        else {
            // Release the `FOR UPDATE` locks now rather than whenever the
            // connection is next serviced — every revocation in the process
            // serialises on them.
            tx.rollback().await.map_err(storage)?;
            return Ok(RevocationOutcome::NotFound);
        };

        if target.revoked_at.is_some() {
            tx.rollback().await.map_err(storage)?;
            return Ok(RevocationOutcome::AlreadyRevoked);
        }

        // Fail closed on an unparseable class: a row we cannot classify is not
        // one we can reason about the lockout guard for.
        let target_class = match parse_class(&target.device_class) {
            Ok(class) => class,
            Err(e) => {
                tx.rollback().await.map_err(storage)?;
                return Err(e);
            }
        };
        if target_class == DeviceClass::OwnerUi {
            let active_owners: Vec<DeviceId> = active_owner_ids
                .iter()
                .map(|id| parse_id(id, "device id"))
                .collect::<Result<_, _>>()?;
            // One rule, one implementation — shared with the in-memory double.
            if jarvis_domain::identity::revoking_would_orphan_the_owner(&active_owners, device_id) {
                tx.rollback().await.map_err(storage)?;
                return Ok(RevocationOutcome::LastOwnerDevice);
            }
        }

        sqlx::query!(
            r#"
            UPDATE identity.devices
            SET revoked_at = $2, revoked_reason = $3
            WHERE id = $1 AND revoked_at IS NULL
            "#,
            device_id.as_str(),
            OffsetDateTime::from(revoked_at),
            reason,
        )
        .execute(&mut *tx)
        .await
        .map_err(storage)?;

        // Same transaction as the identity change (invariant 6).
        crate::audit::append(&mut tx, audit)
            .await
            .map_err(|e| RepositoryError::Storage(e.to_string()))?;

        tx.commit().await.map_err(storage)?;
        Ok(RevocationOutcome::Revoked)
    }
}

fn parse_id<T: FromStr>(raw: &str, what: &str) -> Result<T, RepositoryError>
where
    T::Err: std::fmt::Display,
{
    raw.parse()
        .map_err(|e| RepositoryError::Storage(format!("stored {what}: {e}")))
}

/// An unrecognized stored class authenticates nothing — it is never defaulted
/// to the owner class (that is precisely the failure this feature removes).
fn parse_class(raw: &str) -> Result<DeviceClass, RepositoryError> {
    DeviceClass::from_str(raw).map_err(|e| RepositoryError::Storage(e.to_string()))
}

fn storage(e: sqlx::Error) -> RepositoryError {
    RepositoryError::Storage(e.to_string())
}
