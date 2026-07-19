# 03 — Technology stack

## 1. The core decision: Rust

The v1 design chose .NET because it matched the owner's skill set. v2 revisits that under
assumption **A-08**: Claude Code writes most of the code, the owner reviews. That changes
the optimization target from "language the human writes fastest" to "language whose
tooling catches an agent's mistakes before the human has to."

### Why Rust wins for this system

| Property | Why it matters for Jarvis |
|---|---|
| **Compiler as reviewer** | Agent-generated code that compiles has already passed no-null, no-data-race, exhaustive-match checks. The `RunState` machine, risk tiers, and grant validation are exactly the kind of logic where exhaustive enums + newtypes turn spec violations into compile errors. |
| **Always-on footprint** | A personal daemon must be invisible: `jarvisd` targets <100 MB RSS and <2 s cold start (NFR-15). A Rust axum service idles in tens of MB; a comparable ASP.NET Core host sits several times higher. |
| **Cancellation model** | FR-06 demands cancellation through model, tool, and audio paths. tokio's structured concurrency + `CancellationToken` + dropping futures maps cleanly; the design's "everything cancellable" invariant is idiomatic rather than bolted on. |
| **Single static binary** | `jarvisd` and `jarvis-agent` deploy as two binaries + config. No runtime version drift, trivial systemd units, trivial rollback. |
| **Ecosystem fit** | Official Rust MCP SDK (`rmcp`), `hyprland-rs` for the desktop agent, mature `sqlx`/Postgres, first-class OpenTelemetry via `tracing`. The unusual parts of this system (IPC, process supervision, streaming) are Rust's home turf. |
| **Security posture** | `#![deny(unsafe_code)]`, `cargo deny` for license/advisory gates, no reflection-based deserialization surprises — a smaller attack surface for a system whose whole point is enforcing authority boundaries. |

### Honest costs (and mitigations)

| Cost | Mitigation |
|---|---|
| Owner reads C# better than Rust today | The review surface is deliberately narrowed: policy, grants, and state transitions live in two small pure crates with transition-table tests. Learn Rust by reviewing those; let Claude Code carry the infra crates. |
| Slower compile times than `dotnet build` | Workspace split keeps incremental builds per-crate; CI caches; `cargo check` in the inner loop. |
| No SignalR | Replaced by a plain versioned WebSocket protocol (`05` §3) — fewer moving parts and no framework lock-in; the Angular client uses native WebSocket + generated types. |
| No EF migrations | `sqlx migrate` with compile-time checked queries — arguably stricter. |
| Fewer batteries for desktop UI in-process | Irrelevant: UI is Angular by design; the agent only does IPC. |

