//! Execution-grant persistence (docs/06 §4, invariant #1). A grant is minted on
//! human approval and validated + consumed exactly once immediately before an
//! R2+ tool executes; it is the *only* thing that authorizes such a call.
//!
//! This is the infra side of the `jarvis-application::policy` ports
//! [`GrantMinter`] / [`GrantValidator`], where the crypto the domain forbids
//! (`getrandom`, `sha2`) lives:
//!   * mint: a 256-bit CSPRNG grant id, `sha256(canonical_form(args))`, and an
//!     expiry from the policy TTL, persisted with its `grant.minted` audit event
//!     in ONE transaction (invariant #6);
//!   * validate: the DB row is the source of truth. The presented in-memory
//!     grant and the actual invocation are BOTH re-checked against it; expiry is
//!     re-checked against the caller's clock; `single_use` is consumed under a
//!     `FOR UPDATE` lock so a concurrent replay loses the race. Any failure ⇒ the
//!     grant is not consumed, a `grant.rejected` event is recorded, and the
//!     executor is never reached.
//!
//! Fault handling: `mint` returns `Result<ExecutionGrant, GrantMintError>`
//! (CF-6, F2.6) so an infra/DB fault routes the run to `RunState::Failed`
//! gracefully instead of panicking the task — and, because the fault returns
//! *before* `tx.commit`, the transaction rolls back and no grant is persisted
//! (still FAIL-SAFE: a failed mint authorizes nothing, invariant #1). The
//! `GrantMintError` message is non-sensitive and never carries the raw driver
//! text (invariant #5). `validate` still returns only `GrantError`; a fault
//! there remains an `.expect` (its own error arm is a later carry-forward).

use std::time::SystemTime;

use async_trait::async_trait;
use jarvis_application::policy::{GrantBinding, GrantMintError, GrantMinter, GrantValidator};
use jarvis_domain::audit::AuditEvent;
use jarvis_domain::grants::{ExecutionGrant, GrantError, GrantId, Sha256 as ArgsHash};
use jarvis_domain::tools::{ToolInvocation, ToolVersion, canonical_form};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;

/// Postgres-backed grant store. One instance implements both application ports.
pub struct PgGrantStore {
    pool: PgPool,
}

impl PgGrantStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// `sha256(canonical_form(arguments))` — the one normalization+hash shared by
/// minting and validation, so identical arguments in any key order bind and
/// re-validate to the same value (skill `policy-grants`, property-tested in the
/// domain `canonical_form` suite).
fn args_hash(arguments: &jarvis_domain::tools::CanonicalValue) -> ArgsHash {
    let mut hasher = Sha256::new();
    hasher.update(canonical_form(arguments));
    ArgsHash::from_bytes(hasher.finalize().into())
}

fn i64_ver(v: u64) -> i64 {
    i64::try_from(v).expect("tool version component fits i64")
}

#[async_trait]
impl GrantMinter for PgGrantStore {
    async fn mint(&self, binding: GrantBinding) -> Result<ExecutionGrant, GrantMintError> {
        let mut id_bytes = [0u8; 32];
        getrandom::fill(&mut id_bytes)
            .map_err(|_| GrantMintError("system CSPRNG unavailable".to_owned()))?;
        let grant = ExecutionGrant {
            grant_id: GrantId::from_bytes(id_bytes),
            user_id: binding.user_id,
            device_id: binding.device_id,
            run_id: binding.run_id,
            tool_id: binding.tool_id,
            tool_version: binding.tool_version,
            normalized_args_sha256: args_hash(&binding.arguments),
            target_resource: binding.target_resource,
            expires_at: SystemTime::now() + binding.ttl,
            single_use: true,
        };

        // Any fault below returns before `commit`, so the tx rolls back and no
        // grant row nor audit event persists (fail-safe). Messages are stable and
        // non-sensitive — never the raw driver text (invariant #5).
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| GrantMintError("grant mint: begin failed".to_owned()))?;
        let minted_at = OffsetDateTime::now_utc();
        let expires_at = OffsetDateTime::from(grant.expires_at);
        let grant_id = grant.grant_id.to_string();
        sqlx::query!(
            r#"
            INSERT INTO tooling.grants
                (grant_id, user_id, device_id, run_id, tool_id,
                 tool_version_major, tool_version_minor, tool_version_patch,
                 normalized_args_sha256, target_resource, expires_at, single_use, minted_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            "#,
            grant_id,
            grant.user_id.as_str(),
            grant.device_id.as_str(),
            grant.run_id.as_str(),
            grant.tool_id.as_str(),
            i64_ver(grant.tool_version.major),
            i64_ver(grant.tool_version.minor),
            i64_ver(grant.tool_version.patch),
            grant.normalized_args_sha256.to_string(),
            grant.target_resource.as_str(),
            expires_at,
            grant.single_use,
            minted_at,
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| GrantMintError("grant mint: insert failed".to_owned()))?;

        crate::audit::append(&mut tx, &grant_event(&grant, "grant.minted", None))
            .await
            .map_err(|_| GrantMintError("grant mint: audit failed".to_owned()))?;
        tx.commit()
            .await
            .map_err(|_| GrantMintError("grant mint: commit failed".to_owned()))?;
        Ok(grant)
    }
}

#[async_trait]
impl GrantValidator for PgGrantStore {
    async fn validate(
        &self,
        grant: &ExecutionGrant,
        invocation: &ToolInvocation,
        now: SystemTime,
    ) -> Result<(), GrantError> {
        let mut tx = self.pool.begin().await.expect("grant validate: begin");
        let outcome = check_and_consume(&mut tx, grant, invocation, now).await;
        // A rejection is itself a durable security event; record it in the same
        // transaction as the (non-)consume, then commit so the audit lands.
        let event = match &outcome {
            Ok(()) => grant_event(grant, "grant.consumed", None),
            Err(e) => grant_event(grant, "grant.rejected", Some(e.code())),
        };
        crate::audit::append(&mut tx, &event)
            .await
            .expect("grant validate: audit");
        tx.commit().await.expect("grant validate: commit");
        outcome
    }
}

