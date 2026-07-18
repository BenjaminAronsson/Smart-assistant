# 07 — Testing strategy and acceptance gates

Under assumption A-08 (agent-implemented, human-reviewed), tests are not a phase — they
are the specification's executable form. Policy, grants, and state transitions get tests
**before** adapter code exists.

## 1. Test pyramid

| Level | Examples | Harness |
|---|---|---|
| Unit | State transitions (full transition table), policy rules, context budgets, memory retention, artifact manifests | `cargo test`, pure crates, no I/O |
| Contract | Provider event parsers (Claude stream-json, Ollama), MCP schemas, WS envelope, HA responses, DB migrations | fixture files in `tests/fixtures`, sqlx test DB |
| Component | Host module with real Postgres + fake adapters | testcontainers or compose test profile |
| Integration | Claude CLI smoke (when authenticated), Ollama test model, real MCP child process, Playwright worker | `cargo xtask golden` env |
| End-to-end | Text request → UI → approval → tool side effect → artifact render → restart recovery | Playwright against the Angular shell |
| Adversarial | Prompt injection, malicious tool output, stale approval, path traversal, zip bomb, oversized events | security suite, CI gate |
| Performance | Voice turn, first token, local model warm/cold, tool concurrency, display fan-out, `jarvisd` RSS | reference machine, p50/p95 + RSS thresholds |
| Chaos/recovery | Kill model/tool process, drop network, restart `jarvisd`, DB timeout, full-disk simulation | scripted in `xtask` |

Architecture tests (`cargo xtask arch-test`) assert the dependency rule on every PR.

## 2. Golden trace scenarios

Executable end-to-end scenarios with fake providers where possible; the first five exist
before scope expands (roadmap §M1):

1. Simple question answered by Claude CLI within budget; deterministic paths (title,
   entity resolution) verified to make zero extra model calls.
2. Complex question routed to Claude CLI, streamed to two displays.
3. Claude CLI quota-exhausted → run queues in visible degraded mode with reset-window
   shown; R0 tools and rule-based home command still work; queued run completes when the
   profile recovers.
4. Home light toggle auto-authorized as R1; exact state transition recorded.
5. External message proposal classified R2; user edits → old approval invalidated → new
   approval succeeds.
6. Malicious webpage asks the assistant to reveal secrets; policy denies; injection
   evidence recorded.
7. Coding task creates a patch artifact in a disposable worktree; no direct deployment.
8. Generated app requests an undeclared capability; bridge rejects.
9. Voice response interrupted; TTS, model, and tool cancellation all correct.
10. Host restarts during tool execution; run reconciles via idempotency, no duplicate
    mutation.

## 3. Definition of done (per feature)

- Domain contract + threat/risk classification exist before adapter code.
- Happy path, timeout, cancellation, malformed-response, and permission-denied tests exist.
- Telemetry spans and user-visible failure states exist.
- Configuration has validation, a safe default, and a documented example.
- No secrets or provider-specific types cross module boundaries (arch test green).
- A manual acceptance scenario is documented and repeatable.
- `clippy -D warnings`, `fmt`, `deny`, and the golden suite pass in CI.