**Decision:** Rust for `jarvisd`, `jarvis-agent`, and native tools. Angular retained for
the shell. Python/Node remain acceptable **out of process** for voice engines and
third-party MCP servers. Recorded as [ADR-001](adr/README.md#adr-001).

## 2. Crate selection

| Concern | Crate(s) | Notes |
|---|---|---|
| Async runtime | `tokio` | Multi-threaded runtime; `tokio-util` `CancellationToken` everywhere. |
| HTTP + WS host | `axum` + `tower` | Versioned REST under `/api/v1`, WS upgrade at `/ws/v1`; tower middleware for auth, tracing, limits. |
| Serialization | `serde`, `serde_json` | Discriminated unions via `#[serde(tag = "type")]` for content blocks and WS events. |
| JSON Schema | `schemars` | Generates schemas for all `jarvis-contracts` DTOs; `xtask codegen` emits TypeScript types for Angular. |
| Database | `sqlx` (postgres, runtime-tokio, tls) | Compile-time checked queries; `sqlx migrate`; pgvector via `pgvector` crate. |
| MCP | `rmcp` (official SDK) | Host role only; stdio + streamable HTTP transports; child processes under separate identities. |
| HTTP client | `reqwest` | Ollama API, Home Assistant REST; `tokio-tungstenite` for the HA WebSocket API. |
| Process control | `tokio::process` | Claude CLI adapter: spawn, feed stdin, parse `stream-json`, kill on cancel/timeout. |
| Hyprland IPC | `hyprland` (hyprland-rs) | Request socket short-lived, event socket async — matches Hyprland's documented model. |
| Media / D-Bus | `zbus` | MPRIS player discovery + control (`02` §11a); pure-Rust, async, no libdbus. |
| Web search/fetch | `reqwest` + `scraper` | `web.search`/`web.fetch` (`02` §11b, ADR-014); `scraper` extracts title/text/og:image from fetched HTML; provider swappable via config. |
| Location | in-process port; optional `reqwest` call to an IP-geolocation API for the fallback tier | `LocationProvider` (`02` §11c, ADR-015); device GPS and home-coordinate tiers need no external dependency. |
| Secrets | `keyring` | OS keyring references only; DB stores references, never values. |
| Observability | `tracing`, `tracing-opentelemetry`, `opentelemetry-otlp` | OTLP to local collector; audit events are a separate sqlx-backed sink. |
| Errors | `thiserror` (libs), `anyhow` (bins) | Mapped to RFC 9457 problem details at the gateway. |
| IDs | `ulid` | Newtyped per entity. |
| Scheduling | `tokio-cron-scheduler` or hand-rolled | Automations re-check policy at fire time regardless. |
| State machine | hand-rolled enum + transition table | No macro framework; the explicit `match` is the documentation and the test surface. |
| Validation | `garde` or manual | Command validation at the gateway before use cases. |
| Config | `figment` | Layered: file → env → secrets references; validated at startup, fail-fast. |
| Lints/supply chain | `clippy -D warnings`, `cargo deny`, `cargo audit` | CI gates. |

Pin everything in `Cargo.lock`; MSRV in `rust-toolchain.toml`; review `cargo deny`
license output as part of the SBOM gate.

## 3. Frontend

- **Angular 22** (zoneless, signals) in `web/`, built to static assets served by
  `jarvisd`. Requires Node ≥22.22.3 / 24.15+ (`web/.nvmrc` pins 24; CI runs Node 24).
  Bump the major deliberately via `ng update`, one major at a time.
- Native `WebSocket` client with reconnect + sequence resync (`05` §3); no SignalR client.
- Types generated from JSON Schemas by `cargo xtask codegen` — the wire contract has one
  source of truth in `jarvis-contracts`.
- Maps: **MapLibre GL JS + PMTiles** region extract served by `jarvisd` (offline-capable,
  keyless — ADR-013); Leaflet + OSM raster tiles acceptable during bootstrap, attribution
  always visible.
- Chromium app-mode windows (one per surface/display) launched and placed by
  `jarvis-agent`. No desktop privilege inside the browser shell.
- Flutter remains an option for later mobile/satellite clients consuming the same
  contracts; not in v1.

## 4. AI providers

**v1 reality (ADR-011): Claude Code CLI is the only reasoning provider.** No API billing,
no local-model-capable hardware. The `ModelProvider` port is unchanged; additional
profiles slot in later without core changes.

| Profile | Transport | Status | Use |
|---|---|---|---|
| `claude-cli` | spawn `claude -p --output-format stream-json` in a controlled workdir | **v1 primary and only LLM** | General reasoning + sandboxed coding profile ([ADR-004](adr/README.md#adr-004)). Built-in file/shell tools disabled for reasoning; Jarvis tools are the only action path. |
| `deterministic` | in-process | **v1 fallback** | Not a model: rule-based intent grammar (HA sentence triggers), direct/slash commands, R0 tools, queue-and-wait for LLM-needing work. |
| `local-embeddings` | in-process (`fastembed`, ONNX) | **v1, CPU-only** | bge-small / MiniLM-class embeddings for memory & retrieval — runs in milliseconds on any CPU; no GPU needed. |
| `anthropic-api` | HTTPS | future | Drop-in when API billing exists; same port. |
| `ollama` / `llama-cpp` | HTTP localhost | future | When capable hardware exists. |

Quota discipline for the CLI profile:

- **Single-flight**: at most one CLI reasoning process at a time; runs queue FIFO with
  priority for interactive over background/automation work.
- Adapter parses rate-limit/quota/auth signals from CLI output and exit codes; on
  exhaustion, mark profile unhealthy with backoff and surface the reset window in the UI.
- Per-run `RunBudget` caps model turns; automations that need reasoning are deferrable and
  skip (with notification policy) rather than draining quota unattended.
- Deferrable work (summarization, memory-candidate extraction, titles beyond a
  deterministic default) batches into healthy-quota windows instead of running inline.
- Route everything routable away from the LLM: entity resolution, retrieval filters,
  timestamps, HA command grammar — deterministic code first, model second.

## 5. Voice, home, browser (out-of-process, unchanged from v1 analysis)

Wyoming protocol services: Silero VAD, faster-whisper or whisper.cpp STT, Piper TTS,
openWakeWord later (license-check pretrained models separately). Home Assistant via REST +
WebSocket with a dedicated token. Playwright browser worker as an out-of-process tool
server (Node) speaking MCP — acceptable because it is Z3-isolated and replaceable.

## 6. CI/CD — GitHub Actions

The project lives on GitHub, so CI is GitHub Actions (`.github/workflows/ci.yml`).
(Earlier drafts named Azure DevOps; corrected 2026-07-19 — the platform changed, the
stage/gate contract below did not.) Pipeline stages and gates:

| Stage | Gate |
|---|---|
| Validate | `cargo fmt --check`, `clippy -D warnings`, `npm run lint`, schema compatibility check (generated TS types are committed and must match). |
| Test | `cargo test --workspace`, `cargo xtask arch-test`, policy/transition tables, provider parser tests, Angular tests. |
| Build | Pinned toolchains, reproducible container images, SBOM (`cargo auditable` + syft). |
| Security | `cargo deny`, `cargo audit`, container scan, secret scan, license inventory. |
| Integration | Compose env: Postgres, fake model streams, fake MCP servers; golden traces (`cargo xtask golden`). |
| Performance | Reference trace scenarios with p50/p95 + RSS regression thresholds. |
| Publish | Immutable tags, digest-pinned deployment manifest. |
| Deploy | DB backup → migrations → health gate → rollback command (manual stage to personal host). |
