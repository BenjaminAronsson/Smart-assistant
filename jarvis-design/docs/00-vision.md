# 00 — Vision, product definition, principles

## 1. The problem

Current assistants are fragmented. A coding agent can change a repository but has no room
context. A smart-home assistant controls devices but reasons weakly. A chat UI calls
models but cannot safely operate the desktop. A local model preserves privacy but may not
match a frontier model on complex planning.

The opportunity is a single personal control plane that joins these capabilities **without
handing total authority to the model**. The system must solve *coordination*, not merely
inference. The hard problems are identity, state, permissions, interruption, failure
recovery, provenance, tool contracts, user trust, and consistent rendering across devices.
Model quality matters; model **replaceability** matters more over the product's life.

## 2. Product definition

> **Jarvis is** a personal, local-first operating layer that accepts text or voice,
> understands the active context, plans bounded work, requests approval for consequential
> actions, executes typed tools, presents live results on one or more displays, and
> remembers only what policy permits.

Single-PC v1 for a solo developer, designed to evolve into a whole-house distributed
assistant without rewriting the core.

## 3. Product outcomes

- **Reduce interaction cost.** One conversational surface for code, files, browser tasks,
  information, home control, and local workflows.
- **Preserve user agency.** Every consequential action is inspectable, cancellable, and
  attributable.
- **Remain useful offline.** UI, sessions, smart-home operations, basic voice, and a local
  model continue without cloud access.
- **Avoid provider lock-in.** Model-specific behavior stays inside adapters and capability
  profiles.
- **Grow by adding nodes, not rewriting.** The same contracts support a second display, a
  kitchen satellite, and a mobile client.
- **Create durable outputs.** Answers can become versioned artifacts, dashboards, scripts,
  or small sandboxed applications instead of vanishing into chat history.

## 4. Design principles

| Principle | Meaning |
|---|---|
| **Code owns authority** | The model suggests; deterministic code authorizes and executes. |
| **Local-first, not local-only** | Private state and basic capability remain local; cloud models are optional accelerators. |
| **Protocol reuse over framework adoption** | Adopt MCP, Wyoming, Home Assistant APIs, and OpenTelemetry without adopting anyone's product architecture. |
| **Reversible by default** | Prefer actions with undo, dry-run, and preview; escalate confirmation as reversibility decreases. |
| **Observable by construction** | Every model run, prompt assembly, policy decision, tool call, and artifact version has a trace. |
| **Progressive enhancement** | Text before voice, push-to-talk before wake word, one PC before distributed rooms. |
| **Untrusted content stays untrusted** | Web pages, documents, model output, and generated apps never inherit system authority. |
| **The compiler is a reviewer** | Because an AI agent writes most code, prefer designs the type system can enforce: exhaustive enums, newtypes, ownership over shared mutability. |

The last principle is new in v2 and motivates the Rust decision ([ADR-001](adr/README.md#adr-001)).

## 5. Explicit non-goals for v1

- A fully autonomous employee that runs indefinitely without supervision.
- A replacement for Home Assistant, an IDE, a browser, or a compositor.
- Commercial multi-tenant SaaS, public internet exposure, or anonymous users.
- Always-listening whole-house audio from day one.
- A third-party plugin marketplace before signing, isolation, and permission review exist.
- Training a foundation model.

## 6. What not to do (anti-patterns from the market scan)

The v1 market analysis (OpenClaw, OpenJarvis, Open Interpreter, Open WebUI, AnythingLLM,
Home Assistant, Wyoming, OpenVoiceOS, Leon, LiveKit/Pipecat) remains valid; its conclusions
are folded into the ADRs. The distilled prohibitions:

- **Never** give an LLM an unrestricted shell on the host. Execution is isolated, typed,
  approved.
- **Never** make every integration an in-process plugin; a faulty plugin must not inherit
  core-process privileges.
- **Never** build wake word, STT, TTS, device protocols, or WebRTC from scratch.
- **Never** introduce Kubernetes, a distributed bus, or microservices on one PC.
- **Never** store "memory" as an unbounded transcript dump; memory requires provenance,
  retention, confidence, and user controls.
- **Never** let generated HTML/JS share an origin with privileged control surfaces or
  receive direct host-tool access.

## 7. Success criterion

The project is judged by whether it performs a small number of everyday tasks **safely,
quickly, and transparently** — not by how autonomous it appears in a demo. A trustworthy
assistant that asks at the right moment, shows what it is doing, and survives provider
changes beats an impressive unrestricted agent.
