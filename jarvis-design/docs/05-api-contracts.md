# 05 — Contracts and APIs

## 1. Surface overview

| Endpoint | FR | Purpose |
|---|---|---|
| `POST /api/v1/auth/pair` | — | Exchange first-run pairing code for a device token (§6). |
| `GET /api/v1/sessions?query=&status=&limit=&cursor=` | FR-02 | List/search sessions (full-text on title + summary, filters on status/time). |
| `POST /api/v1/sessions` | FR-02 | Create a session. |
| `GET /api/v1/sessions/{id}` | FR-02 | Session metadata + summary. |
| `GET /api/v1/sessions/{id}/timeline?since=&limit=` | FR-01/07 | Messages + persisted run events (resync snapshot source). |
| `POST /api/v1/sessions/{id}/messages` | FR-01 | Submit input; returns run acknowledgement. |
| `POST /api/v1/sessions/{id}/branch` | FR-02 | Branch from a message. |
| `POST /api/v1/sessions/{id}/archive` | FR-02 | Archive (reversible). |
| `POST /api/v1/runs/{id}/cancel` | FR-06 | Cancel model, tool, and audio work. |
| `GET /api/v1/runs/{id}` | FR-07 | Durable state + timeline + trace linkage. |
| `POST /api/v1/approvals/{id}/decision` | FR-05 | Approve/deny the exact proposed action. |
| `GET /api/v1/artifacts/{id}/versions` | FR-08 | List versions + provenance. |
| `POST /api/v1/artifacts/{id}/open` | FR-09/10 | Request rendering on a selected display. |
| `GET /api/v1/tools` | FR-04 | Curated tool catalogue + grants. |
| `GET /api/v1/memories?layer=&query=&cursor=` | FR-16 | Review memory items with provenance. |
| `PATCH /api/v1/memories/{id}` | FR-16 | Edit text, pin, set retention. |
| `DELETE /api/v1/memories/{id}` | FR-16 | Forget — cascades to embeddings (04 §4). |
| `GET /api/v1/automations` · `POST /api/v1/automations` | FR-17 | List / create (creation is an R2 action → approval flow). |
| `PATCH /api/v1/automations/{id}` · `DELETE …` | FR-17 | Edit/disable/delete (R2). |
| `GET /api/v1/automations/{id}/executions` | FR-17 | Execution history with policy decisions. |
| `POST /api/v1/devices/pair` | FR-19 | Device challenge/approval flow (nodes, M7). |
| `GET /api/v1/providers` | FR-11/12 | Profile health, quota state, reset window. |
| `GET /api/v1/diagnostics/health` | — | Core + adapter readiness (unauthenticated, loopback only). |
| `GET /ws/v1?since=…` | — | WebSocket (token-authenticated): run events, deltas, approvals, artifacts, presence, display commands, voice control. |

One WebSocket replaces v1's three SignalR hubs; message `channel` field discriminates
(`session`, `display`, `voice`). The desktop agent connects to the same `/ws/v1` as a
paired device with `display`-channel capabilities. Binary WS frames carry voice audio in
v1: **PCM 16-bit little-endian, 16 kHz, mono**, 20–40 ms frames, preceded by a JSON
`voice.stream.start` event carrying format metadata; may move to WebRTC/LiveKit at M7.

## 2. Command conventions

- Every side-effecting command carries an **idempotency key** and, where applicable, an
  expected resource version.
- Identity: authenticated user + paired device on every command.
- Errors: RFC 9457 problem details + stable machine `code` for client logic.
- Content is **discriminated blocks** (`text`, `image_ref`, `tool_call`, `approval_ref`,
  `artifact_ref`) — never one overloaded string.

## 3. WebSocket event envelope

```json
{
  "v": 1,
  "seq": 4182,
  "channel": "session",
  "type": "run.tool.completed",
  "occurredAt": "2026-07-17T10:31:04.112Z",
  "traceId": "…",
  "resourceVersion": 17,
  "payload": { }
}
```

Rules:

- `seq` is monotonic per connection scope. On gap or reconnect the client calls the REST
  snapshot endpoints (`GET /runs/{id}`, session timeline) to resync (NFR-13); persisted
  domain events since `since` are replayed, transient deltas are not.
- **Persisted** event categories: domain state (run started/completed, tool
  requested/completed, approval requested/decided, artifact version created), recovery
  checkpoints. **Not persisted:** token deltas, partial transcripts, waveform levels,
  transient progress. Presence is TTL state.
- Token deltas are disposable; a durable snapshot event follows completion.
- Every event carries schema version `v`; additive evolution only within a version.

## 4. Core Rust contracts (normative sketches)

