#!/usr/bin/env node
// Jarvis coding worker (F3a.6, docs/02 §8, docs/06 §5, golden 7, ADR-004/ADR-027).
//
// An out-of-process worker that runs a delegated coding task in a DISPOSABLE git
// worktree and returns a reviewable **patch** (unified diff). It is UNTRUSTED and
// DUMB by design: the Rust host (`jarvis-adapters::coding`) owns the ToolPolicy and
// turns the patch into an immutable artifact.
//
// PATCH-ONLY (owner decision; golden 7 "no direct deployment"): this worker NEVER
// commits to, pushes, or otherwise mutates the source repository. It adds a
// throwaway worktree, runs the coding step there, captures `git diff`, and removes
// the worktree. Applying the patch is a separate approved action, out of scope.
//
// Protocol (line-delimited JSON over stdio, one exchange per line):
//   host → worker:  {"task_id": <u64>, "instruction": <string>, "repo_path": <string>}
//   worker → host:  {"ok": <bool>, "patch": <string?>, "summary": <string?>,
//                    "worker_image": <string?>, "error": <string?>}
// The host reads only those fields; anything else it drops (invariant #1).
//
// Isolation (ADR-027): in production this runs inside a per-task container with a
// read-only mount of the repo and no outbound network unless the task needs it;
// in dev/CI it runs as a plain process. CI uses a FAKE worker — the host's Rust
// tests exercise the protocol and every property without git or a model.
//
// Config via environment (host-set, never argv):
//   JARVIS_CODING_CMD   the coding step to run inside the worktree. Receives the
//                       instruction as $JARVIS_CODING_INSTRUCTION. Default follows
//                       ADR-004: `claude -p "$JARVIS_CODING_INSTRUCTION"` with
//                       built-in tools disabled — configured by ops.
//   JARVIS_CODING_WORKER_IMAGE  optional builder image ref recorded as provenance.

import { execFile } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import readline from "node:readline";
import { promisify } from "node:util";

const run = promisify(execFile);
const CODING_CMD =
  process.env.JARVIS_CODING_CMD || 'claude -p "$JARVIS_CODING_INSTRUCTION"';

function reply(fields) {
  process.stdout.write(JSON.stringify(fields) + "\n");
}

// Report a short, generic reason. We deliberately do NOT forward the child's raw
// stderr: the coding step inherits the full host env (including injected model
// credentials), so its stderr could carry a secret (invariant #5). Build
// provenance (image, network posture) is host/ops-attested on the Rust side, not
// self-reported here (docs/06 §5/§6).
function fail(reason) {
  reply({
    ok: false,
    patch: null,
    summary: null,
    error: String(reason).slice(0, 200),
  });
}

// Run one task in a disposable worktree; return the diff. The source repo's own
// working tree is never touched.
async function handle(req) {
  if (!req || typeof req.repo_path !== "string" || typeof req.instruction !== "string") {
    return fail("malformed task");
  }
  const worktree = await mkdtemp(join(tmpdir(), "jarvis-coding-"));
  try {
    // Detached worktree at the repo's current HEAD — a private sandbox copy.
    await run("git", ["-C", req.repo_path, "worktree", "add", "--detach", worktree]);

    // The coding step edits files inside the worktree only. Instruction is passed
    // via the environment, never interpolated into the shell command string.
    await run("sh", ["-c", CODING_CMD], {
      cwd: worktree,
      env: { ...process.env, JARVIS_CODING_INSTRUCTION: req.instruction },
      maxBuffer: 16 * 1024 * 1024,
    });

    // Capture everything the step changed as a unified diff (staged + unstaged),
    // relative to HEAD. We stage to include new files, then diff --cached.
    await run("git", ["-C", worktree, "add", "-A"]);
    const { stdout: patch } = await run(
      "git",
      ["-C", worktree, "diff", "--cached", "--no-color"],
      { maxBuffer: 16 * 1024 * 1024 },
    );

    reply({
      ok: true,
      patch,
      summary: patch.trim() ? "patch produced" : "no changes",
      error: null,
    });
  } catch (e) {
    // Generic reason only — never the child's stderr (may carry a credential).
    fail(e?.code === "ENOENT" ? "coding step not found" : "coding step failed");
  } finally {
    // Always tear the worktree down — it is disposable (golden 7).
    await run("git", ["-C", req.repo_path, "worktree", "remove", "--force", worktree]).catch(
      () => {},
    );
    await rm(worktree, { recursive: true, force: true }).catch(() => {});
  }
}

async function main() {
  const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  for await (const line of rl) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    let req;
    try {
      req = JSON.parse(trimmed);
    } catch {
      fail("malformed request");
      continue;
    }
    await handle(req);
  }
}

main().catch((e) => {
  fail(e?.message || e);
  process.exit(1);
});
