//! `GET /api/v1/diagnostics/bundle` — one thing to send when it misbehaves
//! (F10.4, NFR-07, docs/09).
//!
//! # Redaction by shape, not by filter
//!
//! Every query here selects counts, host-defined identifiers and timestamps.
//! None of them selects a message body, a prompt, a tool argument, a device
//! name or an audit payload — and the DTO has nowhere to put one if they did.
//!
//! That is deliberate and it is the whole design. A redaction *filter* is a list
//! that must be maintained: it works until someone adds a field, and then it
//! fails silently, in the one artifact whose entire purpose is being sent to
//! someone else. A redaction *shape* cannot fail that way, because the leak has
//! nowhere to land.
//!
//! Concretely, what is deliberately absent:
//!
//! * message and transcript text — `conversation.messages` is counted, never read;
//! * audit payloads, actors and targets — only `event_type` and a tally;
//! * device names — owner-chosen and often personal ("Dad's phone"), counted only;
//! * tool arguments — the registry's tool *ids* are listed, nothing they carried;
//! * config values and keyring references — adapter `detail` is a fixed hint
//!   naming a config key, never its value.
//!
//! Authenticated and `ui`-scoped, unlike the health page. Health answers "are
//! you alive" for an install script on loopback; this is a richer picture of the
//! household, and a satellite has no business assembling one.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::Response;
use jarvis_application::policy::ToolRegistry;
use jarvis_contracts::diagnostics::{
    AdapterLineDto, AuditShapeDto, DiagnosticsBundleDto, MigrationStateDto, ResourcesDto,
    RunOutcomesDto,
};
use jarvis_contracts::health::AdapterState;
use sqlx::PgPool;

/// How many audit event *types* to report. A bounded list keeps a bundle
/// readable; the tail of a long-tailed distribution diagnoses nothing.
const MAX_AUDIT_SHAPES: i64 = 25;

#[derive(Clone)]
pub struct DiagnosticsApi {
    pool: PgPool,
    registry: Arc<ToolRegistry>,
    adapters: Arc<
        std::sync::RwLock<
            std::collections::BTreeMap<String, jarvis_contracts::health::AdapterHealth>,
        >,
    >,
    started_at: std::time::Instant,
}

impl DiagnosticsApi {
    pub fn new(
        pool: PgPool,
        registry: Arc<ToolRegistry>,
        adapters: Arc<
            std::sync::RwLock<
                std::collections::BTreeMap<String, jarvis_contracts::health::AdapterHealth>,
            >,
        >,
    ) -> Self {
        Self {
            pool,
            registry,
            adapters,
            started_at: std::time::Instant::now(),
        }
    }

    pub async fn bundle(&self) -> DiagnosticsBundleDto {
        DiagnosticsBundleDto {
            generated_at: now_rfc3339(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            adapters: self.adapter_lines(),
            migrations: self.migrations().await,
            tools: self.registry.tool_ids().map(ToString::to_string).collect(),
            audit_shapes: self.audit_shapes().await,
            runs: self.run_outcomes().await,
            resources: ResourcesDto {
                rss_kib: rss_kib(),
                uptime_secs: self.started_at.elapsed().as_secs(),
            },
            device_count: self.count("SELECT count(*) FROM identity.devices").await,
            session_count: self
                .count("SELECT count(*) FROM conversation.sessions")
                .await,
            // Counted, never read. The bodies are the single most sensitive
            // thing in the database and there is no diagnostic that needs them.
            message_count: self
                .count("SELECT count(*) FROM conversation.messages")
                .await,
        }
    }

    fn adapter_lines(&self) -> Vec<AdapterLineDto> {
        self.adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(name, health)| AdapterLineDto {
                name: name.clone(),
                state: match health.state {
                    AdapterState::Up => "up",
                    AdapterState::Down => "down",
                    AdapterState::Disabled => "disabled",
                }
                .to_owned(),
                detail: health.detail.clone(),
            })
            .collect()
    }

    /// A count that never fails the bundle.
    ///
    /// A diagnostics bundle is produced *because* something is wrong, so a query
    /// that errors is the normal case, not the exceptional one. Returning 0
    /// rather than propagating keeps the rest of the picture — which is the
    /// point of collecting several independent things.
    async fn count(&self, sql: &str) -> u64 {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(&self.pool)
            .await
            .map(|n| u64::try_from(n).unwrap_or(0))
            .unwrap_or(0)
    }

    async fn migrations(&self) -> MigrationStateDto {
        let applied = self.count("SELECT count(*) FROM _sqlx_migrations").await;
        let latest_version =
            sqlx::query_scalar::<_, Option<i64>>("SELECT max(version) FROM _sqlx_migrations")
                .fetch_one(&self.pool)
                .await
                .ok()
                .flatten();
        MigrationStateDto {
            applied,
            latest_version,
        }
    }

    /// Event **types** and tallies. Never `actor`, `target` or `payload`.
    async fn audit_shapes(&self) -> Vec<AuditShapeDto> {
        sqlx::query_as::<_, (String, i64, time::OffsetDateTime)>(
            "SELECT event_type, count(*), max(occurred_at) \
             FROM audit.audit_events GROUP BY event_type \
             ORDER BY count(*) DESC LIMIT $1",
        )
        .bind(MAX_AUDIT_SHAPES)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(event_type, count, last_at)| AuditShapeDto {
            event_type,
            count: u64::try_from(count).unwrap_or(0),
            last_at: last_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        })
        .collect()
    }

    /// Tallies by terminal state. No prompts, no answers, no errors-as-text —
    /// a run's failure *message* can quote a tool result or a model reply.
    async fn run_outcomes(&self) -> RunOutcomesDto {
        let rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT state, count(*) FROM orchestration.runs GROUP BY state",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let mut out = RunOutcomesDto::default();
        for (state, count) in rows {
            let n = u64::try_from(count).unwrap_or(0);
            out.total += n;
            match state.as_str() {
                "completed" => out.completed += n,
                "failed" => out.failed += n,
                "cancelled" => out.cancelled += n,
                _ => out.in_flight += n,
            }
        }
        out
    }
}

/// Resident set size, from `/proc/self/statm` where it exists.
fn rss_kib() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4) // 4 KiB pages
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

pub async fn get_bundle(
    State(api): State<DiagnosticsApi>,
) -> Result<Json<DiagnosticsBundleDto>, Response> {
    Ok(Json(api.bundle().await))
}