/// The row as the grant binding's source of truth.
struct GrantRow {
    user_id: String,
    device_id: String,
    run_id: String,
    tool_id: String,
    tool_version_major: i64,
    tool_version_minor: i64,
    tool_version_patch: i64,
    normalized_args_sha256: String,
    target_resource: String,
    expires_at: OffsetDateTime,
    consumed_at: Option<OffsetDateTime>,
}

async fn check_and_consume(
    tx: &mut Transaction<'_, Postgres>,
    grant: &ExecutionGrant,
    invocation: &ToolInvocation,
    now: SystemTime,
) -> Result<(), GrantError> {
    let grant_id = grant.grant_id.to_string();
    // FOR UPDATE serializes concurrent validations of the same grant: the loser
    // sees `consumed_at` already set and reports Consumed — no double execution.
    let row = sqlx::query_as!(
        GrantRow,
        r#"
        SELECT user_id, device_id, run_id, tool_id,
               tool_version_major, tool_version_minor, tool_version_patch,
               normalized_args_sha256, target_resource, expires_at, consumed_at
        FROM tooling.grants
        WHERE grant_id = $1
        FOR UPDATE
        "#,
        grant_id,
    )
    .fetch_optional(&mut **tx)
    .await
    .expect("grant validate: select")
    .ok_or(GrantError::Missing)?;

    let row_version = ToolVersion::new(
        row.tool_version_major as u64,
        row.tool_version_minor as u64,
        row.tool_version_patch as u64,
    );

    // 1. The presented in-memory grant must match the stored binding exactly —
    //    it may have been tampered with after minting (invariant #1: never trust
    //    that decision-time state still holds).
    if grant.user_id.as_str() != row.user_id || grant.device_id.as_str() != row.device_id {
        return Err(GrantError::WrongActor);
    }
    if grant.run_id.as_str() != row.run_id {
        return Err(GrantError::WrongRun);
    }
    if grant.tool_id.as_str() != row.tool_id || grant.tool_version != row_version {
        return Err(GrantError::WrongTool);
    }
    if grant.target_resource.as_str() != row.target_resource {
        return Err(GrantError::ResourceMismatch);
    }
    if grant.normalized_args_sha256.to_string() != row.normalized_args_sha256 {
        return Err(GrantError::ArgsMismatch);
    }

    // 2. The actual invocation must match the stored binding — the model may
    //    propose different arguments than were approved (edit-after-approval is
    //    invalidation by hash, not a flag).
    if invocation.tool_id.as_str() != row.tool_id || invocation.tool_version != row_version {
        return Err(GrantError::WrongTool);
    }
    if args_hash(&invocation.arguments).to_string() != row.normalized_args_sha256 {
        return Err(GrantError::ArgsMismatch);
    }

    // 3. Expiry. The stored expiry is authoritative; the effective clock is the
    //    LATER of the caller-supplied `now` and the infra wall clock, so a stale
    //    or back-dated `now` can only make a grant *more* expired, never revive
    //    an expired one (domain `grants.rs`: expiry is "never trusted from a
    //    caller-supplied timestamp"; security-auditor advisory F2.4). When the
    //    live path is wired, `now` must still originate from the infra Clock port
    //    (CF-6/F2.6) — this clamp is defence in depth, not a substitute.
    let effective_now = now.max(SystemTime::now());
    if OffsetDateTime::from(effective_now) >= row.expires_at {
        return Err(GrantError::Expired);
    }

    // 4. Single-use: a spent grant is a replay.
    if row.consumed_at.is_some() {
        return Err(GrantError::Consumed);
    }

    // Consume. The guard trigger rejects a second consume even if a bug raced
    // past the check above; RETURNING confirms exactly one row was spent.
    let consumed = sqlx::query_scalar!(
        r#"
        UPDATE tooling.grants
        SET consumed_at = $2
        WHERE grant_id = $1 AND consumed_at IS NULL
        RETURNING grant_id
        "#,
        grant_id,
        OffsetDateTime::from(now),
    )
    .fetch_optional(&mut **tx)
    .await
    .expect("grant validate: consume");

    if consumed.is_none() {
        return Err(GrantError::Consumed);
    }
    Ok(())
}

/// Build a grant-lifecycle audit event. The raw arguments are never included —
/// only the bound hash — so a sensitive payload cannot leak into the chain
/// (invariant #5). The payload is built with `serde_json` (never string
/// interpolation): `audit::append` re-parses it and panics on invalid JSON, so
/// any field that ever carries a quote must be escaped, not trusted to be
/// charset-clean.
fn grant_event(grant: &ExecutionGrant, event_type: &str, code: Option<&str>) -> AuditEvent {
    let mut payload = serde_json::json!({
        "toolId": grant.tool_id.to_string(),
        "toolVersion": grant.tool_version.to_string(),
        "argsSha256": grant.normalized_args_sha256.to_string(),
    });
    if let Some(code) = code {
        payload["code"] = serde_json::Value::String(code.to_owned());
    }
    AuditEvent {
        occurred_at: SystemTime::now(),
        actor: format!("user:{}", grant.user_id.as_str()),
        event_type: event_type.to_owned(),
        target: format!("grant:{}", grant.grant_id),
        correlation_id: Some(grant.run_id.as_str().to_owned()),
        payload_json: payload.to_string(),
    }
}
