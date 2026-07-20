# 08 — Delivery roadmap

Ordered by **risk retirement, not spectacle**. Each milestone ships a usable vertical
slice and preserves the ability to stop without a half-built distributed platform.

## 1. Milestones

| Milestone | Scope | Exit evidence |
|---|---|---|
| **M0 Foundation** ✅ (signed off 2026-07-19, tag `m0-complete`) | Cargo workspace + Angular workspace, Postgres/pgvector + migrations, `jarvis-contracts` + codegen, OTel wiring, auth placeholder, compose env, CI pipeline. | `jarvisd` starts; health page works; one persisted session round-trips; CI green end to end. **All demonstrated** — see `docs/milestones/M0-gate-report.md`. |
| **M1 Text vertical slice** ✅ (signed off 2026-07-20, approve-with-fixes; PR #4 merged, follow-up fixes PR #6, tag `m1-complete`) | Message/run state machine, WS streaming UI, Claude CLI adapter (single-flight queue, quota/health detection), deterministic fallback mode, cancellation, tracing. | Question streams; quota/auth/network failure switches to a visible degraded mode and queues the run; run survives restart; golden traces 1–3 pass. **All demonstrated** — see `docs/milestones/M1-gate-report.md`. |
| **M2 Safe actions** | Tool registry, native tools, MCP host (rmcp), policy/risk engine, approval UI + grants, idempotency, audit, `web.search`/`web.fetch` tool (FR-25). | Read a project file (R0); perform one reversible action (R1); block an unapproved mutation; answer a current-facts question via search rather than stale memory, image shows its source link; a location-dependent query ("lunch nearby") resolves via the location provider (FR-26); a genuinely ambiguous query ("microcondia") gets one fluent clarifying question, not a picker; a contested-news query ("latest on Iran") is summarized with attributed, even-handed framing (FR-30); adversarial basics incl. malicious-fetched-page injection; golden 4–6. |
| **M3 Artifacts & desktop** | Artifact CAS + renderers, display profiles, `jarvis-agent` + Hyprland IPC, isolated Playwright worker, MPRIS adapter + media window + media bar (FR-22), HUD face per docs/12 (card grammar incl. headlines/digest, panel lifecycle FR-24, backgrounds FR-23, MapLibre/PMTiles map card + out-of-region fallback), deep-dive support (thread continuity, sources/gallery cards, browser-worker source handoff, Research Notes artifact promotion — FR-27), timers/alarms/reminders (FR-33) and lists/notes (FR-34) with their cards. | Create/reopen artifact after restart; place canvas on selected monitor; audited browser flow; pause whatever is playing from the media bar; golden 7. |
| **M4 Memory & quota-smart intelligence** | CPU embeddings (`fastembed`), bounded memory + retrieval, deterministic HA intent grammar incl. math/unit conversions, deferrable-work scheduler for quota windows, evaluation harness, provider health scoring, CalDAV adapter (FR-35), SMTP send adapter (FR-36). Ollama adapter deferred until capable hardware exists. | Offline search/retrieval and rule-based home commands work with zero LLM calls; "15% of 230" answers with zero LLM calls; "what's on today" renders an agenda card; the landlord message sends end-to-end through approval → SMTP; deferred summarization runs in a healthy-quota window; memory forget verified. |
| **M5 Voice, home & media** | Push-to-talk, VAD/STT/TTS via Wyoming, barge-in, HA state/actions with allowlist, Spotify adapter + cast-a-link (FR-21/22) with voice transport commands routed through the deterministic grammar. | Full voice round trip within NFR-04; safely control one allowlisted HA entity; "pause the music" works with zero LLM calls; play a searched Spotify track on a chosen device; "play ABBA" starts shuffled top tracks with no unnecessary clarification, "play playlist X" resolves the user's own library first, "what's playing" answers with a now-playing card (FR-32); a plural area command ("turn on the living room lamps") resolves to multiple entities and reports partial failure honestly (FR-28); golden 9. |
| **M6 Generated apps** | Template/spec format, sandbox builder, manifests, CSP, capability bridge. | Dashboard app generated; cannot access undeclared capabilities; golden 8. |
| **M7 Distributed rooms** | Device keys/pairing, remote display/voice nodes, mTLS/private network, resync. | Second node pairs, receives a surface, performs voice/display flow; revocation works. |
| **M8 Product hardening** | Installer/update, backup/restore + restore test, policy UI, accessibility, diagnostics bundle, signed releases. | Repeatable install/upgrade/rollback; full security release checklist passes; golden 10. |

## 2. First implementation slice — exact build order

1. Create the Cargo workspace + Angular workspace with contract codegen (`xtask codegen`).
2. Stand up Postgres/pgvector; implement sessions, messages, runs, append-only audit
   (schemas from `04`).
3. Implement the WS event envelope + minimal conversation/timeline UI.
4. Implement the deterministic run state machine against a **fake streaming provider**;
   write the full transition-table test first.
5. Add the Claude CLI adapter (no-tools reasoning profile): spawn, stream-json parsing
   from fixtures, cancellation, **single-flight queue**, health detection (missing auth,
   quota/rate-limit signals, non-zero exit, malformed events, idle timeout → mark
   unhealthy with backoff and surface the reset window).
6. Implement deterministic degraded mode: run queueing with visible waiting state,
   direct/slash commands, R0 tools without a model.
7. Add one native read-only tool and the R0 policy path.
8. Add one R2 fake/external tool and the full approval → grant → execute flow.
9. Add artifact manifest/CAS and a Markdown renderer.
10. Capture latency traces and lock in golden scenarios 1–5 **before expanding scope**.

## 3. Working discipline

- ADRs for every irreversible choice; small vertical PRs; contract fixtures over mocks
  where a real wire format exists.
- No "framework exploration" branches that don't end in a measured decision.
- External projects stay behind adapter ports so experiments are deletable.
- Automate setup, migrations, fake services, and golden traces at M0 — integration drift
  is the number-one solo-project killer.
- Delay irreversible decisions; implement safety and observability decisions immediately.

## 5. Handover checklist (pre-M0 gate — all satisfied at baseline v2.2)

- [x] Every FR/NFR maps to a design section and milestone (`01` §6 traceability).
- [x] Sole-provider + quota strategy specified (ADR-011, `03` §4, config in `09` §1).
- [x] Complete API surface incl. session search, memory review, automations (`05` §1).
- [x] v1 authentication bootstrap specified (`05` §6).
- [x] Error-code registry seeded (`05` §7).
- [x] Configuration reference + deployment units + backup/restore (`09`).
- [x] Golden traces 1–10 defined and mapped to milestones (`07` §2).
- [x] Security release gates CI-enforceable (`06` §8).
- [x] External references carried over and updated (`10`).

## 6. Decisions deliberately deferred to implementation

These are *not* gaps; deciding them now would be speculation. Each has a decision point
and a default:

| Decision | Decide at | Default until then |
|---|---|---|
| Angular state management (signals vs NgRx) | M1, after first streaming UI | Angular signals + services |
| Exact STT model size (base vs small int8) | M5 on reference hardware | faster-whisper `base` int8 |
| Wake-word engine + model licensing | post-M5 | openWakeWord, pending asset-license review |
| Generated-app template format details | M6 | JSON spec + locked Vite template |
| NATS JetStream vs alternatives | M7, if second machine materializes | in-process outbox only |
| Anthropic API vs local model as second provider | when billing/hardware changes | claude-cli only (ADR-011) |
| WebRTC/LiveKit for remote media | M7 | WS binary PCM frames |
| YouTube search via Data API vs browser worker | M5, if a key is provisioned | browser worker |
| PMTiles region-extract tooling (self-built vs downloaded extract) | M3 | downloaded regional extract |
| Dark theme (token-set derivation of the light glass system) | post-M8 polish | light only in v1 (12 §5) |

## 7. Risk register

| Risk | Impact | Likelihood | Mitigation |
|---|---|---|---|
| Claude subscription/CLI changes or quota | High | Medium | Adapter isolation, health detection, Ollama fallback, API adapter later. |
| Local model quality insufficient | Medium | High | Task-specific evals (M4 harness), hybrid routing, never promise cloud-equivalent quality. |
| Prompt/tool injection causes side effect | High | Medium | Default deny, exact grants, isolation, adversarial suite, no ambient shell. |
| Rust learning curve slows review | Medium | Medium | Review surface concentrated in two small pure crates; transition-table tests as readable spec; Claude Code explains diffs on request. |
| Voice latency/acoustics disappoint | Medium | High | PTT first, reference hardware, Wyoming services, delay wake word. |
| Scope creeps into OS/home replacement | High | High | Non-goals, milestone gates, HA ownership, adapter boundaries. |
| OSS license conflicts | Medium | Medium | `cargo deny` + SBOM, process boundaries, per-asset voice-model inventory. |
| Generated apps become attack surface | High | Medium | Separate origin, CSP, build sandbox, capability bridge, dependency allowlist. |
| Distributed architecture too early | Medium | High | Modular monolith; no broker until a second machine proves the need. |
| Memory stores incorrect/private facts | High | Medium | Provenance, confidence, explicit review, retention, working forget flow. |
| Person-recognition scope creep | High | Medium | The HUD "who is this" demo is illustrative only — no FR, no camera input design, no consent/privacy model exists for recognizing other people. Do not implement without a dedicated FR and a privacy ADR (biometrics of third parties is the most sensitive data class in the system). |
| Solo maintenance burden | High | High | Reuse edge services, one language in core, CI automation, pinned versions. |