```rust
// jarvis-application::ports — provider-neutral model boundary (FR-03, NFR-08)
#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn id(&self) -> ProfileId;
    fn capabilities(&self) -> ModelCapabilities;
    async fn run(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ModelEvent>, ModelError>;
}

pub struct ModelCapabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub structured_output: bool,
    pub vision: bool,
    pub locality: DataLocality,      // Local | Cloud
    pub max_context_tokens: u32,
}

pub enum ModelEvent {
    TextDelta(String),
    ToolProposal(ToolProposal),
    Usage(UsageSample),
    Done(FinishReason),
    ProviderError(ModelError),
}

// Routing (02 §5.4)
pub struct RoutingRequest {
    pub task: TaskClass,
    pub required: RequiredCapabilities,
    pub sensitivity: Sensitivity,
    pub offline_only: bool,
    pub latency_budget: Duration,
    pub cost_budget: Option<Decimal>,
    pub excluded_profiles: BTreeSet<ProfileId>,
}

// Tools (FR-04/05) — policy metadata is host-owned; servers cannot self-declare safety
pub struct ToolPolicy {
    pub risk: RiskLevel,                    // R0..R4
    pub is_reversible: bool,
    pub requires_user_presence: bool,
    pub timeout: Duration,
    pub required_scopes: BTreeSet<Scope>,
    pub egress: DataEgress,
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        grant: Option<ExecutionGrant>,   // None only for auto-authorized R0/R1
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError>;
}

// Grants (06 §4) — validated again immediately before execution
pub struct ExecutionGrant {
    pub grant_id: GrantId,
    pub user_id: UserId,
    pub device_id: DeviceId,
    pub run_id: RunId,
    pub tool_id: ToolId,
    pub tool_version: semver::Version,
    pub normalized_args_sha256: Sha256,
    pub target_resource: ResourcePattern,
    pub expires_at: OffsetDateTime,
    pub single_use: bool,
}

pub struct RunBudget {
    pub max_model_turns: u8,
    pub max_tool_calls: u16,
    pub max_duration: Duration,
    pub max_artifact_bytes: u64,
}
```

These are design contracts, not a complete SDK; implement with `#![deny(unsafe_code)]`,
newtyped IDs, and explicit cancellation throughout.

## 5. Contract governance

- All DTOs in `jarvis-contracts`; JSON Schemas via schemars; TypeScript types generated by
  `cargo xtask codegen` and committed (CI fails on drift).
- Tool schemas are versioned; historical runs preserve the schema version they used.
- Breaking change ⇒ new `v` and a compatibility shim window; the owner controls all
  clients (A-07), so windows can be short but never zero.

## 6. Authentication model (v1)

Single-owner, loopback-first — deliberately simple, upgraded at M7:

1. **Bootstrap.** On first start (or `jarvisd pair --new`), `jarvisd` generates a one-time
   pairing code, prints it to the journal, and shows it on the health page (loopback
   only). The client posts it to `/api/v1/auth/pair` with a device name and receives a
   device record + opaque device token (random 256-bit, stored hashed server-side, keyring
   client-side).
2. **Requests.** Every REST call and the WS upgrade carry `Authorization: Bearer <token>`.
   Unauthenticated surface: `GET /api/v1/diagnostics/health` on loopback only.
3. **Scopes.** Tokens carry device scopes (e.g. `ui`, `display-agent`, `voice-capture`);
   the desktop agent and the Angular shell are separate devices with separate scopes.
4. **Revocation.** Immediate per-device token revocation via settings; revoked tokens fail
   closed on the next request/frame.
5. **M7 upgrade path.** LAN/remote adds TLS + per-device keys with challenge-response
   pairing and mTLS or signed tokens (06 §5); the token model above remains for loopback.

## 7. Error code registry (starter set)

Stable machine codes for client logic; the registry lives in `jarvis-contracts::errors`
and grows additively. HTTP mapping via RFC 9457 problem details.

| Code | Meaning | Typical HTTP |
|---|---|---|
| `auth.invalid_token` | Missing/revoked/expired device token | 401 |
| `auth.scope_missing` | Device lacks required scope | 403 |
| `validation.failed` | Command failed schema/field validation | 400 |
| `idempotency.conflict` | Key reused with different payload | 409 |
| `resource.version_conflict` | Expected version mismatch | 409 |
| `resource.not_found` | Unknown ID | 404 |
| `run.budget_exceeded` | Model turns/tool calls/duration/bytes cap hit | 422 (event on WS) |
| `run.not_cancellable` | Run already terminal | 409 |
| `provider.unavailable` | Profile unhealthy (auth/network) | 503 (event on WS) |
| `provider.quota_exhausted` | Rate-limit window exhausted; `resetAt` in detail | 503 (event on WS) |
| `policy.denied` | Risk policy rejected the action (incl. R4) | 403 |
| `grant.expired` | Approval grant past expiry | 410 |
| `grant.args_mismatch` | Normalized-args hash differs from grant | 409 |
| `grant.consumed` | Single-use grant already used | 410 |
| `tool.timeout` | Tool exceeded its policy timeout | 504 (event on WS) |
| `tool.result_invalid` | Result failed schema/size validation | 502 (event on WS) |
| `artifact.too_large` | Exceeds max_artifact_bytes | 413 |
| `degraded.queued` | Accepted but queued awaiting provider recovery | 202 |
