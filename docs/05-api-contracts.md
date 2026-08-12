# 05 — Contracts and APIs

## 1. Surface overview

| Endpoint | FR | Purpose |
|---|---|---|
| `POST /api/v1/auth/pair` | — | Exchange first-run pairing code for a device token (§6). |
| `GET /api/v1/sessions?query=&status=&limit=&cursor=` | FR-02 | List/search sessions (full-text on title + summary, filters on status/time). |
| `POST /api/v1/sessions` | FR-02 | Create a session. Body: `{ title? }` — all other metadata is server-assigned. |
| `GET /api/v1/sessions/{id}` | FR-02 | Session metadata + summary. |
| `GET /api/v1/sessions/{id}/timeline?since=&limit=` | FR-01/07 | Messages + persisted run events (resync snapshot source). |
| `POST /api/v1/sessions/{id}/messages` | FR-01 | Submit input; returns run acknowledgement. |
| `POST /api/v1/sessions/{id}/branch` | FR-02 | Branch from a message. |
| `POST /api/v1/sessions/{id}/archive` | FR-02 | Archive (reversible). |
| `POST /api/v1/runs/{id}/cancel` | FR-06 | Cancel model, tool, and audio work. |
| `GET /api/v1/runs/{id}` | FR-07 | Durable state + timeline + trace linkage. |
| `POST /api/v1/approvals/{id}/decision` | FR-05 | Approve/deny the exact proposed action. |
| `GET /api/v1/artifacts/{id}/versions` | FR-08 | List versions + provenance. |
| `GET /api/v1/artifacts/{id}/versions/{version}/blob` | FR-08 | Download a version's bytes; content-addressed `ETag`, `nosniff` + `attachment` (served, never rendered inline). |
| `POST /api/v1/artifacts/{id}/open` | FR-09/10 | Request rendering on a selected display. |
| `GET /api/v1/map/coverage` | FR-25 | Locally served PMTiles extract: bounds, zoom range, centre, tile-URL template, mandatory OSM attribution. Absent (404) when no archive is configured — the card then takes the coverage fallback (docs/12 §3). |
| `GET /api/v1/map/tiles/{z}/{x}/{y}` | FR-25 | One tile from the local extract, served as stored (`nosniff`, strong `ETag`). Outside the archive's bbox/zoom ⇒ 404 (refused, never approximated); in-region but empty ⇒ 204. |
| `GET /api/v1/tools` | FR-04 | Curated tool catalogue + grants. |
| `GET /api/v1/memories?layer=&query=&cursor=` | FR-16 | Review memory items with provenance. |
| `PATCH /api/v1/memories/{id}` | FR-16 | Edit text, pin, set retention. |
| `DELETE /api/v1/memories/{id}` | FR-16 | Forget — cascades to embeddings (04 §4). |
| `GET /api/v1/automations` · `POST /api/v1/automations` | FR-17 | List / create (creation is an R2 action → approval flow). |
| `PATCH /api/v1/automations/{id}` · `DELETE …` | FR-17 | Edit/disable/delete (R2). |
| `GET /api/v1/automations/{id}/executions` | FR-17 | Execution history with policy decisions. |
| `POST /api/v1/devices/pairing-window` | FR-19 | Owner opens a node-pairing window and receives the one-time code (`ui` scope). |
| `POST /api/v1/devices/pair` | FR-19 | Node presents its public key + the code; receives a single-use challenge (unauthenticated — the node has no token yet). |
| `POST /api/v1/devices/pair/complete` | FR-19 | Node returns the signature; receives its device token and assigned class. |
| `GET /api/v1/devices` | FR-19 | List paired devices with class, scopes, last seen, revocation state (`ui` scope only). |
| `POST /api/v1/devices/{id}/revoke` | FR-19 | Revoke a device immediately: token fails closed on the next request and its live socket is closed (`ui` scope only). |
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
- **Adding a required field to a *response* DTO is additive; adding one to a *request*
  DTO is breaking.** Old readers ignore unknown fields, and the reverse skew (new client
  against old server) cannot occur in this topology — server and clients live in one repo
  and deploy together (A-07). A request DTO has no such asymmetry: an older client that
  cannot send the field is refused. Precedent: `PairResponse.deviceClass` (F7.1), made
  required rather than defaulted because a default would be a silently wrong
  *authority-relevant* value. A future change may cite this only for responses.

## 6. Authentication model (v1)

Single-owner, loopback-first — deliberately simple, upgraded at M7:

1. **Bootstrap.** On first start (or `jarvisd pair --new`), `jarvisd` generates a one-time
   pairing code, prints it to the journal, and shows it on the health page (loopback
   only). The client posts it to `/api/v1/auth/pair` with a device name and receives a
   device record + opaque device token (random 256-bit, stored hashed server-side, keyring
   client-side). The pair response body is `{ deviceId, deviceToken, scopes }` — the
   granted scope list (§6.3) is returned explicitly so clients never infer it.
