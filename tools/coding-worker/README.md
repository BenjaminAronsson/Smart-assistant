# jarvis-coding-worker

Out-of-process **coding worker** (F3a.6, docs/02 §8, docs/06 §5, golden 7, ADR-004/ADR-027).

The Rust host `jarvis-adapters::coding` launches this worker, sends it one coding task, and
turns the returned diff into an immutable **patch artifact** (F3a.1 `CodeText`). This worker
is untrusted (Z4) and dumb: it declares no tools and owns no policy — the host does.

## PATCH-ONLY (golden 7)

This worker runs the coding step in a **disposable git worktree** and captures `git diff`.
It **never** commits to, pushes, or otherwise mutates the source repository, and there is no
apply/deploy path — in the worker or in the host. Applying a patch is a separate approved
action, deferred to a later milestone (owner decision).

## Protocol

One task line in, one response line out.

```
host → worker:  {"task_id": 7, "instruction": "add a null check", "repo_path": "/repo"}
worker → host:  {"ok": true, "patch": "--- a/x\n+++ b/x\n@@ ...", "summary": "patch produced", "worker_image": null, "error": null}
```

## Isolation (ADR-027)

- **Production:** run inside a per-task container — repo mounted read-only, outbound network
  off unless the task needs it, CPU/mem/time limits (docs/02 §12). Container = ops config.
- **Dev/CI:** plain process. CI uses a **fake** worker (no git/model); the host's Rust tests
  exercise the protocol and every property. Real runs are manual-verify.

## Config (environment, host-set — never argv)

| Var | Meaning |
|-----|---------|
| `JARVIS_CODING_CMD` | The coding step run inside the worktree. Gets the instruction as `$JARVIS_CODING_INSTRUCTION` (passed via env, never interpolated into the command string). Default per ADR-004: `claude -p "$JARVIS_CODING_INSTRUCTION"` with built-in tools disabled. |

Build provenance (worker image, network posture) is **host/ops-attested** on the Rust side
(`CodingWorkerHost`), not self-reported by this untrusted worker (docs/06 §5/§6).

Credentials (e.g. the model API/subscription) arrive as host-injected environment variables,
never argv, never logged (invariant #5).

## Run

```bash
JARVIS_CODING_CMD='git apply /dev/stdin <<EOF ... EOF' node src/index.mjs   # example
# then feed a task JSON line on stdin
```

Not built or tested in CI.
