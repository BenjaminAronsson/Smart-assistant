# 04 — Data, storage and schemas

## 1. Storage decision (ADR-008)

**PostgreSQL 16 + pgvector** for transactional state, JSON payloads, audit relationships,
and embeddings. **Content-addressed file store** (SHA-256-keyed directory tree under
`/var/lib/jarvis/artifacts`) for blobs and generated bundles — integrity, dedup, and
provenance without blobs in the database. SQLite is acceptable only for throwaway spikes.

## 2. Core entities

| Entity | Key fields / responsibility |
|---|---|
| User | Owner identity, policy profile, encryption references. |
| Device | Paired client/node identity, public key, scopes, last seen, capabilities. |
| Session | Conversation metadata, branch, summary, status, active context. |
| Message | Immutable role/content blocks, provenance, sensitivity, provider-bound status. |
| Run | State-machine status, selected profile, budgets, timestamps, terminal outcome. |
| ModelInvocation | Provider/profile, request hash, usage, latency, error, trace linkage. |
| PlanStep | Structured action proposal, dependencies, status, observation. |
| ToolCall | Tool/version, normalized-arguments hash, grant, result, idempotency key. |
| Approval | Risk, exact effect, requesting device, decision, expiry, grant reference. |
| Memory | Layer, text, source, confidence, sensitivity, retention, embedding pointer. |
| Artifact / ArtifactVersion | Manifest, hash, renderer, source run, build provenance, storage pointer. |
| Automation | Trigger, execution identity, policy, budget, state, next run. |
| AuditEvent | Append-only event with actor, target, correlation, hash chain. |

All IDs are ULIDs exposed as opaque strings; database sequences never leak (contract
convention, `05` §5).

## 3. Schema boundaries

One database, schema-per-module; cross-module access goes through application ports, not
foreign tables. Module-owned tables:

| Schema | Tables |
|---|---|
| identity | users, devices, device_keys, grants |
| conversation | sessions, messages, session_summaries, context_references |
| orchestration | runs, plan_steps, checkpoints, cancellations |
| models | profiles, invocations, usage_samples, health_state |
| tools | tool_definitions, tool_calls, tool_results, approvals |
| memory | memories, memory_sources, embeddings, retention_jobs |
| artifacts | artifacts, artifact_versions, manifests, render_jobs |
| automation | automations, triggers, executions |
| audit | audit_events, security_alerts |
| outbox | outbox_events (transactional outbox, `02` §2) |

Migrations: `sqlx migrate`, one directory per module ordering under a single migration
stream; every migration reversible or explicitly marked destructive with a backup gate.

## 4. Artifact manifest (illustrative)

```json
{
  "artifactId": "01J8Z…",
  "version": 3,
  "mediaType": "application/vnd.jarvis.webapp+zip",
  "sha256": "…",
  "renderer": "sandboxed-webapp/v1",
  "createdByRunId": "01J8Z…",
  "sources": [{ "type": "message", "id": "01J8Z…" }],
  "sensitivity": "personal",
  "build": {
    "workerImage": "jarvis-web-builder@sha256:…",
    "lockfileHash": "…",
    "network": "disabled"
  },
  "capabilities": ["artifact.read-own-data"]
}
```

Manifests are immutable; a new version is a new row + new CAS entry. Deletion coordinates
manifest, CAS blob, and any derived embeddings so "forget" is real (FR-16).

## 5. Retention & privacy defaults

- Messages: immutable, retained until session archive policy deletes.
- Token deltas / partial transcripts: never persisted (presentation stream only).
- Memories: per-layer retention rules; semantic facts persist until user edit/forget.
- Audit: longest retention, append-only, hash-chained; separate from verbose logs.
- Provider-bound context: recorded per ModelInvocation (request hash + item provenance)
  so the user can audit exactly what left the machine (NFR-02).