2. **Requests.** Every REST call carries `Authorization: Bearer <token>`. The `/ws/v1`
   upgrade does too **for non-browser clients** (the desktop agent, tests) — but a
   browser's native `WebSocket` constructor cannot set arbitrary request headers on a
   handshake, so the Angular shell instead offers the token as a WS subprotocol behind a
   `jarvis.device.v1` sentinel (`new WebSocket(url, ['jarvis.device.v1', token])`);
   `jarvisd::auth::ws_subprotocol_token` accepts that as a fallback, scoped to genuine WS
   handshakes only, and echoes back the sentinel (never the token) to complete the
   negotiation. Unauthenticated surface: `GET /api/v1/diagnostics/health` on loopback only.
3. **Scopes come from the device class (M7 F7.1).** A device never names its own
   authority: pairing assigns a **class**, and the class decides the scopes. Two
   vocabularies, typed apart in `jarvis_domain::identity`:
   *class scopes* — `ui`, `display-agent`, `voice-capture` — and *tool scopes*
   (`<area>:<capability>`, e.g. `home:control`) which only `owner-ui` holds.

   | Class | Class scopes | Tool scopes | What it is |
   |---|---|---|---|
   | `owner-ui` | `ui` | all (`OWNER_TOOL_SCOPES`) | The owner's shell/CLI; the bootstrap device. |
   | `display-node` | `display-agent` | none | A screen: the local `jarvis-agent`, or a remote display node. |
   | `voice-node` | `voice-capture` | none | A microphone/speaker satellite with no screen. |
   | `room-node` | `display-agent`, `voice-capture` | none | A satellite that both listens and shows. |

   A satellite is therefore **toolless by construction** — its authority is to present
   and to capture, never to act. The class is stored on the device row and is what
   authorization reads; `identity.devices.scopes` is the pairing-time snapshot kept for
   audit only, so a stale or tampered row cannot widen authority. Device management
   (`GET /api/v1/devices`, revoke) requires `ui`, so no node can enumerate or revoke.
4. **Revocation.** Immediate per-device revocation (`POST /api/v1/devices/{id}/revoke`,
   `ui` scope, audited with a reason). Revoked tokens fail closed on the next request, and
   the device's **live WebSocket is closed** rather than left running until it happens to
   reconnect. Revocation is idempotent. Revoking the last active `owner-ui` device is
   refused (`identity.last_owner_device`): it would leave nothing able to pair a
   replacement short of a restart.
5. **M7 upgrade path.** LAN/remote adds TLS + per-device keys with challenge-response
   pairing and mTLS or signed tokens (06 §5); the token model above remains for loopback.

## 7. Error code registry (starter set)

Stable machine codes for client logic; the registry lives in `jarvis-contracts::errors`
and grows additively. HTTP mapping via RFC 9457 problem details.

| Code | Meaning | Typical HTTP |
|---|---|---|
| `auth.invalid_token` | Missing/revoked/expired device token | 401 |
| `auth.scope_missing` | Device lacks required scope | 403 |
| `auth.pairing_invalid` | No open pairing window matches the presented code | 403 |
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
| `artifact.integrity_failed` | Stored blob failed content-address verification on read (CAS, F3a.2) | 500 |
| `degraded.queued` | Accepted but queued awaiting provider recovery | 202 |
| `media.nothing_playing` | No media player is running (FR-22) | 409 |
| `media.target_ambiguous` | Two or more players active and none named; the server never guesses (ADR-016) | 409 |
| `media.player_gone` | Named player left the bus between snapshot and command; retryable | 409 |
| `media.control_unsupported` | Player reports it cannot perform this control | 409 |
| `timer.invalid_transition` | Verb illegal for the timer's state (snooze before it rang, cancel after) — FR-33 | 409 |
| `timer.stale` | Timer changed between read and decision (fired, or another device answered); retryable after refresh | 409 |
| `list.full` | The list is at its item bound; retrying unchanged will not help — the owner removes or checks something off, or promotes the list to an artifact (FR-34) | 409 |
| `list.unrecognized_command` | The deterministic grammar refused rather than guessing which list the owner meant (FR-34, ADR-024/ADR-016); the body was valid, its *content* was not resolvable here, so the caller falls back to the normal run path | 422 |
| `deepdive.nothing_to_promote` | The deep-dive thread has consulted nothing yet, so there is no Research Notes document to write (FR-27, ADR-017); promoting a bare heading would mint a versioned artifact that says nothing | 409 |
| `identity.last_owner_device` | Revoking this device would leave no `owner-ui` device, so nothing could pair a replacement without a `jarvisd` restart (FR-19, F7.1); never blocks revoking a node | 409 |
| `identity.class_not_grantable` | A node requested a device class it may not have (`owner-ui`, or an unknown name) — never quietly downgraded (FR-19, F7.2) | 403 |
| `identity.challenge_rejected` | Pairing challenge unknown, expired, spent, or issued to a different key; one code for all four so the challenge space cannot be probed (FR-19, F7.2) | 403 |
