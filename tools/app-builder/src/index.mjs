#!/usr/bin/env node
// Jarvis app builder (F6.2, FR-18, docs/06 §6, ADR-027, ADR-029).
//
// An out-of-process worker that renders a HOST-VALIDATED app spec against a
// LOCKED Vite template and returns ONE SELF-CONTAINED DOCUMENT. It is UNTRUSTED
// and DUMB by design: the Rust host (`jarvis-adapters::app_builder`) owns the
// ToolPolicy, the caps, the build provenance and the artifact write.
//
// What this worker deliberately does NOT do:
//   * decide what may be built — the spec arrived already validated against
//     closed template and capability vocabularies (F6.1); this worker re-checks
//     the template id only because a worker that trusts its input is a worker
//     that can be walked out of its own directory;
//   * report its own isolation — build provenance (worker image, lockfile hash,
//     network posture) is host/ops-attested on the Rust side. There is no field
//     in the reply to claim it in (docs/06 §5/§6);
//   * install anything. The template's dependencies come from the worker image
//     (production) or a documented `npm ci` (dev/CI). A build never resolves a
//     dependency, so a build never needs the network.
//
// Protocol (line-delimited JSON over stdio, one exchange per line):
//   host → worker: {"build_id":<u64>,"template":"dashboard/v1","title":<string>,
//                   "capabilities":[<string>],"bindings":[{name,capability,target}],
//                   "max_bundle_bytes":<u64>,"max_build_seconds":<u32>}
//   worker → host: {"ok":<bool>,"bundle":<string?>,"summary":<string?>,"error":<string?>}
// The host reads only those fields; anything else it drops (invariant 1).
//
// Config via environment (host-set, never argv):
//   JARVIS_APP_TEMPLATE_ROOT  directory holding the locked templates.
//                             Defaults to `<this worker>/templates`.

import { execFile } from "node:child_process";
import { cp, mkdtemp, readFile, rm, stat, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import readline from "node:readline";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const TEMPLATE_ROOT = resolve(
  process.env.JARVIS_APP_TEMPLATE_ROOT || join(HERE, "..", "templates"),
);

// The closed template vocabulary, mirroring `jarvis_domain::appspec::TemplateId`.
// The host has already rejected anything not in here; this map exists so a
// template id can never be used as a path component (a directory traversal is
// the one thing an id-shaped string could otherwise buy).
const TEMPLATES = new Map([["dashboard/v1", "dashboard-v1"]]);

function reply(fields) {
  process.stdout.write(JSON.stringify(fields) + "\n");
}

// Short, generic reason only. The build child inherits this process's
// environment, so its stderr may carry a credential — it is never forwarded
// (invariant 5).
function fail(reason) {
  reply({ ok: false, bundle: null, summary: null, error: String(reason).slice(0, 200) });
}

function isPlainObject(v) {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** Everything the template is allowed to see about the request. */
function specFor(req) {
  return {
    title: String(req.title ?? ""),
    capabilities: (Array.isArray(req.capabilities) ? req.capabilities : []).map(String),
    bindings: (Array.isArray(req.bindings) ? req.bindings : [])
      .filter(isPlainObject)
      .map((b) => ({
        name: String(b.name ?? ""),
        capability: String(b.capability ?? ""),
        target: String(b.target ?? ""),
      })),
  };
}

async function build(req) {
  const dirName = TEMPLATES.get(req.template);
  if (!dirName) return fail("unknown template");

  const source = join(TEMPLATE_ROOT, dirName);
  const modules = join(source, "node_modules");
  try {
    await stat(join(modules, "vite"));
  } catch {
    // A missing install is a configuration failure, not a build failure: say so
    // plainly rather than letting vite's own ENOENT surface as "build failed".
    return fail("template dependencies are not installed (npm ci)");
  }

  const work = await mkdtemp(join(tmpdir(), "jarvis-app-build-"));
  try {
    // A private copy per build: the locked template tree is never written to,
    // so two builds cannot interfere and a build cannot mutate the template it
    // claims to have been built from.
    await cp(source, work, {
      recursive: true,
      filter: (path) => !path.startsWith(modules),
    });
    // Dependencies are read-only and huge — link, never copy.
    await symlink(modules, join(work, "node_modules"), "dir");

    // The spec reaches the template as DATA, imported as JSON. It is never
    // interpolated into source, so there is no splice and no eval (threat
    // note #8).
    await writeFile(join(work, "src", "spec.json"), JSON.stringify(specFor(req), null, 2));

    const seconds = Number(req.max_build_seconds) || 120;
    await new Promise((ok, no) => {
      const child = execFile(
        process.execPath,
        [join(work, "node_modules", "vite", "bin", "vite.js"), "build", "--logLevel", "warn"],
        {
          cwd: work,
          timeout: seconds * 1000,
          killSignal: "SIGKILL",
          maxBuffer: 8 * 1024 * 1024,
          env: {
            ...process.env,
            // Belt to the container's braces: nothing here should resolve a
            // dependency, and if something tries, it fails fast instead of
            // reaching out (docs/06 §6 "network disabled").
            npm_config_offline: "true",
            npm_config_audit: "false",
            npm_config_fund: "false",
            NO_UPDATE_NOTIFIER: "1",
          },
        },
        (e) => (e ? no(e) : ok()),
      );
      child.on("error", no);
    });

    const document = await readFile(join(work, "dist", "index.html"), "utf8");
    const max = Number(req.max_bundle_bytes) || 0;
    // Refused whole, never truncated — the host enforces the same bound
    // independently, and neither side trusts the other.
    if (max > 0 && Buffer.byteLength(document, "utf8") > max) {
      return fail("built bundle exceeds the size limit");
    }
    reply({
      ok: true,
      bundle: document,
      summary: `built ${req.template}`,
      error: null,
    });
  } catch (e) {
    if (e?.killed || e?.signal === "SIGKILL") return fail("build timed out");
    return fail("build failed");
  } finally {
    await rm(work, { recursive: true, force: true }).catch(() => {});
  }
}

async function handle(line) {
  let req;
  try {
    req = JSON.parse(line);
  } catch {
    return fail("malformed request");
  }
  if (!isPlainObject(req) || typeof req.template !== "string") {
    return fail("malformed task");
  }
  return build(req);
}

async function main() {
  const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  for await (const line of rl) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    await handle(trimmed);
  }
}

main().catch((e) => {
  fail(e?.message || e);
  process.exit(1);
});
